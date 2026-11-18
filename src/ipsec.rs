//! The IPsec keyfile Secret (#44).
//!
//! Cilium's IPsec reads a pre-shared key from `kube-system/cilium-ipsec-keys`
//! (key `keys`), one line of `<SPI>[+] <algorithm> <hex key> <ICV bits>`. The
//! render turns IPsec on and mounts that Secret into the agent; this module
//! makes sure the Secret is there and rotates it on request:
//!
//! * **Create if absent, never overwrite.** A missing Secret gets a fresh key
//!   (20 random bytes — a 128-bit AES-GCM key plus its 4-byte salt — SPI 3,
//!   per-tunnel keys `+`, `rfc4106(gcm(aes))`, 128-bit ICV: the line Cilium's
//!   docs generate). An existing Secret, ours or one made by hand, is left as
//!   it is.
//! * **Rotate on request.** Setting the `Network` annotation
//!   [`ROTATE_ANNOTATION`] to a new value writes a new key with the next SPI
//!   (1–15, wrapping 15 -> 1), keeping the algorithm, key length and ICV size
//!   of the current line. The value handled is recorded on the Secret, so a
//!   request is acted on once. Agents pick the new file up with the key
//!   watcher and keep the old key for [`ROTATION_DURATION`]; a second rotation
//!   inside that window is held back until it has passed, or the old key
//!   still in use would be dropped.
//! * **Off means gone.** When `encryption.type` is no longer `ipsec`, the
//!   Secret is deleted — but only one this operator created (labelled
//!   `managed-by: network-operator`); a hand-made one is left alone.
//!
//! The key is never logged: [`KeyLine`]'s `Debug` redacts it, and parse
//! errors do not quote the line.
//!
//! The decisions ([`KeyLine`], [`next_spi`], [`plan`]) are pure and unit
//! tested; [`reconcile`] and [`remove`] are the thin apiserver side.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::ByteString;
use kube::api::{Api, DeleteParams, PostParams};
use kube::{Client, Error as KubeError};
use tracing::info;

use crate::modes::{EffectiveConfig, NAMESPACE};
use crate::render::FIELD_MANAGER;

/// The Secret Cilium's chart names, and the agent mounts.
pub const SECRET: &str = "cilium-ipsec-keys";
/// The key inside it.
const DATA_KEY: &str = "keys";
/// Where the agent mounts the Secret.
pub const MOUNT_PATH: &str = "/etc/ipsec";
/// `ipsec-key-file`: [`MOUNT_PATH`]/[`DATA_KEY`].
pub const KEY_FILE: &str = "/etc/ipsec/keys";
/// `ipsec-key-rotation-duration`: how long an agent keeps the previous key
/// after the file changes (the chart's default).
pub const ROTATION_DURATION: &str = "5m";
/// [`ROTATION_DURATION`] as a duration, for holding back a second rotation.
const ROTATION_WINDOW: Duration = Duration::from_secs(5 * 60);

/// On the `Network`: change its value to rotate the key. On the Secret: the
/// value last acted on.
pub const ROTATE_ANNOTATION: &str = "network.storm.io/ipsec-key-rotation";
/// On the Secret: when the key was last written (RFC 3339).
const ROTATED_AT_ANNOTATION: &str = "network.storm.io/ipsec-key-rotated-at";

const ALGORITHM: &str = "rfc4106(gcm(aes))";
/// 16-byte AES-128 key + 4-byte salt.
const KEY_BYTES: usize = 20;
const ICV_BITS: u32 = 128;
/// The SPI Cilium's docs start from.
const FIRST_SPI: u8 = 3;
/// SPIs are 4 bits on the wire, and 0 is reserved.
const MAX_SPI: u8 = 15;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("{SECRET}: {0}")]
    Unparsable(&'static str),
    #[error("{SECRET} has no `{DATA_KEY}` entry to rotate")]
    Missing,
}

#[derive(Debug, thiserror::Error)]
pub enum IpsecError {
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error("{SECRET}: reading /dev/urandom: {0}")]
    Random(#[from] std::io::Error),
    #[error("{SECRET}: {0}")]
    Kube(#[from] KubeError),
}

/// One line of Cilium's keyfile.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyLine {
    pub spi: u8,
    /// The `+` after the SPI: per-tunnel keys derived from this one. Cannot be
    /// switched on a running cluster, so a rotation keeps whatever it was.
    pub per_tunnel: bool,
    pub algorithm: String,
    key_hex: String,
    pub icv_bits: u32,
}

impl std::fmt::Debug for KeyLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyLine")
            .field("spi", &self.spi)
            .field("per_tunnel", &self.per_tunnel)
            .field("algorithm", &self.algorithm)
            .field("key", &"<redacted>")
            .field("icv_bits", &self.icv_bits)
            .finish()
    }
}

impl KeyLine {
    /// A new cluster's key: SPI 3, per-tunnel, AES-GCM-128.
    pub fn fresh(key: &[u8]) -> KeyLine {
        KeyLine {
            spi: FIRST_SPI,
            per_tunnel: true,
            algorithm: ALGORITHM.to_string(),
            key_hex: hex(key),
            icv_bits: ICV_BITS,
        }
    }

    /// The line after this one: next SPI, new key, everything else kept.
    pub fn rotated(&self, key: &[u8]) -> KeyLine {
        KeyLine {
            spi: next_spi(self.spi),
            key_hex: hex(key),
            ..self.clone()
        }
    }

    /// Bytes of key material, so a rotation generates the same length.
    pub fn key_len(&self) -> usize {
        self.key_hex.len() / 2
    }

    /// Parse the first line of a keyfile. Only the four-field AEAD form is
    /// understood (what this operator writes and Cilium's docs generate); the
    /// older auth+enc form has more fields and is refused rather than guessed.
    pub fn parse(text: &str) -> Result<KeyLine, KeyError> {
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .ok_or(KeyError::Unparsable("the keyfile is empty"))?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [spi, algorithm, key, icv] = fields[..] else {
            return Err(KeyError::Unparsable(
                "expected `<spi>[+] <algorithm> <hex key> <icv bits>`",
            ));
        };
        let (spi, per_tunnel) = match spi.strip_suffix('+') {
            Some(n) => (n, true),
            None => (spi, false),
        };
        let spi: u8 = spi
            .parse()
            .ok()
            .filter(|s| (1..=MAX_SPI).contains(s))
            .ok_or(KeyError::Unparsable("the SPI is not 1-15"))?;
        if key.is_empty() || key.len() % 2 != 0 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(KeyError::Unparsable("the key is not hex"));
        }
        let icv_bits = icv.parse().map_err(|_| KeyError::Unparsable("the ICV size is not a number"))?;
        Ok(KeyLine {
            spi,
            per_tunnel,
            algorithm: algorithm.to_string(),
            key_hex: key.to_ascii_lowercase(),
            icv_bits,
        })
    }

    /// The keyfile line, newline-terminated.
    pub fn render(&self) -> String {
        format!(
            "{}{} {} {} {}\n",
            self.spi,
            if self.per_tunnel { "+" } else { "" },
            self.algorithm,
            self.key_hex,
            self.icv_bits
        )
    }
}

/// The SPI after `spi`: 1..=15, wrapping 15 -> 1 (0 is reserved).
pub fn next_spi(spi: u8) -> u8 {
    spi % MAX_SPI + 1
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the cluster's Secret currently says, as far as [`plan`] cares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Existing {
    /// The `keys` entry, if any.
    pub keys: Option<String>,
    /// [`ROTATE_ANNOTATION`] on the Secret: the last request acted on.
    pub handled: Option<String>,
    pub rotated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// No Secret: generate one.
    Create,
    /// Present and no new rotation request.
    Keep,
    /// A new request: write the line after this one.
    Rotate(KeyLine),
    /// A new request inside the previous rotation's window: try again then.
    Wait(Duration),
}

/// Decide what to do with the Secret, given the `Network`'s rotation request.
pub fn plan(
    existing: Option<&Existing>,
    requested: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Plan, KeyError> {
    let Some(existing) = existing else { return Ok(Plan::Create) };
    let Some(requested) = requested else { return Ok(Plan::Keep) };
    if existing.handled.as_deref() == Some(requested) {
        return Ok(Plan::Keep);
    }
    if let Some(at) = existing.rotated_at {
        let elapsed = (now - at).to_std().unwrap_or(Duration::ZERO);
        if elapsed < ROTATION_WINDOW {
            return Ok(Plan::Wait(ROTATION_WINDOW - elapsed));
        }
    }
    let keys = existing.keys.as_deref().ok_or(KeyError::Missing)?;
    Ok(Plan::Rotate(KeyLine::parse(keys)?))
}

fn existing(secret: &Secret) -> Existing {
    let annotations = secret.metadata.annotations.clone().unwrap_or_default();
    Existing {
        keys: secret
            .data
            .as_ref()
            .and_then(|d| d.get(DATA_KEY))
            .map(|b| String::from_utf8_lossy(&b.0).into_owned()),
        handled: annotations.get(ROTATE_ANNOTATION).cloned(),
        rotated_at: annotations
            .get(ROTATED_AT_ANNOTATION)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc)),
    }
}

/// The new Secret for a cluster with none. Labelled and owned like every
/// rendered object, so it is recognisably ours and goes with the `Network`.
pub fn new_secret(
    cfg: &EffectiveConfig,
    line: &KeyLine,
    requested: Option<&str>,
    now: DateTime<Utc>,
) -> Secret {
    let mut metadata = crate::render::meta(cfg, SECRET, &[]);
    metadata.annotations = Some(stamp(BTreeMap::new(), requested, now));
    Secret {
        metadata,
        type_: Some("Opaque".to_string()),
        data: Some(BTreeMap::from([(
            DATA_KEY.to_string(),
            ByteString(line.render().into_bytes()),
        )])),
        ..Default::default()
    }
}

fn stamp(
    mut annotations: BTreeMap<String, String>,
    requested: Option<&str>,
    now: DateTime<Utc>,
) -> BTreeMap<String, String> {
    if let Some(r) = requested {
        annotations.insert(ROTATE_ANNOTATION.to_string(), r.to_string());
    }
    annotations.insert(ROTATED_AT_ANNOTATION.to_string(), now.to_rfc3339());
    annotations
}

/// Whether a Secret is one this operator created (and so may delete).
pub fn is_ours(secret: &Secret) -> bool {
    secret
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get("app.kubernetes.io/managed-by"))
        .map(String::as_str)
        == Some(FIELD_MANAGER)
}

fn random_key(len: usize) -> Result<Vec<u8>, std::io::Error> {
    let mut key = vec![0u8; len];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    Ok(key)
}

/// Make sure the keyfile Secret exists, and rotate it if the `Network` asks.
/// Returns how long to wait before a held-back rotation can go ahead.
pub async fn reconcile(
    client: &Client,
    cfg: &EffectiveConfig,
    requested: Option<&str>,
) -> Result<Option<Duration>, IpsecError> {
    let api: Api<Secret> = Api::namespaced(client.clone(), NAMESPACE);
    let current = api.get_opt(SECRET).await?;
    let now = Utc::now();

    match plan(current.as_ref().map(existing).as_ref(), requested, now)? {
        Plan::Keep => Ok(None),
        Plan::Wait(d) => {
            info!(secret = SECRET, wait_secs = d.as_secs(), "ipsec key rotation held back until the previous one's window has passed");
            Ok(Some(d))
        }
        Plan::Create => {
            let line = KeyLine::fresh(&random_key(KEY_BYTES)?);
            match api.create(&PostParams::default(), &new_secret(cfg, &line, requested, now)).await {
                Ok(_) => {
                    info!(secret = SECRET, spi = line.spi, "created the ipsec key");
                    Ok(None)
                }
                // Made by someone else since the GET: theirs stands.
                Err(KubeError::Api(e)) if e.code == 409 => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        Plan::Rotate(from) => {
            let mut secret = current.expect("Rotate is only planned for an existing Secret");
            let line = from.rotated(&random_key(from.key_len())?);
            secret
                .data
                .get_or_insert_with(BTreeMap::new)
                .insert(DATA_KEY.to_string(), ByteString(line.render().into_bytes()));
            secret.metadata.annotations =
                Some(stamp(secret.metadata.annotations.take().unwrap_or_default(), requested, now));
            // A replace carries the resourceVersion from the GET, so a
            // concurrent edit fails it rather than being overwritten.
            api.replace(SECRET, &PostParams::default(), &secret).await?;
            info!(secret = SECRET, from_spi = from.spi, spi = line.spi, "rotated the ipsec key");
            Ok(None)
        }
    }
}

/// IPsec is off: delete the keyfile Secret if this operator made it.
pub async fn remove(client: &Client) -> Result<(), IpsecError> {
    let api: Api<Secret> = Api::namespaced(client.clone(), NAMESPACE);
    let Some(secret) = api.get_opt(SECRET).await? else { return Ok(()) };
    if !is_ours(&secret) {
        return Ok(());
    }
    info!(secret = SECRET, "removing — ipsec is no longer the encryption type");
    match api.delete(SECRET, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(KubeError::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;
    use chrono::TimeZone;

    const KEY: [u8; 20] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x01, 0x02, 0x03, 0x04,
    ];

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn a_fresh_key_is_the_line_ciliums_docs_generate() {
        assert_eq!(
            KeyLine::fresh(&KEY).render(),
            "3+ rfc4106(gcm(aes)) 00112233445566778899aabbccddeeff01020304 128\n"
        );
        assert_eq!(KeyLine::fresh(&KEY).key_len(), 20);
    }

    #[test]
    fn spis_run_1_to_15_and_wrap_past_0() {
        assert_eq!(next_spi(3), 4);
        assert_eq!(next_spi(14), 15);
        assert_eq!(next_spi(15), 1);
        for s in 1..=15 {
            assert!((1..=15).contains(&next_spi(s)));
        }
    }

    #[test]
    fn parse_round_trips_and_rotation_keeps_the_shape() {
        let line = KeyLine::fresh(&KEY);
        assert_eq!(KeyLine::parse(&line.render()).unwrap(), line);

        // A hand-made key without per-tunnel keys and with a longer key keeps
        // both through a rotation — only the SPI and the material change.
        let theirs = KeyLine::parse("15 rfc4106(gcm(aes)) 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20212223 128").unwrap();
        assert_eq!(theirs.key_len(), 36);
        let next = theirs.rotated(&[0xab; 36]);
        assert_eq!(next.spi, 1);
        assert!(!next.per_tunnel);
        assert_eq!(next.algorithm, "rfc4106(gcm(aes))");
        assert_eq!(next.icv_bits, 128);
        assert_eq!(next.render(), format!("1 rfc4106(gcm(aes)) {} 128\n", "ab".repeat(36)));
    }

    #[test]
    fn unusable_keyfiles_are_refused_not_guessed() {
        for bad in [
            "",
            "3+ rfc4106(gcm(aes)) 0011 128 extra",
            "0+ rfc4106(gcm(aes)) 0011 128",
            "16 rfc4106(gcm(aes)) 0011 128",
            "3 rfc4106(gcm(aes)) xyz1 128",
            "3 rfc4106(gcm(aes)) 001 128",
            "3 rfc4106(gcm(aes)) 0011 bits",
            // The older auth+enc form: more fields than we rotate.
            "3 hmac(sha256) 0011 cbc(aes) 0011",
        ] {
            assert!(KeyLine::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn the_key_never_reaches_a_log_line() {
        let line = KeyLine::fresh(&KEY);
        let debug = format!("{line:?}");
        assert!(!debug.contains("00112233"), "{debug}");
        assert!(debug.contains("redacted"));
        let err = KeyLine::parse("3 rfc4106(gcm(aes)) 00112233zz 128").unwrap_err().to_string();
        assert!(!err.contains("00112233"), "{err}");
    }

    fn present(handled: Option<&str>, rotated_at: Option<DateTime<Utc>>) -> Existing {
        Existing {
            keys: Some(KeyLine::fresh(&KEY).render()),
            handled: handled.map(str::to_string),
            rotated_at,
        }
    }

    #[test]
    fn a_missing_secret_is_created_and_a_present_one_never_overwritten() {
        assert_eq!(plan(None, None, t(0)).unwrap(), Plan::Create);
        assert_eq!(plan(None, Some("1"), t(0)).unwrap(), Plan::Create);
        // Present, nothing asked: kept, even one with no annotations (made by hand).
        assert_eq!(plan(Some(&Existing::default()), None, t(0)).unwrap(), Plan::Keep);
        assert_eq!(plan(Some(&present(None, None)), None, t(0)).unwrap(), Plan::Keep);
    }

    #[test]
    fn a_rotation_request_is_acted_on_once() {
        let e = present(Some("1"), Some(t(0)));
        assert_eq!(plan(Some(&e), Some("1"), t(10_000)).unwrap(), Plan::Keep);
        match plan(Some(&e), Some("2"), t(10_000)).unwrap() {
            Plan::Rotate(from) => assert_eq!(from.spi, 3),
            other => panic!("expected a rotation, got {other:?}"),
        }
        // A hand-made Secret has handled nothing yet: the first request rotates it.
        assert!(matches!(plan(Some(&present(None, None)), Some("x"), t(0)).unwrap(), Plan::Rotate(_)));
    }

    #[test]
    fn a_second_rotation_waits_out_the_first_ones_window() {
        let e = present(Some("1"), Some(t(0)));
        assert_eq!(plan(Some(&e), Some("2"), t(60)).unwrap(), Plan::Wait(Duration::from_secs(240)));
        assert!(matches!(plan(Some(&e), Some("2"), t(300)).unwrap(), Plan::Rotate(_)));
    }

    #[test]
    fn rotating_needs_a_key_to_rotate_from() {
        let e = Existing { keys: None, handled: Some("1".into()), rotated_at: None };
        assert!(matches!(plan(Some(&e), Some("2"), t(0)), Err(KeyError::Missing)));
        let e = Existing { keys: Some("junk".into()), ..Default::default() };
        assert!(matches!(plan(Some(&e), Some("2"), t(0)), Err(KeyError::Unparsable(_))));
    }

    #[test]
    fn the_new_secret_is_ours_owned_and_records_the_request() {
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.network_uid = Some("uid-1".into());
        let s = new_secret(&cfg, &KeyLine::fresh(&KEY), Some("r1"), t(0));
        assert!(is_ours(&s));
        assert_eq!(s.metadata.name.as_deref(), Some(SECRET));
        assert_eq!(s.metadata.namespace.as_deref(), Some("kube-system"));
        assert_eq!(s.metadata.owner_references.as_ref().unwrap()[0].uid, "uid-1");

        let back = existing(&s);
        assert_eq!(back.handled.as_deref(), Some("r1"));
        assert_eq!(back.rotated_at, Some(t(0)));
        assert_eq!(KeyLine::parse(back.keys.as_deref().unwrap()).unwrap(), KeyLine::fresh(&KEY));
        // So the request that was current at creation does not rotate it again.
        assert_eq!(plan(Some(&back), Some("r1"), t(0)).unwrap(), Plan::Keep);

        let mut hand_made = s.clone();
        hand_made.metadata.labels = None;
        assert!(!is_ours(&hand_made));
    }

    #[test]
    fn random_keys_come_from_the_kernel() {
        let a = random_key(KEY_BYTES).unwrap();
        let b = random_key(KEY_BYTES).unwrap();
        assert_eq!(a.len(), 20);
        assert_ne!(a, b);
    }
}
