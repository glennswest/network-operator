//! Writing `Network.status` and `VLAN.status`.
//!
//! rustkube's CR handling has historically been partial (rustkube#23): some
//! builds serve no `/status` subresource, and older ones reject PATCH entirely.
//! So this walks down a ladder — patch `/status`, then PUT `/status`, then
//! read-modify-PUT the whole object — and only the last rung can clobber a
//! concurrent spec write, which is why it is last.

use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::{Client, Error as KubeError, Resource};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use tracing::debug;

use crate::crd::{Network, NetworkStatus};
use crate::render::FIELD_MANAGER;
use crate::vlan::crd::{Vlan, VlanStatus};

/// Write `Network.status`, returning the object as the apiserver stored it.
pub async fn write(
    client: &Client,
    name: &str,
    status: &NetworkStatus,
) -> Result<Network, KubeError> {
    let api: Api<Network> = Api::all(client.clone());
    write_status(&api, name, "Network", status, |n: &mut Network| n.status = Some(status.clone())).await
}

/// Write `VLAN.status` (#32), down the same ladder.
pub async fn write_vlan(client: &Client, name: &str, status: &VlanStatus) -> Result<Vlan, KubeError> {
    let api: Api<Vlan> = Api::all(client.clone());
    write_status(&api, name, "VLAN", status, |v: &mut Vlan| v.status = Some(status.clone())).await
}

/// The ladder, for any `network.storm.io/v1` kind: `set` puts the status on
/// a fetched object.
async fn write_status<K, S>(
    api: &Api<K>,
    name: &str,
    kind: &str,
    status: &S,
    set: impl Fn(&mut K),
) -> Result<K, KubeError>
where
    K: Resource + Clone + DeserializeOwned + Serialize + std::fmt::Debug,
    S: Serialize,
{
    match api
        .patch_status(
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&json!({
                "apiVersion": "network.storm.io/v1",
                "kind": kind,
                "status": status,
            })),
        )
        .await
    {
        Ok(obj) => return Ok(obj),
        Err(e) if !unsupported(&e) => return Err(e),
        Err(e) => debug!(error = %e, "status patch unsupported; trying PUT /status"),
    }

    // PUT /status: still confined to the subresource, so a spec write racing us
    // cannot be lost.
    let mut current = api.get(name).await?;
    set(&mut current);
    let encoded = serde_json::to_vec(&current).map_err(KubeError::SerdeError)?;
    match api.replace_status(name, &PostParams::default(), encoded).await {
        Ok(obj) => return Ok(obj),
        Err(e) if !unsupported(&e) => return Err(e),
        Err(e) => debug!(error = %e, "PUT /status unsupported; falling back to whole-object PUT"),
    }

    // Last resort: replace the whole object. Re-read first so we PUT back the
    // spec that is currently stored rather than a stale one.
    let mut current = api.get(name).await?;
    set(&mut current);
    api.replace(name, &PostParams::default(), &current).await
}

/// Whether the apiserver is telling us this *route* does not exist, as opposed
/// to rejecting this particular write.
fn unsupported(e: &KubeError) -> bool {
    match e {
        KubeError::Api(e) => matches!(e.code, 404 | 405 | 501),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::ErrorResponse;

    fn api_err(code: u16) -> KubeError {
        KubeError::Api(ErrorResponse {
            status: "Failure".into(),
            message: String::new(),
            reason: String::new(),
            code,
        })
    }

    #[test]
    fn missing_routes_fall_through_but_real_rejections_do_not() {
        for code in [404, 405, 501] {
            assert!(unsupported(&api_err(code)), "{code} should fall through");
        }
        for code in [400, 403, 409, 422, 500] {
            assert!(!unsupported(&api_err(code)), "{code} must surface");
        }
    }
}
