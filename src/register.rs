//! Self-registration of the `Network` CRD, kept current across upgrades (#13).
//!
//! The CRD is stamped with the operator version that wrote it
//! ([`VERSION_ANNOTATION`]). On start the operator creates it if absent, and
//! replaces it when its spec differs — unless the stamp says a *newer*
//! operator wrote it, so rolling the operator back never strips fields a newer
//! schema added from stored CRs.
//!
//! It is a replace (GET, then PUT with the `resourceVersion`), not an apply:
//! rustkube's apply is a plain merge, which would never drop a property the
//! new schema removed. [`decide`] is pure, so every case is unit-tested.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, PostParams};
use kube::{Client, CustomResourceExt};
use tracing::{info, warn};

use crate::crd::Network;

/// Which operator version last wrote the CRD.
pub const VERSION_ANNOTATION: &str = "network.storm.io/operator-version";

/// This operator's version, as stamped on the CRD.
pub const OPERATOR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `Network` CRD as this operator registers it: the generated schema plus
/// the version stamp. `crdgen` prints this, so `deploy/crds/` carries it too.
pub fn crd() -> CustomResourceDefinition {
    let mut crd = Network::crd();
    crd.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(VERSION_ANNOTATION.to_string(), OPERATOR_VERSION.to_string());
    crd
}

/// What to do about the CRD on the cluster.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Create,
    /// Spec or stamp differs and the cluster's copy is not newer.
    Replace,
    /// Already what we would write.
    Current,
    /// A newer operator wrote it; leave it alone. Carries that version.
    Newer(String),
}

/// Decide against the CRD currently on the cluster (`None` = absent).
/// An unstamped CRD (applied by hand from an older `deploy/crds/`, or by a
/// pre-#13 operator) is treated as older.
pub fn decide(existing: Option<&CustomResourceDefinition>, ours: &CustomResourceDefinition) -> Action {
    let Some(existing) = existing else { return Action::Create };
    let stamp = |c: &CustomResourceDefinition| {
        c.metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(VERSION_ANNOTATION))
            .cloned()
    };
    let theirs = stamp(existing);
    let ours_v = stamp(ours).unwrap_or_default();

    if let Some(v) = &theirs {
        if newer(v, &ours_v) {
            return Action::Newer(v.clone());
        }
    }
    if existing.spec == ours.spec && theirs.as_deref() == Some(ours_v.as_str()) {
        Action::Current
    } else {
        Action::Replace
    }
}

/// `a > b` by `MAJOR.MINOR.PATCH`, ignoring any pre-release/build suffix. An
/// unparsable version is never called newer, so it gets overwritten.
fn newer(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// Register the `Network` CRD, or bring it up to this operator's schema.
pub async fn ensure_crd(client: &Client) -> Result<(), kube::Error> {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let mut ours = crd();
    let name = ours.metadata.name.clone().unwrap_or_default();
    let existing = crds.get_opt(&name).await?;

    match decide(existing.as_ref(), &ours) {
        Action::Create => match crds.create(&PostParams::default(), &ours).await {
            Ok(_) => info!(version = OPERATOR_VERSION, "registered Network CRD"),
            // Another replica (or a hand apply) won the race; the next start
            // will reconcile its schema.
            Err(kube::Error::Api(e)) if e.code == 409 => info!("Network CRD already present"),
            Err(e) => return Err(e),
        },
        Action::Replace => {
            let existing = existing.expect("Replace is only decided for an existing CRD");
            ours.metadata.resource_version = existing.metadata.resource_version;
            crds.replace(&name, &PostParams::default(), &ours).await?;
            info!(version = OPERATOR_VERSION, "updated Network CRD schema");
        }
        Action::Current => info!(version = OPERATOR_VERSION, "Network CRD is current"),
        Action::Newer(v) => warn!(
            crd_version = %v,
            operator_version = OPERATOR_VERSION,
            "Network CRD was written by a newer operator; leaving it as is"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamped(version: Option<&str>) -> CustomResourceDefinition {
        let mut c = Network::crd();
        if let Some(v) = version {
            c.metadata.annotations =
                Some([(VERSION_ANNOTATION.to_string(), v.to_string())].into());
        }
        c
    }

    #[test]
    fn an_absent_crd_is_created() {
        assert_eq!(decide(None, &crd()), Action::Create);
    }

    #[test]
    fn our_own_crd_is_left_alone() {
        assert_eq!(decide(Some(&crd()), &crd()), Action::Current);
    }

    #[test]
    fn an_older_schema_is_replaced() {
        let mut old = stamped(Some("0.1.0"));
        old.spec.versions[0].schema = None;
        assert_eq!(decide(Some(&old), &crd()), Action::Replace);
    }

    #[test]
    fn an_unstamped_crd_is_restamped_even_with_the_same_schema() {
        // A pre-#13 operator or a hand apply; afterwards a rollback can tell.
        assert_eq!(decide(Some(&stamped(None)), &crd()), Action::Replace);
    }

    #[test]
    fn a_newer_operators_crd_survives_a_rollback() {
        let mut newer_crd = stamped(Some("99.0.0"));
        newer_crd.spec.versions[0].schema = None;
        assert_eq!(decide(Some(&newer_crd), &crd()), Action::Newer("99.0.0".into()));
    }

    #[test]
    fn versions_compare_numerically_not_as_strings() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.9.9", "0.10.0"));
        assert!(!newer("0.3.0", "0.3.0"));
        assert!(newer("v1.0.0-rc1", "0.3.0"));
        assert!(!newer("garbage", "0.3.0"));
        assert!(!newer("1.2", "0.3.0"));
    }

    #[test]
    fn the_stamp_is_this_operators_version() {
        let c = crd();
        assert_eq!(
            c.metadata.annotations.unwrap()[VERSION_ANNOTATION],
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn the_shipped_manifest_is_what_the_operator_registers() {
        // `make crds` output; a stale manifest would regress the stamp.
        let shipped: CustomResourceDefinition = serde_yaml::from_str(include_str!(
            "../deploy/crds/network.storm.io_networks.yaml"
        ))
        .unwrap();
        assert_eq!(shipped, crd(), "deploy/crds/ is stale: run `make crds`");
    }
}
