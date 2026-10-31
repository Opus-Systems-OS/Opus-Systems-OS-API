//! SQLite for what the API owns: its clients' keys, and the OAuth tokens of
//! the briefing's sources (with the one-time states that start a consent).
//! Everything else it serves is read live from upstream. Same open/migrate shape as
//! Iron-Fleet's control plane so the two are operated the same way.

use crate::auth::keys::{Scope, ScopeSet};
use crate::error::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS api_keys (
  id             TEXT PRIMARY KEY,   -- the public prefix, e.g. "3f9a1c2b"
  name           TEXT NOT NULL,      -- "mac", "quest-3", "jarvis-ios"
  secret_sha256  TEXT NOT NULL,      -- hex; keys are 256-bit random, no KDF needed
  scopes         TEXT NOT NULL,      -- comma-separated Scope names
  agents         TEXT,               -- comma-separated agent slugs; NULL = every agent
  created_at     TEXT NOT NULL,
  last_used_at   TEXT,
  revoked_at     TEXT
);
-- One row per provider ("google", "whoop"). The refresh token is replaced
-- whenever the provider sends a new one (WHOOP rotates on every refresh).
CREATE TABLE IF NOT EXISTS oauth_tokens (
  provider       TEXT PRIMARY KEY,
  refresh_token  TEXT NOT NULL,
  access_token   TEXT,
  expires_at     INTEGER,            -- unix seconds
  updated_at     TEXT NOT NULL
);
-- A consent in flight: `opus-api oauth start` writes it, the callback takes
-- it (once, within 15 minutes).
CREATE TABLE IF NOT EXISTS oauth_states (
  state       TEXT PRIMARY KEY,
  provider    TEXT NOT NULL,
  created_at  INTEGER NOT NULL        -- unix seconds
);
"#;

/// A provider's stored tokens.
pub struct OAuthTokenRow {
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub expires_at: Option<i64>,
    pub updated_at: String,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    /// Seals the OAuth tokens at rest; `None` stores them in plaintext.
    token_key: Option<Arc<XChaCha20Poly1305>>,
}

/// The 32-byte key that encrypts OAuth tokens at rest (`TOKEN_KEY_FILE`).
#[derive(Clone)]
pub struct TokenKey([u8; 32]);

impl TokenKey {
    /// 64 hex characters (`openssl rand -hex 32`) or 32 raw bytes.
    pub fn parse(file: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(file).map(str::trim).unwrap_or("");
        let mut key = [0u8; 32];
        if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            for (i, byte) in key.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
            }
        } else if file.len() == 32 {
            key.copy_from_slice(file);
        } else {
            return None;
        }
        Some(TokenKey(key))
    }
}

impl std::fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenKey(<redacted>)")
    }
}

/// Prefix of a sealed token: `v1:` + hex(24-byte nonce || ciphertext).
/// Anything without it is a row written before encryption.
const SEALED_V1: &str = "v1:";

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct KeyRow {
    pub id: String,
    pub name: String,
    #[serde(skip)]
    pub secret_sha256: String,
    pub scopes: Vec<Scope>,
    /// The fleet agents (registry slugs) this key may reach: start, read,
    /// stream and message their sessions, see them in the fleet and usage.
    /// `None` is every agent — every key minted before this column, and
    /// every device key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

impl KeyRow {
    pub fn scope_set(&self) -> ScopeSet {
        ScopeSet::from_iter(self.scopes.iter().copied())
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    rusqlite::Error::InvalidPath(std::path::PathBuf::from(format!(
                        "{}: {e}",
                        parent.display()
                    )))
                })?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        owner_only(path)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            token_key: None,
        })
    }

    /// Encrypt OAuth tokens from here on; plaintext rows are re-sealed the
    /// first time they are read.
    pub fn with_token_key(mut self, key: &TokenKey) -> Self {
        self.token_key = Some(Arc::new(XChaCha20Poly1305::new(&key.0.into())));
        self
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            token_key: None,
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        Ok(f(&conn)?)
    }

    pub fn insert_key(&self, row: &KeyRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO api_keys (id, name, secret_sha256, scopes, agents, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.id,
                    row.name,
                    row.secret_sha256,
                    Scope::join(&row.scopes),
                    row.agents.as_ref().map(|a| a.join(",")),
                    row.created_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn key(&self, id: &str) -> Result<Option<KeyRow>> {
        self.with(|c| {
            c.query_row(
                "SELECT id, name, secret_sha256, scopes, created_at, last_used_at, revoked_at, agents
                 FROM api_keys WHERE id = ?1",
                params![id],
                read_key,
            )
            .optional()
        })
    }

    pub fn keys(&self) -> Result<Vec<KeyRow>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, secret_sha256, scopes, created_at, last_used_at, revoked_at, agents
                 FROM api_keys ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([], read_key)?;
            rows.collect()
        })
    }

    /// Revoke; `Ok(false)` if there is no such key or it was already revoked.
    pub fn revoke_key(&self, id: &str) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE api_keys SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                params![id, now()],
            )?;
            Ok(n == 1)
        })
    }

    /// Add scopes to a live key (never `keys:admin`: minting is the only
    /// path to that). `Ok(None)` if there is no such live key; otherwise
    /// the key's scopes after the grant.
    pub fn grant_scopes(&self, id: &str, add: &[Scope]) -> Result<Option<Vec<Scope>>> {
        let Some(key) = self.key(id)? else {
            return Ok(None);
        };
        if key.revoked_at.is_some() {
            return Ok(None);
        }
        let mut scopes: std::collections::BTreeSet<Scope> = key.scopes.into_iter().collect();
        scopes.extend(add.iter().copied().filter(|s| *s != Scope::KeysAdmin));
        let scopes: Vec<Scope> = scopes.into_iter().collect();
        self.with(|c| {
            c.execute(
                "UPDATE api_keys SET scopes = ?2 WHERE id = ?1",
                params![id, Scope::join(&scopes)],
            )
        })?;
        Ok(Some(scopes))
    }

    /// Replace a live key's agent allowlist (`None` = every agent).
    /// `Ok(false)` if there is no such live key.
    pub fn set_key_agents(&self, id: &str, agents: Option<&[String]>) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE api_keys SET agents = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                params![id, agents.map(|a| a.join(","))],
            )?;
            Ok(n == 1)
        })
    }

    // ---- OAuth (briefing sources)

    pub fn insert_oauth_state(&self, state: &str, provider: &str) -> Result<()> {
        let now = unix_now();
        self.with(|c| {
            // Old states are dead weight; clear them on the way in.
            c.execute(
                "DELETE FROM oauth_states WHERE created_at < ?1",
                params![now - 86_400],
            )?;
            c.execute(
                "INSERT INTO oauth_states (state, provider, created_at) VALUES (?1, ?2, ?3)",
                params![state, provider, now],
            )?;
            Ok(())
        })
    }

    /// The provider a state was issued for, if it exists and is younger
    /// than `max_age_secs`. Taking it deletes it: a state works once.
    pub fn take_oauth_state(&self, state: &str, max_age_secs: i64) -> Result<Option<String>> {
        let now = unix_now();
        self.with(|c| {
            let row: Option<(String, i64)> = c
                .query_row(
                    "SELECT provider, created_at FROM oauth_states WHERE state = ?1",
                    params![state],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            c.execute("DELETE FROM oauth_states WHERE state = ?1", params![state])?;
            Ok(row
                .filter(|(_, at)| now - at <= max_age_secs)
                .map(|(p, _)| p))
        })
    }

    /// A provider's tokens, decrypted. A row from before encryption is
    /// read as plaintext and sealed in place when a key is set.
    pub fn oauth_token(&self, provider: &str) -> Result<Option<OAuthTokenRow>> {
        let stored = self.with(|c| {
            c.query_row(
                "SELECT refresh_token, access_token, expires_at, updated_at FROM oauth_tokens WHERE provider = ?1",
                params![provider],
                |r| {
                    Ok(OAuthTokenRow {
                        refresh_token: r.get(0)?,
                        access_token: r.get(1)?,
                        expires_at: r.get(2)?,
                        updated_at: r.get(3)?,
                    })
                },
            )
            .optional()
        })?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        let legacy = !stored.refresh_token.starts_with(SEALED_V1)
            || stored
                .access_token
                .as_deref()
                .is_some_and(|a| !a.starts_with(SEALED_V1));
        let row = OAuthTokenRow {
            refresh_token: self.open_token(provider, "refresh", &stored.refresh_token)?,
            access_token: stored
                .access_token
                .as_deref()
                .map(|a| self.open_token(provider, "access", a))
                .transpose()?,
            ..stored
        };
        if legacy && self.token_key.is_some() {
            let refresh = self.seal_token(provider, "refresh", &row.refresh_token)?;
            let access = row
                .access_token
                .as_deref()
                .map(|a| self.seal_token(provider, "access", a))
                .transpose()?;
            self.with(|c| {
                c.execute(
                    "UPDATE oauth_tokens SET refresh_token = ?2, access_token = ?3 WHERE provider = ?1",
                    params![provider, refresh, access],
                )
            })?;
            tracing::info!(provider, "plaintext oauth tokens encrypted at rest");
        }
        Ok(Some(row))
    }

    /// Save an access token, and the refresh token when one was issued
    /// (`None` keeps the stored one).
    pub fn store_oauth_token(
        &self,
        provider: &str,
        refresh: Option<&str>,
        access: &str,
        expires_at: i64,
    ) -> Result<()> {
        let access = self.seal_token(provider, "access", access)?;
        let refresh = refresh
            .map(|r| self.seal_token(provider, "refresh", r))
            .transpose()?;
        self.with(|c| {
            match refresh {
                Some(r) => c.execute(
                    "INSERT INTO oauth_tokens (provider, refresh_token, access_token, expires_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(provider) DO UPDATE SET refresh_token = excluded.refresh_token,
                       access_token = excluded.access_token, expires_at = excluded.expires_at,
                       updated_at = excluded.updated_at",
                    params![provider, r, access, expires_at, now()],
                )?,
                None => c.execute(
                    "UPDATE oauth_tokens SET access_token = ?2, expires_at = ?3, updated_at = ?4 WHERE provider = ?1",
                    params![provider, access, expires_at, now()],
                )?,
            };
            Ok(())
        })
    }

    /// `v1:` + hex(nonce || ciphertext). The provider and column are the
    /// associated data, so a sealed value only opens in the cell it came from.
    fn seal_token(&self, provider: &str, column: &str, plain: &str) -> Result<String> {
        let Some(cipher) = &self.token_key else {
            return Ok(plain.to_owned());
        };
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).expect("os randomness");
        let aad = format!("{provider}/{column}");
        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plain.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| Error::Config("could not encrypt an oauth token".into()))?;
        let hex: String = nonce
            .iter()
            .chain(&sealed)
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(format!("{SEALED_V1}{hex}"))
    }

    fn open_token(&self, provider: &str, column: &str, stored: &str) -> Result<String> {
        let Some(hex) = stored.strip_prefix(SEALED_V1) else {
            return Ok(stored.to_owned());
        };
        let cipher = self.token_key.as_ref().ok_or_else(|| {
            Error::Config(format!(
                "{provider}'s oauth tokens are encrypted and TOKEN_KEY_FILE is not set"
            ))
        })?;
        let bad = || {
            Error::Config(format!(
                "{provider}'s stored {column} token does not decrypt"
            ))
        };
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();
        let bytes = bytes.filter(|b| b.len() > 24).ok_or_else(bad)?;
        let (nonce, sealed) = bytes.split_at(24);
        let aad = format!("{provider}/{column}");
        let plain = cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| bad())?;
        String::from_utf8(plain).map_err(|_| bad())
    }

    /// Best-effort, coarse: one write per key per minute at most, so a busy
    /// headset doesn't turn every request into a disk write.
    pub fn touch_key(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE api_keys SET last_used_at = ?2
                 WHERE id = ?1 AND (last_used_at IS NULL OR last_used_at < ?3)",
                params![id, now(), minute_ago()],
            )?;
            Ok(())
        })
    }
}

fn read_key(r: &rusqlite::Row<'_>) -> rusqlite::Result<KeyRow> {
    let scopes: String = r.get(3)?;
    Ok(KeyRow {
        id: r.get(0)?,
        name: r.get(1)?,
        secret_sha256: r.get(2)?,
        scopes: Scope::parse_list(&scopes).unwrap_or_default(),
        created_at: r.get(4)?,
        last_used_at: r.get(5)?,
        revoked_at: r.get(6)?,
        agents: r
            .get::<_, Option<String>>(7)?
            .map(|a| a.split(',').map(str::to_owned).collect()),
    })
}

/// The database and its WAL files readable by this user only: they hold
/// key hashes and OAuth tokens.
fn owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let file = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            if file.exists() {
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| Error::Config(format!("chmod {}: {e}", file.display())))?;
            }
        }
    }
    Ok(())
}

/// Columns added after a table first shipped: an `ALTER` guarded by
/// `PRAGMA table_info`, idempotent, no-op on a fresh database.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    const ADDED: &[(&str, &str, &str)] = &[("api_keys", "agents", "TEXT")];
    for (table, column, ty) in ADDED {
        let present = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|name| name.as_deref() == Ok(column));
        if !present {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
        }
    }
    Ok(())
}

pub fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn minute_ago() -> String {
    (time::OffsetDateTime::now_utc() - time::Duration::seconds(60))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> KeyRow {
        KeyRow {
            id: id.into(),
            name: "test".into(),
            secret_sha256: "ab".repeat(32),
            scopes: vec![Scope::FleetRead, Scope::SessionsWrite],
            agents: None,
            created_at: now(),
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn insert_read_revoke_round_trip() {
        let db = Db::in_memory().unwrap();
        db.insert_key(&row("k1")).unwrap();
        let k = db.key("k1").unwrap().unwrap();
        assert_eq!(k.scopes, vec![Scope::FleetRead, Scope::SessionsWrite]);
        assert!(k.revoked_at.is_none());
        assert!(db.revoke_key("k1").unwrap());
        assert!(!db.revoke_key("k1").unwrap(), "second revoke is a no-op");
        assert!(db.key("k1").unwrap().unwrap().revoked_at.is_some());
        assert!(db.key("nope").unwrap().is_none());
        assert_eq!(db.keys().unwrap().len(), 1);
    }

    #[test]
    fn touch_writes_at_most_once_a_minute() {
        let db = Db::in_memory().unwrap();
        db.insert_key(&row("k1")).unwrap();
        db.touch_key("k1").unwrap();
        let first = db.key("k1").unwrap().unwrap().last_used_at.unwrap();
        db.touch_key("k1").unwrap();
        let second = db.key("k1").unwrap().unwrap().last_used_at.unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn grant_adds_scopes_but_never_admin() {
        let db = Db::in_memory().unwrap();
        db.insert_key(&row("k1")).unwrap();
        let after = db
            .grant_scopes(
                "k1",
                &[Scope::SourcesRead, Scope::KeysAdmin, Scope::FleetRead],
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            after,
            vec![Scope::FleetRead, Scope::SessionsWrite, Scope::SourcesRead]
        );
        assert_eq!(db.key("k1").unwrap().unwrap().scopes, after);
        assert!(db
            .grant_scopes("nope", &[Scope::OpsRead])
            .unwrap()
            .is_none());
        db.revoke_key("k1").unwrap();
        assert!(db.grant_scopes("k1", &[Scope::OpsRead]).unwrap().is_none());
    }

    #[test]
    fn oauth_tokens_keep_the_refresh_token_unless_rotated() {
        let db = Db::in_memory().unwrap();
        db.store_oauth_token("whoop", Some("r1"), "a1", 100)
            .unwrap();
        db.store_oauth_token("whoop", None, "a2", 200).unwrap();
        let t = db.oauth_token("whoop").unwrap().unwrap();
        assert_eq!(
            (
                t.refresh_token.as_str(),
                t.access_token.as_deref(),
                t.expires_at
            ),
            ("r1", Some("a2"), Some(200))
        );
        db.store_oauth_token("whoop", Some("r2"), "a3", 300)
            .unwrap();
        assert_eq!(
            db.oauth_token("whoop").unwrap().unwrap().refresh_token,
            "r2"
        );
        assert!(db.oauth_token("google").unwrap().is_none());
    }

    fn token_key() -> TokenKey {
        TokenKey::parse("ab".repeat(32).as_bytes()).unwrap()
    }

    fn raw_tokens(db: &Db, provider: &str) -> (String, Option<String>) {
        db.with(|c| {
            c.query_row(
                "SELECT refresh_token, access_token FROM oauth_tokens WHERE provider = ?1",
                params![provider],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap()
    }

    #[test]
    fn oauth_tokens_round_trip_encrypted() {
        let db = Db::in_memory().unwrap().with_token_key(&token_key());
        db.store_oauth_token("google", Some("1//refresh"), "ya29.access", 100)
            .unwrap();
        let (refresh, access) = raw_tokens(&db, "google");
        let access = access.unwrap();
        assert!(refresh.starts_with("v1:") && access.starts_with("v1:"));
        assert!(!refresh.contains("refresh") && !access.contains("ya29"));
        let t = db.oauth_token("google").unwrap().unwrap();
        assert_eq!(t.refresh_token, "1//refresh");
        assert_eq!(t.access_token.as_deref(), Some("ya29.access"));

        // Another key does not open it.
        let other = db
            .clone()
            .with_token_key(&TokenKey::parse(&[7u8; 32]).unwrap());
        assert!(other.oauth_token("google").is_err());
        // Nor does the same value moved into another provider's row.
        db.with(|c| {
            c.execute(
                "INSERT INTO oauth_tokens (provider, refresh_token, updated_at) VALUES ('whoop', ?1, 'x')",
                params![refresh],
            )
        })
        .unwrap();
        assert!(db.oauth_token("whoop").is_err());
        // Without a key, a sealed row is an error, not garbage.
        let keyless = Db {
            token_key: None,
            ..db.clone()
        };
        assert!(keyless.oauth_token("google").is_err());
    }

    #[test]
    fn plaintext_tokens_are_sealed_on_first_read() {
        let plain = Db::in_memory().unwrap();
        plain
            .store_oauth_token("whoop", Some("r-plain"), "a-plain", 100)
            .unwrap();
        assert_eq!(
            raw_tokens(&plain, "whoop").0,
            "r-plain",
            "no key: as before"
        );
        let db = plain.with_token_key(&token_key());
        let t = db.oauth_token("whoop").unwrap().unwrap();
        assert_eq!(
            (t.refresh_token.as_str(), t.access_token.as_deref()),
            ("r-plain", Some("a-plain"))
        );
        let (refresh, access) = raw_tokens(&db, "whoop");
        assert!(refresh.starts_with("v1:"), "{refresh}");
        assert!(access.unwrap().starts_with("v1:"));
        let again = db.oauth_token("whoop").unwrap().unwrap();
        assert_eq!(again.refresh_token, "r-plain");
        assert_eq!(again.access_token.as_deref(), Some("a-plain"));
    }

    #[test]
    fn token_key_is_hex_or_raw() {
        assert!(TokenKey::parse(format!("{}\n", "0f".repeat(32)).as_bytes()).is_some());
        assert!(TokenKey::parse(&[9u8; 32]).is_some());
        assert!(TokenKey::parse(b"short").is_none());
        assert!(TokenKey::parse("zz".repeat(32).as_bytes()).is_none());
        assert_eq!(format!("{:?}", token_key()), "TokenKey(<redacted>)");
    }

    #[cfg(unix)]
    #[test]
    fn open_makes_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("opus-api-db-{}", std::process::id()));
        let path = dir.join("t.db");
        Db::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oauth_states_expire() {
        let db = Db::in_memory().unwrap();
        db.insert_oauth_state("s1", "google").unwrap();
        assert_eq!(db.take_oauth_state("s1", -1).unwrap(), None, "too old");
        assert_eq!(
            db.take_oauth_state("s1", 900).unwrap(),
            None,
            "already taken"
        );
    }

    #[test]
    fn agent_allowlist_round_trips_and_resets() {
        let db = Db::in_memory().unwrap();
        db.insert_key(&row("k1")).unwrap();
        assert_eq!(db.key("k1").unwrap().unwrap().agents, None);
        let only = vec!["jarvis-powers".to_owned(), "jarvis-studio".to_owned()];
        assert!(db.set_key_agents("k1", Some(&only)).unwrap());
        assert_eq!(db.key("k1").unwrap().unwrap().agents, Some(only));
        assert!(db.set_key_agents("k1", None).unwrap());
        assert_eq!(db.key("k1").unwrap().unwrap().agents, None);
        assert!(!db.set_key_agents("nope", None).unwrap());
    }

    #[test]
    fn migrate_adds_agents_to_an_old_keys_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE api_keys (id TEXT PRIMARY KEY, name TEXT NOT NULL,
               secret_sha256 TEXT NOT NULL, scopes TEXT NOT NULL, created_at TEXT NOT NULL,
               last_used_at TEXT, revoked_at TEXT);
             INSERT INTO api_keys (id, name, secret_sha256, scopes, created_at)
               VALUES ('old', 'mac', 'ab', 'fleet:read', '2026-09-01T00:00:00Z');",
        )
        .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn).unwrap();
        let agents: Option<String> = conn
            .query_row("SELECT agents FROM api_keys WHERE id = 'old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(agents, None, "an existing key keeps every agent");
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }
}
