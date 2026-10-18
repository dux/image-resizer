// API keys: `irk_<random>`, stored as SHA-256, all hashes held in memory. A read-only key may only call
// actions that change nothing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::store::{self, Store, now};

const PREFIX: &str = "irk_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRef {
    pub id: i64,
    pub read_only: bool,
}

#[derive(Serialize, schemars::JsonSchema, Clone, Debug)]
pub struct KeyView {
    pub id: i64,
    pub name: String,
    /// First characters of the token, to tell keys apart.
    pub prefix: String,
    pub read_only: bool,
    pub created_by: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.trim().as_bytes()))
}

pub struct Keys {
    by_hash: ArcSwap<HashMap<String, KeyRef>>,
    used: Mutex<HashSet<i64>>,
}

impl Keys {
    pub fn load(store: &Store) -> store::Result<Self> {
        let keys = Self {
            by_hash: ArcSwap::from_pointee(HashMap::new()),
            used: Mutex::default(),
        };
        keys.reload(store)?;
        Ok(keys)
    }

    fn reload(&self, store: &Store) -> store::Result<()> {
        let map = store.read(|c| {
            c.prepare("SELECT token_hash, id, read_only FROM api_keys")?
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        KeyRef {
                            id: r.get(1)?,
                            read_only: r.get(2)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<HashMap<String, KeyRef>>>()
        })?;
        self.by_hash.store(Arc::new(map));
        Ok(())
    }

    pub fn find(&self, token: &str) -> Option<KeyRef> {
        let found = self.by_hash.load().get(&token_hash(token)).copied();
        if let Some(key) = found {
            self.used.lock().insert(key.id);
        }
        found
    }

    /// Creates a key and returns it with the token, which is never shown again.
    pub fn create(
        &self,
        store: &Store,
        name: &str,
        read_only: bool,
        created_by: &str,
    ) -> store::Result<(KeyView, String)> {
        let token = format!(
            "{PREFIX}{}",
            URL_SAFE_NO_PAD.encode(crate::auth::random_bytes::<32>())
        );
        let prefix: String = token.chars().take(PREFIX.len() + 6).collect();
        let created_at = now();
        let id =
            store.write(|c| {
                c.query_row(
                "INSERT INTO api_keys (name, prefix, token_hash, read_only, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
                params![name, prefix, token_hash(&token), read_only, created_by, created_at],
                |r| r.get(0),
            )
            })?;
        self.reload(store)?;
        let view = KeyView {
            id,
            name: name.into(),
            prefix,
            read_only,
            created_by: created_by.into(),
            created_at,
            last_used_at: None,
        };
        Ok((view, token))
    }

    pub fn delete(&self, store: &Store, id: i64) -> store::Result<bool> {
        let deleted = store.write(|c| c.execute("DELETE FROM api_keys WHERE id = ?1", [id]))? > 0;
        self.reload(store)?;
        Ok(deleted)
    }

    pub fn list(&self, store: &Store) -> store::Result<Vec<KeyView>> {
        store.read(|c| {
            c.prepare("SELECT id, name, prefix, read_only, created_by, created_at, last_used_at FROM api_keys ORDER BY id DESC")?
                .query_map([], |r| {
                    Ok(KeyView {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        prefix: r.get(2)?,
                        read_only: r.get(3)?,
                        created_by: r.get(4)?,
                        created_at: r.get(5)?,
                        last_used_at: r.get(6)?,
                    })
                })?
                .collect()
        })
    }

    pub fn get(&self, store: &Store, id: i64) -> store::Result<Option<String>> {
        store.read(|c| {
            c.query_row("SELECT name FROM api_keys WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
        })
    }

    pub fn flush(&self, store: &Store) -> store::Result<()> {
        let ids: Vec<i64> = std::mem::take(&mut *self.used.lock()).into_iter().collect();
        if ids.is_empty() {
            return Ok(());
        }
        let now = now();
        store.write(|c| {
            let tx = c.transaction()?;
            {
                let mut stmt =
                    tx.prepare_cached("UPDATE api_keys SET last_used_at = ?1 WHERE id = ?2")?;
                for id in &ids {
                    stmt.execute(params![now, id])?;
                }
            }
            tx.commit()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_find_flush_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let keys = Keys::load(&store).unwrap();
        let (view, token) = keys.create(&store, "ci", true, "rejotl@gmail.com").unwrap();
        assert!(token.starts_with("irk_") && token.len() > 40);
        assert!(token.starts_with(&view.prefix));
        assert_eq!(
            keys.find(&token),
            Some(KeyRef {
                id: view.id,
                read_only: true
            })
        );
        assert_eq!(keys.find("irk_wrong"), None);
        keys.flush(&store).unwrap();
        assert!(keys.list(&store).unwrap()[0].last_used_at.is_some());
        assert_eq!(
            Keys::load(&store).unwrap().find(&token).map(|k| k.id),
            Some(view.id),
            "survives restart"
        );
        assert!(keys.delete(&store, view.id).unwrap());
        assert_eq!(keys.find(&token), None);
    }
}
