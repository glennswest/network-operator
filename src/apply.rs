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
    let name = r.name();
    let params = PatchParams::apply(FIELD_MANAGER).force();

    match api.patch(name, &params, &Patch::Apply(&r.obj)).await {
        Ok(_) => {
            debug!(object = %r.id(), "applied");
            Ok(())
        }
        Err(KubeError::Api(e)) if is_kind_missing(&e) => Err(ApplyError::KindNotFound { id: r.id() }),
        // rustkube builds without PATCH support: fall back to create/replace.
        Err(KubeError::Api(e)) if e.code == 405 || e.code == 501 => {
            warn!(object = %r.id(), "apiserver rejected PATCH; falling back to create/replace");
            replace(&api, r).await
        }
        Err(source) => Err(ApplyError::Api { id: r.id(), source }),
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
}
