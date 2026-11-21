//! Server-side apply of the rendered object set.
//!
//! SSA is what makes drift self-healing: we always re-send our full intent under
//! a stable field manager, so a hand-edited ConfigMap or a stripped DaemonSet
//! field is put back on the next reconcile without us having to diff anything.
//!
//! Two accommodations for the rustkube apiserver this stack runs against:
//!
//! * It implements `application/apply-patch+yaml` as a plain merge with no
//!   field-ownership tracking. Our fields are still restored; fields *added* by
//!   someone else survive rather than being pruned. Drift-heal, not drift-purge.
//!   That merge is wrong for a ConfigMap, whose `data` is a map we own whole:
//!   a key a later render drops (`mtu` back to auto, `proxy-prometheus-port`
//!   once standalone Envoy takes the port) would survive the merge. So after
//!   applying a ConfigMap we read it back and, if its `data`/`binaryData` is
//!   not exactly the rendered map, replace it with that map (carrying the
//!   resourceVersion, retrying conflicts) — see [`exact_data`]. On an
//!   apiserver with real SSA the read finds nothing to do.
//! * Older builds reject PATCH outright, so a 405/501 falls back to
//!   create-then-replace.

use kube::api::{Api, DeleteParams, DynamicObject, Patch, PatchParams, PostParams};
use kube::core::ErrorResponse;
use kube::{Client, Error as KubeError};
use tracing::{debug, warn};

use crate::render::{Rendered, FIELD_MANAGER};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The object's CRD does not exist yet. For Cilium's own CRs this is
    /// expected on a fresh install — `cilium-operator` has not created them.
    #[error("{id}: kind not registered yet")]
    KindNotFound { id: String },

    #[error("{id}: {source}")]
    Api {
        id: String,
        #[source]
        source: KubeError,
    },
}

/// Apply one object, forcing conflicts so that a previous owner (a hand-run
/// `kubectl apply`, or an older field manager) cannot pin a field against us.
pub async fn apply_one(client: &Client, r: &Rendered) -> Result<(), ApplyError> {
    let api = api_for(client, r);
    apply_merge(&api, r).await?;
    if is_configmap(r) {
        enforce_exact_data(&api, r).await?;
    }
    Ok(())
}

async fn apply_merge(api: &Api<DynamicObject>, r: &Rendered) -> Result<(), ApplyError> {
    let name = r.name();
    let params = PatchParams::apply(FIELD_MANAGER).force();

    match api.patch(name, &params, &Patch::Apply(&r.obj)).await {
        Ok(_) => {
            debug!(object = %r.id(), "applied");
            Ok(())
        }
        Err(KubeError::Api(e)) => match classify(&e) {
            Disposition::DeferKind => Err(ApplyError::KindNotFound { id: r.id() }),

            Disposition::CreateMissing => {
                warn!(
                    object = %r.id(),
                    "apply did not create the missing object (server-side apply must upsert); creating it directly"
                );
                match replace(api, r).await {
                    // Still absent after an explicit create: it was the kind
                    // that was missing all along, not the object. Hand it back
                    // to the deferral path so Cilium's own CRs still wait for
                    // cilium-operator rather than failing the reconcile.
                    Err(ApplyError::Api { source: KubeError::Api(e), .. }) if e.code == 404 => {
                        Err(ApplyError::KindNotFound { id: r.id() })
                    }
                    other => other,
                }
            }

            Disposition::NoPatchSupport => {
                warn!(object = %r.id(), "apiserver rejected PATCH; falling back to create/replace");
                replace(api, r).await
            }

            Disposition::Fail => Err(ApplyError::Api { id: r.id(), source: KubeError::Api(e) }),
        },
        Err(source) => Err(ApplyError::Api { id: r.id(), source }),
    }
}

/// How many times a ConfigMap's exact-data replace is retried on a 409
/// (someone wrote it between our read and our replace).
const EXACT_DATA_ATTEMPTS: usize = 5;

fn is_configmap(r: &Rendered) -> bool {
    r.api.group.is_empty() && r.api.kind == "ConfigMap"
}

/// Make the stored ConfigMap's data exactly the rendered data (finding
/// 32389d356451): read, and if it differs, replace with the resourceVersion
/// just read; a conflict means it moved under us, so read again.
async fn enforce_exact_data(api: &Api<DynamicObject>, r: &Rendered) -> Result<(), ApplyError> {
    let name = r.name();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let current = api
            .get(name)
            .await
            .map_err(|source| ApplyError::Api { id: r.id(), source })?;
        let Some(next) = exact_data(&current, &r.obj) else {
            return Ok(());
        };
        match api.replace(name, &PostParams::default(), &next).await {
            Ok(_) => {
                debug!(object = %r.id(), "pruned ConfigMap keys the render no longer has");
                return Ok(());
            }
            Err(KubeError::Api(e)) if e.code == 409 && attempt < EXACT_DATA_ATTEMPTS => {
                debug!(object = %r.id(), attempt, "conflict replacing ConfigMap data; re-reading");
            }
            Err(source) => return Err(ApplyError::Api { id: r.id(), source }),
        }
    }
}

/// The ConfigMap fields whose contents are ours whole.
const DATA_FIELDS: [&str; 2] = ["data", "binaryData"];

/// If `current`'s data maps differ from `rendered`'s, the object to replace it
/// with: `current` (its metadata and resourceVersion) carrying exactly the
/// rendered `data` and `binaryData`, an absent one removed. `None` when they
/// already match. Pure, so the pruning rule is unit-tested.
pub fn exact_data(current: &DynamicObject, rendered: &DynamicObject) -> Option<DynamicObject> {
    let empty = serde_json::Map::new();
    let map_of = |o: &DynamicObject, f: &str| {
        o.data.get(f).and_then(|v| v.as_object()).cloned().unwrap_or_else(|| empty.clone())
    };
    if DATA_FIELDS.iter().all(|f| map_of(current, f) == map_of(rendered, f)) {
        return None;
    }
    let mut next = current.clone();
    if !next.data.is_object() {
        next.data = serde_json::Value::Object(serde_json::Map::new());
    }
    let body = next.data.as_object_mut().expect("made an object above");
    for f in DATA_FIELDS {
        match rendered.data.get(f) {
            Some(v) if v.as_object().is_some_and(|m| !m.is_empty()) => {
                body.insert(f.to_string(), v.clone());
            }
            _ => {
                body.remove(f);
            }
        }
    }
    Some(next)
}

/// What to do about an error from an apply. Split out from [`apply_one`] so the
/// precedence between the two very different meanings of 404 is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    /// The kind is not registered. Expected for `cilium.io/*` on a fresh
    /// install; the caller requeues instead of failing.
    DeferKind,
    /// The kind exists, the object does not, and the apiserver did not create
    /// it. Server-side apply is an upsert (KEP-555), so this is an apiserver
    /// bug (rustkube#45); create it ourselves.
    CreateMissing,
    /// No PATCH verb at all. Older rustkube builds.
    NoPatchSupport,
    /// A real rejection — forbidden, invalid, conflict, server error.
    Fail,
}

fn classify(e: &ErrorResponse) -> Disposition {
    // Order matters: a missing *kind* and a missing *object* are both 404, and
    // only the kind case may be deferred.
    if is_kind_missing(e) {
        Disposition::DeferKind
    } else if e.code == 404 {
        Disposition::CreateMissing
    } else if e.code == 405 || e.code == 501 {
        Disposition::NoPatchSupport
    } else {
        Disposition::Fail
    }
}

/// Apply every object in order. Returns the ids of Cilium CRs skipped because
/// their CRD is not registered yet, so the caller can requeue rather than fail.
pub async fn apply_all(client: &Client, objects: &[Rendered]) -> Result<Vec<String>, ApplyError> {
    let mut deferred = Vec::new();
    for r in objects {
        match apply_one(client, r).await {
            Ok(()) => {}
            Err(ApplyError::KindNotFound { id }) if crate::render::is_cilium_cr(r) => {
                debug!(object = %id, "deferring — cilium-operator has not installed this CRD yet");
                deferred.push(id);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(deferred)
}

/// Create the object, or replace it wholesale if it already exists. Only used
/// on apiservers without PATCH; it is a blunter tool than SSA — it overwrites
/// the whole object rather than merging our fields into it.
async fn replace(api: &Api<DynamicObject>, r: &Rendered) -> Result<(), ApplyError> {
    let name = r.name();
    match api.create(&PostParams::default(), &r.obj).await {
        Ok(_) => return Ok(()),
        Err(KubeError::Api(e)) if e.code == 409 => {}
        Err(source) => return Err(ApplyError::Api { id: r.id(), source }),
    }

    // Carry the current resourceVersion, or the replace is rejected as a
    // mid-air collision.
    let current = api
        .get(name)
        .await
        .map_err(|source| ApplyError::Api { id: r.id(), source })?;
    let mut next = r.obj.clone();
    next.metadata.resource_version = current.metadata.resource_version;

    api.replace(name, &PostParams::default(), &next)
        .await
        .map(|_| ())
        .map_err(|source| ApplyError::Api { id: r.id(), source })
}

/// Delete an object we previously rendered but no longer do — e.g. the BGP CRs
/// after a switch away from `announce: bgp`. A missing object is success.
pub async fn delete_one(client: &Client, r: &Rendered) -> Result<(), ApplyError> {
    let api = api_for(client, r);
    match api.delete(r.name(), &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(KubeError::Api(e)) if e.code == 404 || is_kind_missing(&e) => Ok(()),
        Err(source) => Err(ApplyError::Api { id: r.id(), source }),
    }
}

fn api_for(client: &Client, r: &Rendered) -> Api<DynamicObject> {
    match &r.namespace {
        Some(ns) => Api::namespaced_with(client.clone(), ns, &r.api),
        None => Api::all_with(client.clone(), &r.api),
    }
}

/// A 404 whose message is about the *resource type* means the kind is unknown,
/// as opposed to a 404 naming an object, which means that object is absent.
fn is_kind_missing(e: &ErrorResponse) -> bool {
    e.code == 404
        && (e.message.contains("could not find the requested resource")
            || e.message.contains("no matches for kind")
            || e.message.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(code: u16, message: &str) -> ErrorResponse {
        ErrorResponse {
            status: "Failure".into(),
            message: message.into(),
            reason: "NotFound".into(),
            code,
        }
    }

    #[test]
    fn recognises_an_unregistered_kind() {
        assert!(is_kind_missing(&err(
            404,
            "the server could not find the requested resource"
        )));
        assert!(is_kind_missing(&err(404, "")));
        assert!(is_kind_missing(&err(404, "no matches for kind \"CiliumBGPPeerConfig\"")));
    }

    #[test]
    fn a_missing_named_object_is_not_a_missing_kind() {
        assert!(!is_kind_missing(&err(
            404,
            "ciliumloadbalancerippools.cilium.io \"storm-default\" not found"
        )));
        assert!(!is_kind_missing(&err(403, "")));
    }

    #[test]
    fn the_two_meanings_of_404_are_dispatched_differently() {
        // Kind not registered -> defer, so cilium.io CRs wait for the operator.
        assert_eq!(
            classify(&err(404, "the server could not find the requested resource")),
            Disposition::DeferKind
        );
        assert_eq!(
            classify(&err(404, "no matches for kind \"CiliumBGPPeerConfig\"")),
            Disposition::DeferKind
        );

        // Object absent -> create it ourselves. This is the rustkube#45 shape,
        // verbatim from the failure on the rig.
        assert_eq!(
            classify(&err(
                404,
                "resource \"/registry/serviceaccounts/kube-system/cilium\" not found"
            )),
            Disposition::CreateMissing
        );
    }

    #[test]
    fn real_rejections_are_never_papered_over() {
        for (code, msg) in [
            (403, "forbidden"),
            (409, "conflict"),
            (422, "Invalid value"),
            (500, "internal error"),
        ] {
            assert_eq!(classify(&err(code, msg)), Disposition::Fail, "{code} must fail");
        }
    }

    fn cm(data: serde_json::Value, rv: &str) -> DynamicObject {
        let mut o: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "cilium-config", "namespace": "kube-system",
                         "resourceVersion": rv, "labels": {"someone": "else"}},
        }))
        .unwrap();
        o.data = data;
        o
    }

    #[test]
    fn a_key_the_render_dropped_is_pruned() {
        // `mtu` back to auto: the render omits it; the merge kept it.
        let stored = cm(serde_json::json!({"data": {"mtu": "1400", "debug": "false"}}), "7");
        let rendered = cm(serde_json::json!({"data": {"debug": "false"}}), "");
        let next = exact_data(&stored, &rendered).expect("must replace");
        assert_eq!(next.data["data"], serde_json::json!({"debug": "false"}));
        // The replace carries the resourceVersion read, and keeps metadata.
        assert_eq!(next.metadata.resource_version.as_deref(), Some("7"));
        assert_eq!(next.metadata.labels.unwrap()["someone"], "else");
    }

    #[test]
    fn standalone_envoy_drops_the_proxy_prometheus_port() {
        let stored = cm(
            serde_json::json!({"data": {"proxy-prometheus-port": "9964", "external-envoy-proxy": "false"}}),
            "3",
        );
        let rendered = cm(serde_json::json!({"data": {"external-envoy-proxy": "true"}}), "");
        let next = exact_data(&stored, &rendered).unwrap();
        assert_eq!(next.data["data"], serde_json::json!({"external-envoy-proxy": "true"}));
    }

    #[test]
    fn matching_data_needs_no_replace() {
        let stored = cm(serde_json::json!({"data": {"a": "1"}}), "9");
        assert!(exact_data(&stored, &cm(serde_json::json!({"data": {"a": "1"}}), "")).is_none());
        // Absent and empty are the same map.
        assert!(exact_data(&cm(serde_json::json!({}), "1"), &cm(serde_json::json!({"data": {}}), "")).is_none());
    }

    #[test]
    fn binary_data_we_never_rendered_is_removed() {
        let stored = cm(serde_json::json!({"data": {"a": "1"}, "binaryData": {"x": "AA=="}}), "2");
        let next = exact_data(&stored, &cm(serde_json::json!({"data": {"a": "1"}}), "")).unwrap();
        assert!(next.data.get("binaryData").is_none());
        assert_eq!(next.data["data"], serde_json::json!({"a": "1"}));
    }

    #[test]
    fn only_core_configmaps_get_the_exact_data_pass() {
        use kube::core::{ApiResource, GroupVersionKind};
        let r = |g: &str, k: &str| Rendered {
            api: ApiResource::from_gvk(&GroupVersionKind::gvk(g, "v1", k)),
            namespace: None,
            obj: cm(serde_json::json!({}), ""),
        };
        assert!(is_configmap(&r("", "ConfigMap")));
        assert!(!is_configmap(&r("", "Secret")));
        assert!(!is_configmap(&r("apps", "DaemonSet")));
    }

    #[test]
    fn absent_patch_verb_falls_back_to_create_replace() {
        assert_eq!(classify(&err(405, "")), Disposition::NoPatchSupport);
        assert_eq!(classify(&err(501, "")), Disposition::NoPatchSupport);
    }
}
