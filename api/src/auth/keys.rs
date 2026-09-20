//! API keys: `osk_<id>_<secret>`. `id` is 8 hex chars and is the row's primary
//! key — it is what we look up by and what appears in logs and listings. The
//! secret is 32 random bytes as 64 hex chars, stored only as its SHA-256.
//! A 256-bit random secret does not need a slow KDF; it needs a constant-time
//! comparison, which `verify` does on the hashes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use subtle::ConstantTimeEq;

pub const PREFIX: &str = "osk_";
const ID_BYTES: usize = 4;
const SECRET_BYTES: usize = 32;

/// What a key may do. Deliberately no scope can create agents or
/// environments or change a budget — the API has no such routes at all.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// `GET /v1/fleet/*`, `GET /v1/rig`.
    #[serde(rename = "fleet:read")]
    FleetRead,
    /// Read sessions, their events, and streams.
    #[serde(rename = "sessions:read")]
    SessionsRead,
    /// Start sessions, send messages, interrupt.
    #[serde(rename = "sessions:write")]
    SessionsWrite,
    /// `GET /v1/usage*`.
    #[serde(rename = "usage:read")]
    UsageRead,
    /// `/v1/inference/*` — the rig's local models.
    #[serde(rename = "inference")]
    Inference,
    /// `/v1/voice/*` — Jarvis's voice (text to speech).
    #[serde(rename = "voice")]
    Voice,
    /// `/v1/ops*` — read-only status of the stack's services.
    #[serde(rename = "ops:read")]
    OpsRead,
    /// `POST /v1/pair/{code}/approve` — mint a device key with the fixed
    /// device profile. Held by the Mac, never by a device.
    #[serde(rename = "pair:approve")]
    PairApprove,
    /// Manage keys. Never granted to a device.
    #[serde(rename = "keys:admin")]
    KeysAdmin,
}

impl Scope {
    pub const ALL: [Scope; 9] = [
        Scope::FleetRead,
        Scope::SessionsRead,
        Scope::SessionsWrite,
        Scope::UsageRead,
        Scope::Inference,
        Scope::Voice,
        Scope::OpsRead,
        Scope::PairApprove,
        Scope::KeysAdmin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::FleetRead => "fleet:read",
            Scope::SessionsRead => "sessions:read",
            Scope::SessionsWrite => "sessions:write",
            Scope::UsageRead => "usage:read",
            Scope::Inference => "inference",
            Scope::Voice => "voice",
            Scope::OpsRead => "ops:read",
            Scope::PairApprove => "pair:approve",
            Scope::KeysAdmin => "keys:admin",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|sc| sc.as_str() == s)
    }

    /// Comma-separated, whitespace tolerated; `Err` names the first bad one.
    pub fn parse_list(s: &str) -> Result<Vec<Scope>, String> {
        let mut out = BTreeSet::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            out.insert(Scope::parse(part).ok_or_else(|| format!("unknown scope `{part}`"))?);
        }
        Ok(out.into_iter().collect())
    }

    pub fn join(scopes: &[Scope]) -> String {
        scopes
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeSet(BTreeSet<Scope>);

impl ScopeSet {
    pub fn has(&self, s: Scope) -> bool {
        self.0.contains(&s)
    }
    pub fn iter(&self) -> impl Iterator<Item = Scope> + '_ {
        self.0.iter().copied()
    }
}

impl FromIterator<Scope> for ScopeSet {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        ScopeSet(iter.into_iter().collect())
    }
}

/// A freshly minted key: the plaintext exists only in this value, once.
pub struct Minted {
    pub id: String,
    pub plaintext: String,
    pub secret_sha256: String,
}

pub fn mint() -> Minted {
    let mut id = [0u8; ID_BYTES];
    let mut secret = [0u8; SECRET_BYTES];
    getrandom::fill(&mut id).expect("os randomness");
    getrandom::fill(&mut secret).expect("os randomness");
    let id = hex(&id);
    let secret = hex(&secret);
    Minted {
        plaintext: format!("{PREFIX}{id}_{secret}"),
        secret_sha256: sha256_hex(&secret),
        id,
    }
}

/// Split a presented key into `(id, secret)` without validating the secret.
/// Shape errors are `None` — the caller answers 401 either way, and never
/// says which part was wrong.
pub fn parse(presented: &str) -> Option<(&str, &str)> {
    let rest = presented.strip_prefix(PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    let ok = id.len() == ID_BYTES * 2
        && secret.len() == SECRET_BYTES * 2
        && id.bytes().all(|b| b.is_ascii_hexdigit())
        && secret.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then_some((id, secret))
}

/// Constant-time check of a presented secret against a stored hash.
pub fn verify(secret: &str, stored_sha256_hex: &str) -> bool {
    let presented = sha256_hex(secret);
    presented.len() == stored_sha256_hex.len()
        && bool::from(presented.as_bytes().ct_eq(stored_sha256_hex.as_bytes()))
}

fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_parse_verify() {
        let m = mint();
        assert!(m.plaintext.starts_with("osk_"));
        let (id, secret) = parse(&m.plaintext).expect("well-formed");
        assert_eq!(id, m.id);
        assert!(verify(secret, &m.secret_sha256));
        assert!(!verify(&secret.replace('a', "b"), &m.secret_sha256) || !secret.contains('a'));
        let other = mint();
        assert!(!verify(secret, &other.secret_sha256));
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse("").is_none());
        assert!(parse("osk_").is_none());
        assert!(parse("osk_abcd_1234").is_none());
        assert!(parse("sk-ant-whatever").is_none());
        let m = mint();
        assert!(parse(&m.plaintext[..m.plaintext.len() - 1]).is_none());
        assert!(parse(&format!("{}!", m.plaintext)).is_none());
    }

    #[test]
    fn scopes_parse_and_join() {
        let s = Scope::parse_list(" sessions:write, fleet:read ,fleet:read").unwrap();
        assert_eq!(s, vec![Scope::FleetRead, Scope::SessionsWrite]);
        assert_eq!(Scope::join(&s), "fleet:read,sessions:write");
        assert_eq!(
            Scope::parse_list("fleet:read,admin").unwrap_err(),
            "unknown scope `admin`"
        );
        for sc in Scope::ALL {
            assert_eq!(Scope::parse(sc.as_str()), Some(sc));
            assert_eq!(
                serde_json::to_string(&sc).unwrap(),
                format!("\"{}\"", sc.as_str())
            );
        }
    }
}
