//! Cilium versions this operator knows, pinned by digest.
//!
//! A tag is a name someone else controls and can move; a digest is the bytes.
//! This stack follows digests and nothing else, so `spec.cilium.version`
//! resolves through this table to a digest per image, and a version that is
//! not in it is rejected unless the CR names every image by digest itself
//! (`spec.cilium.images`).
//!
//! The digests are copied from stormcos-cilium's `pinned.txt`, which is the
//! source of truth for the Cilium stormcos runs. They are the **linux/amd64
//! child** manifests, not the multi-arch index: an index lets the resolver
//! pick the architecture, and one once picked arm64 on an x86_64 fleet with
//! nothing noticing until a node could not exec the binary.
//!
//! Bumping Cilium means adding a row here from stormcos-cilium's file (its
//! `scripts/update-pin.sh` resolves them), not typing a digest from a
//! terminal.

/// One Cilium release: the agent, operator and relay move together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    /// Without a leading `v`, as `spec.cilium.version` is usually written.
    pub version: &'static str,
    /// `cilium/cilium`.
    pub agent: &'static str,
    /// `cilium/operator-generic`.
    pub operator: &'static str,
    /// `cilium/hubble-relay`.
    pub hubble_relay: &'static str,
}

/// The platform every digest below was resolved for.
pub const PLATFORM: &str = "linux/amd64";

/// Known releases. From stormcos-cilium `pinned.txt` (e999661).
pub const PINS: &[Pin] = &[Pin {
    version: "1.20.2",
    agent: "sha256:9d308e3f7f05972b0b0604c40d2b0f08fa2f6a55084fef1e1aaadb469e430639",
    operator: "sha256:5bb0f9d871dd7b80461f8e3056896daa51e7d9b70ffe51c3e2126b4c35c02b5e",
    hubble_relay: "sha256:48d39e14cb2326c49d1903faed0508feb92ca0ceef9f3a1f1b779ca4c1e51c5d",
}];

/// The pin for `version`, written with or without the leading `v`.
pub fn lookup(version: &str) -> Option<&'static Pin> {
    let v = version.trim().trim_start_matches('v');
    PINS.iter().find(|p| p.version == v)
}

/// The versions in [`PINS`], for error messages.
pub fn known_versions() -> String {
    PINS.iter().map(|p| p.version).collect::<Vec<_>>().join(", ")
}

/// Whether `image` is a reference pinned by digest: `<repository>@sha256:<64 hex>`.
pub fn is_digest_ref(image: &str) -> bool {
    let Some((repo, digest)) = image.split_once('@') else {
        return false;
    };
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    !repo.is_empty()
        && hex.len() == 64
        && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pin_is_a_well_formed_digest() {
        for p in PINS {
            for d in [p.agent, p.operator, p.hubble_relay] {
                assert!(is_digest_ref(&format!("quay.io/cilium/x@{d}")), "{} {d}", p.version);
            }
        }
    }

    #[test]
    fn lookup_accepts_the_v_prefix() {
        assert_eq!(lookup("1.20.2").unwrap().version, "1.20.2");
        assert_eq!(lookup("v1.20.2").unwrap().version, "1.20.2");
        assert!(lookup("1.19.6").is_none());
    }

    #[test]
    fn a_tag_is_not_a_digest() {
        assert!(!is_digest_ref("quay.io/cilium/cilium:v1.20.2"));
        assert!(!is_digest_ref("quay.io/cilium/cilium@sha256:abc"));
        assert!(!is_digest_ref("@sha256:9d308e3f7f05972b0b0604c40d2b0f08fa2f6a55084fef1e1aaadb469e430639"));
        assert!(!is_digest_ref(
            "quay.io/cilium/cilium@sha256:9D308E3F7F05972B0B0604C40D2B0F08FA2F6A55084FEF1E1AAADB469E430639"
        ));
        // A tag *and* a digest is still pinned: the digest is what is pulled.
        assert!(is_digest_ref(
            "quay.io/cilium/cilium:v1.20.2@sha256:9d308e3f7f05972b0b0604c40d2b0f08fa2f6a55084fef1e1aaadb469e430639"
        ));
    }
}
