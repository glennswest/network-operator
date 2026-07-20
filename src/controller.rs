//! The reconcile loop.
//!
//! One pass:
//!
//! 1. resolve the spec into an [`EffectiveConfig`] (mode defaults + validation)
//! 2. refuse immutable changes against `status.applied*`
//! 3. render and server-side apply
//! 4. reap objects we used to render but no longer do
//! 5. observe health and write the conditions back
//!
//! Drift-heal comes from watching the objects we own: any modification or
//! deletion of the DaemonSet, Deployment or ConfigMap maps back to the owning
//! `Network` and re-triggers the pass, which re-applies our intent.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::runtime::controller::Action;
use kube::runtime::watcher::Config as WatcherConfig;
use kube::runtime::Controller;
use kube::{Api, Client, Resource};
use tracing::{error, info, warn};

use crate::crd::{Network, NetworkStatus};
use crate::modes::{resolve_network, EffectiveConfig, NAMESPACE};
use crate::render::Rendered;
use crate::{apply, health, immutable, render, status};

/// Resync even when nothing has changed. Catches anything a watch could miss —
/// notably rollout progress, which changes pod status without touching an
/// object we own.
const RESYNC: Duration = Duration::from_secs(60);
/// Backoff after a failed pass.
const RETRY: Duration = Duration::from_secs(15);
/// Faster requeue while we are waiting on cilium-operator to register its CRDs.
const DEFERRED_RETRY: Duration = Duration::from_secs(10);

pub struct Context {
    pub client: Client,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Network spec — {0}")]
    Invalid(#[from] crate::modes::ValidationError),

    #[error("rejected: {0}")]
    Immutable(String),

    #[error(transparent)]
    Apply(#[from] apply::ApplyError),

    #[error("kubernetes: {0}")]
    Kube(#[from] kube::Error),
}

/// Run the controller until the process is signalled.
pub async fn run(client: Client) -> anyhow::Result<()> {
    let networks: Api<Network> = Api::all(client.clone());
    let ctx = Arc::new(Context { client: client.clone() });

    // Owned objects, watched so drift and deletions self-heal.
    let daemonsets: Api<DaemonSet> = Api::namespaced(client.clone(), NAMESPACE);
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), NAMESPACE);

    info!(namespace = NAMESPACE, "starting network-operator");

    Controller::new(networks, WatcherConfig::default())
        .owns(daemonsets, WatcherConfig::default())
        .owns(deployments, WatcherConfig::default())
        .owns(configmaps, WatcherConfig::default())
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => info!(network = %obj.name, "reconciled"),
                Err(e) => warn!(error = %e, "reconcile failed"),
            }
        })
        .await;
    Ok(())
}

async fn reconcile(net: Arc<Network>, ctx: Arc<Context>) -> Result<Action, Error> {
    let name = net.meta().name.clone().unwrap_or_default();
    let generation = net.meta().generation.unwrap_or(0);
    let previous = net.status.clone().unwrap_or_default();

    // 1. resolve
    let cfg = match resolve_network(&net) {
        Ok(cfg) => cfg,
        Err(e) => {
            // A spec we cannot resolve is a user error, not a transient one:
            // record it and stop rather than hot-looping on it.
            report_failure(&ctx, &name, &previous, generation, &e.to_string()).await?;
            return Err(Error::Invalid(e));
        }
    };

    // 2. refuse changes that cannot be made under a live dataplane
    let violations = immutable::check(&cfg, net.status.as_ref());
    if !violations.is_empty() {
        let message = violations
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        report_failure(&ctx, &name, &previous, generation, &message).await?;
        return Err(Error::Immutable(message));
    }

    // 3. apply
    let objects = render::render(&cfg);
    let deferred = match apply::apply_all(&ctx.client, &objects).await {
        Ok(deferred) => deferred,
        Err(e) => {
            report_failure(&ctx, &name, &previous, generation, &e.to_string()).await?;
            return Err(Error::Apply(e));
        }
    };

    // 4. reap what this config no longer wants
    reap(&ctx, &cfg, &objects).await?;

    // 5. observe and report
    let snapshot = health::observe(&ctx.client, deferred.clone()).await?;

    let mut next = previous.clone();
    next.conditions = health::conditions(&previous.conditions, &snapshot, generation, &now());
    next.observed_generation = Some(generation);
    immutable::applied_from(&cfg, &mut next);
    status::write(&ctx.client, &name, &next).await?;

    Ok(Action::requeue(if deferred.is_empty() {
        RESYNC
    } else {
        DEFERRED_RETRY
    }))
}

/// Delete objects a previous config rendered but this one does not — otherwise
/// turning BGP off would leave the peering CRs behind, still advertising.
async fn reap(ctx: &Context, cfg: &EffectiveConfig, keep: &[Rendered]) -> Result<(), Error> {
    let wanted: Vec<String> = keep.iter().map(|r| r.id()).collect();
    for candidate in render::reapable(cfg) {
        if !wanted.contains(&candidate.id()) {
            info!(object = %candidate.id(), "removing — no longer part of the desired config");
            apply::delete_one(&ctx.client, &candidate).await?;
        }
    }
    Ok(())
}

/// Record a `Degraded` reason without touching `applied*` — the install on the
/// cluster is unchanged, so the immutability baseline must not move.
async fn report_failure(
    ctx: &Context,
    name: &str,
    previous: &NetworkStatus,
    generation: i64,
    message: &str,
) -> Result<(), Error> {
    error!(network = %name, %message, "reconcile failed");
    // Still report on whatever is running — a bad spec edit does not stop the
    // installed dataplane, and Available should keep saying so.
    let mut snapshot = health::observe(&ctx.client, Vec::new())
        .await
        .unwrap_or_default();
    snapshot.failure = Some(message.to_string());

    let mut next = previous.clone();
    next.conditions = health::conditions(&previous.conditions, &snapshot, generation, &now());
    next.observed_generation = Some(generation);
    status::write(&ctx.client, name, &next).await?;
    Ok(())
}

fn on_error(_net: Arc<Network>, _err: &Error, _ctx: Arc<Context>) -> Action {
    Action::requeue(RETRY)
}

fn now() -> Time {
    Time(chrono::Utc::now())
}
