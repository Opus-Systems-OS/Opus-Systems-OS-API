//! SQLite for what the API owns: its clients' keys, and the OAuth tokens of
//! the briefing's sources (with the one-time states that start a consent).
//! Everything else it serves is read live from upstream. Same open/migrate shape as
//! Iron-Fleet's control plane so the two are operated the same way.

use crate::auth::keys::{Scope, ScopeSet};
use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS api_keys (
  id             TEXT PRIMARY KEY,   -- the public prefix, e.g. "3f9a1c2b"
  name           TEXT NOT NULL,      -- "mac", "quest-3", "jarvis-ios"
  secret_sha256  TEXT NOT NULL,      -- hex; keys are 256-bit random, no KDF needed
  scopes         TEXT NOT NULL,      -- comma-separated Scope names
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
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct KeyRow {
    pub id: String,
    pub name: String,
    #[serde(skip)]
    pub secret_sha256: String,
    pub scopes: Vec<Scope>,
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
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        Ok(f(&conn)?)
    }

    pub fn insert_key(&self, row: &KeyRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO api_keys (id, name, secret_sha256, scopes, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    row.id,
                    row.name,
                    row.secret_sha256,
                    Scope::join(&row.scopes),
                    row.created_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn key(&self, id: &str) -> Result<Option<KeyRow>> {
        self.with(|c| {
            c.query_row(
                "SELECT id, name, secret_sha256, scopes, created_at, last_used_at, revoked_at
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
                "SELECT id, name, secret_sha256, scopes, created_at, last_used_at, revoked_at
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

    pub fn oauth_token(&self, provider: &str) -> Result<Option<OAuthTokenRow>> {
        self.with(|c| {
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
        })
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
    })
}

/// Columns added after a table first shipped: an `ALTER` guarded by
/// `PRAGMA table_info`, idempotent, no-op on a fresh database.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    const ADDED: &[(&str, &str, &str)] = &[];
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
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }
}
