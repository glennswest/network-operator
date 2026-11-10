//! The VLAN controller (`run --controllers vlan`).
//!
//! Level-triggered over the whole set, not per object: every pass lists the
//! VLANs, namespaces, NADs, nodes and pods, [`super::plan`]s, and applies the
//! difference. That is what conflicts need (one ID per bridge, one default per
//! namespace are decisions about the set), and what makes deletion simple: a
//! VLAN that is gone just stops being planned, so its NADs are reaped and its
//! namespace defaults removed on the next pass, with no finalizer. The node
//! agents drop its bridge entries the same way.
//!
//! A pass runs when a VLAN or a namespace changes, and every [`RESYNC`]
//! otherwise (nodes' reports, pods and a deleted NAD are picked up then).

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use k8s_openapi::api::core::v1::{Namespace, Node, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, DynamicObject, ListParams, Patch, PatchParams};
use kube::runtime::watcher::{self, watcher};
use kube::runtime::WatchStreamExt;
use kube::{Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::crd::{Vlan, VlanSpec};
use super::{
    attached_nads, nad, nad_api, plan, status, ExistingNad, NodeState, World, DEFAULT_NETWORKS_ANNOTATION,
    NETWORK_STATUS_ANNOTATION, NODE_STATE_ANNOTATION,
};
use crate::metrics::{FailureReason, Metrics};
use crate::{apply, status as status_write};

/// Pass at least this often.
pub const RESYNC: Duration = Duration::from_secs(20);

/// A stream that yields once per change it sees.
pub type Changes = Pin<Box<dyn Stream<Item = ()> + Send>>;

/// Changes to every object of a kind (errors back off, then count as a change).
pub fn watch<K>(api: Api<K>) -> Changes
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug + Send + 'static,
{
    Box::pin(watcher(api, watcher::Config::default()).default_backoff().map(|_| ()))
}

pub fn changes(streams: Vec<Changes>) -> Changes {
    Box::pin(futures::stream::select_all(streams))
}

/// Wait for a change or `resync`, then let a burst of changes settle, so
/// creating 100 VLANs is a few passes, not 100.
pub async fn settle(triggers: &mut Changes, resync: Duration) {
    tokio::select! {
        next = triggers.next() => {
            if next.is_none() {
                // The watches ended; fall back to the timer.
                tokio::time::sleep(resync).await;
            }
        }
        _ = tokio::time::sleep(resync) => {}
    }
    while let Ok(Some(())) = tokio::time::timeout(Duration::from_millis(500), triggers.next()).await {}
}

/// Run the controller until the process is signalled.
pub async fn run(client: Client, metrics: Arc<Metrics>) -> anyhow::Result<()> {
    let vlans: Api<Vlan> = Api::all(client.clone());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let mut triggers = changes(vec![watch(vlans), watch(namespaces)]);
    info!("starting VLAN controller");
    metrics.set_ready();

    loop {
        let started = Instant::now();
        match pass(&client).await {
            Ok(()) => metrics.record_success(chrono::Utc::now().timestamp(), started.elapsed().as_micros() as u64),
            Err(e) => {
                warn!(error = %e, "VLAN pass failed");
                metrics.record_failure(FailureReason::Kube, started.elapsed().as_micros() as u64);
            }
        }
        settle(&mut triggers, RESYNC).await;
    }
}

async fn pass(client: &Client) -> Result<(), kube::Error> {
    let lp = ListParams::default();
    let vlans_api: Api<Vlan> = Api::all(client.clone());
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let nad_all: Api<DynamicObject> = Api::all_with(client.clone(), &nad_api());

    let vlans = vlans_api.list(&lp).await?.items;
    let namespaces = ns_api
        .list(&lp)
        .await?
        .items
        .into_iter()
        .filter(|n| n.metadata.deletion_timestamp.is_none())
        .map(|n| {
            let current = n.annotations().get(DEFAULT_NETWORKS_ANNOTATION).cloned();
            (n.name_any(), current)
        })
        .collect();
    // No NAD API means no Multus support on this cluster: say so on every
    // VLAN rather than fail the pass, so status still reports the bridges.
    let (nads, nad_api_error) = match nad_all.list(&lp).await {
        Ok(l) => (l.items.iter().map(ExistingNad::from_object).collect(), None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let nodes = Api::<Node>::all(client.clone())
        .list(&lp)
        .await?
        .items
        .into_iter()
        .map(|n| {
            let state = n
                .annotations()
                .get(NODE_STATE_ANNOTATION)
                .and_then(|a| serde_json::from_str::<NodeState>(a).ok());
            (n.name_any(), state)
        })
        .collect();
    let pods = Api::<Pod>::all(client.clone())
        .list(&lp)
        .await?
        .items
        .into_iter()
        .filter_map(|p| {
            let ns = p.namespace().unwrap_or_default();
            let attached = attached_nads(&ns, p.annotations().get(NETWORK_STATUS_ANNOTATION)?);
            Some((format!("{ns}/{}", p.name_any()), attached))
        })
        .collect();

    let world = World { vlans: vlans.clone(), namespaces, nads, nodes, pods };
    let mut plan = plan(&world);

    if let Some(e) = nad_api_error {
        let why = format!("the NetworkAttachmentDefinition API is not available ({e})");
        plan.apply.clear();
        plan.delete.clear();
        for r in plan.reports.values_mut() {
            r.nads.clear();
            r.problems.push(why.clone());
        }
    }

    for r in &plan.apply {
        // The NAD is named after its VLAN.
        let vlan = r.name().to_string();
        match apply::apply_one(client, r).await {
            Ok(()) => info!(nad = %r.id(), "applied"),
            Err(e) => {
                warn!(nad = %r.id(), error = %e, "applying NAD failed");
                if let Some(rep) = plan.reports.get_mut(&vlan) {
                    rep.waiting.push(format!("applying {}: {e}", r.id()));
                }
            }
        }
    }
    for (ns, name) in &plan.delete {
        let r = nad(name, ns, &VlanSpec::default());
        match apply::delete_one(client, &r).await {
            Ok(()) => info!(nad = %r.id(), "removed: no VLAN allows it any more"),
            Err(e) => warn!(nad = %r.id(), error = %e, "removing NAD failed"),
        }
    }
    for (ns, value) in &plan.annotate {
        let v = value.as_ref().map(|s| Value::String(s.clone())).unwrap_or(Value::Null);
        let patch = json!({"metadata": {"annotations": {DEFAULT_NETWORKS_ANNOTATION: v}}});
        match ns_api.patch(ns, &PatchParams::default(), &Patch::Merge(&patch)).await {
            Ok(_) => info!(namespace = %ns, default = ?value, "namespace default VLAN set"),
            Err(e) => warn!(namespace = %ns, error = %e, "setting the namespace default failed"),
        }
    }

    let now = Time(chrono::Utc::now());
    let reports: BTreeMap<_, _> = plan.reports;
    for v in &vlans {
        let name = v.name_any();
        let Some(report) = reports.get(&name) else { continue };
        let previous = v.status.clone().unwrap_or_default();
        let next = status(&previous, report, v.metadata.generation.unwrap_or(0), &now);
        if v.status.as_ref() != Some(&next) {
            if let Err(e) = status_write::write_vlan(client, &name, &next).await {
                warn!(vlan = %name, error = %e, "writing status failed");
            }
        }
    }
    Ok(())
}
