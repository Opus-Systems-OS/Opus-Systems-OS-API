//! SQLite for the one thing the API owns: its clients' keys. Everything else
//! it serves is read live from upstream. Same open/migrate shape as
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
"#;

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
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }
}
