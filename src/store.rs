// Image cache: SQLite holds metadata, bytes live in content-addressed files `blobs/ab/cd/<blake3>`.
//
// Invariant: the files under `blobs/` are exactly the `blobs` rows, and `blobs.refs` counts the entries
// pointing at each row. One writer connection guards every insert, delete and unlink, triggers keep
// `refs` right on every delete path, a file is renamed into place before its row commits and unlinked
// only after its row is gone. A crash can therefore only leave an orphan file, which `reconcile` removes.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Sql(rusqlite::Error),
    Pool(r2d2::Error),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "sqlite: {e}"),
            Self::Pool(e) => write!(f, "sqlite pool: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

impl From<r2d2::Error> for Error {
    fn from(e: r2d2::Error) -> Self {
        Self::Pool(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS blobs (
  hash TEXT PRIMARY KEY,
  size INTEGER NOT NULL,
  refs INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS entries (
  id INTEGER PRIMARY KEY,
  url TEXT NOT NULL,
  key TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('source', 'step', 'variant')),
  hash TEXT NOT NULL REFERENCES blobs (hash),
  content_type TEXT NOT NULL,
  format TEXT NOT NULL,
  origin_format TEXT NOT NULL,
  size INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  hit_at INTEGER NOT NULL,
  UNIQUE (url, key)
);
CREATE INDEX IF NOT EXISTS entries_kind_created ON entries (kind, created_at);
CREATE INDEX IF NOT EXISTS entries_created ON entries (created_at);
CREATE INDEX IF NOT EXISTS entries_hit ON entries (hit_at);
CREATE INDEX IF NOT EXISTS entries_hash ON entries (hash);

CREATE TRIGGER IF NOT EXISTS entries_ref_insert AFTER INSERT ON entries BEGIN
  UPDATE blobs SET refs = refs + 1 WHERE hash = NEW.hash;
END;
CREATE TRIGGER IF NOT EXISTS entries_ref_delete AFTER DELETE ON entries BEGIN
  UPDATE blobs SET refs = refs - 1 WHERE hash = OLD.hash;
END;
CREATE TRIGGER IF NOT EXISTS entries_ref_update AFTER UPDATE OF hash ON entries BEGIN
  UPDATE blobs SET refs = refs - 1 WHERE hash = OLD.hash;
  UPDATE blobs SET refs = refs + 1 WHERE hash = NEW.hash;
END;

CREATE TABLE IF NOT EXISTS referer_daily (
  domain TEXT NOT NULL,
  day TEXT NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY (domain, day)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS domains (
  domain TEXT PRIMARY KEY,
  disabled INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS api_keys (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  prefix TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  read_only INTEGER NOT NULL,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Downloaded or rendered source, shared by all variants of a URL.
    Source,
    /// Raw STEP bytes.
    Step,
    /// A served output: resize, forced format or GLB.
    Variant,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Step => "step",
            Self::Variant => "variant",
        }
    }
}

#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub struct Entry {
    pub id: i64,
    pub url: String,
    pub key: String,
    pub kind: String,
    pub hash: String,
    pub content_type: String,
    /// Format of the stored bytes.
    pub format: String,
    /// Format of the original download (drives the JPEG/PNG fallback encode).
    pub origin_format: String,
    pub size: i64,
    pub created_at: i64,
    pub hit_at: i64,
}

impl Entry {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            url: row.get(1)?,
            key: row.get(2)?,
            kind: row.get(3)?,
            hash: row.get(4)?,
            content_type: row.get(5)?,
            format: row.get(6)?,
            origin_format: row.get(7)?,
            size: row.get(8)?,
            created_at: row.get(9)?,
            hit_at: row.get(10)?,
        })
    }

    /// Quoted strong `ETag` from the content hash.
    pub fn etag(&self) -> String {
        etag(&self.hash)
    }
}

pub fn etag(hash: &str) -> String {
    format!("\"{}\"", &hash[..hash.len().min(32)])
}

pub fn hash(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

const COLUMNS: &str =
    "id, url, key, kind, hash, content_type, format, origin_format, size, created_at, hit_at";

pub struct NewEntry<'a> {
    pub url: &'a str,
    pub key: &'a str,
    pub kind: Kind,
    pub content_type: &'a str,
    pub format: &'a str,
    pub origin_format: &'a str,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    pub orphan_files: usize,
    pub broken_rows: usize,
    pub refs_fixed: usize,
}

pub struct Store {
    db_path: PathBuf,
    blobs: PathBuf,
    write: Mutex<Connection>,
    read: r2d2::Pool<SqliteConnectionManager>,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let db_path = dir.join("cache.db");
        let blobs = dir.join("blobs");
        fs::create_dir_all(blobs.join("incoming"))?;

        let conn = Connection::open(&db_path)?;
        // auto_vacuum only takes effect before the first table exists
        conn.execute_batch(
            "PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;",
        )?;
        conn.execute_batch(SCHEMA)?;

        let manager = SqliteConnectionManager::file(&db_path)
            .with_init(|c| c.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA query_only = ON;"));
        let read = r2d2::Pool::builder().max_size(8).build(manager)?;
        Ok(Self {
            db_path,
            blobs,
            write: Mutex::new(conn),
            read,
        })
    }

    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.blobs.join(&hash[..2]).join(&hash[2..4]).join(hash)
    }

    /// Runs `f` on a read-only connection.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.read.get()?;
        Ok(f(&conn)?)
    }

    /// Runs `f` on the writer connection. Only for tables without files (`referer_daily`, `domains`, `api_keys`).
    pub fn write<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Result<T> {
        let mut conn = self.write.lock();
        Ok(f(&mut conn)?)
    }

    pub fn get(&self, url: &str, key: &str) -> Result<Option<Entry>> {
        self.read(|c| {
            c.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM entries WHERE url = ?1 AND key = ?2"
            ))?
            .query_row(params![url, key], Entry::from_row)
            .optional()
        })
    }

    pub fn get_by_id(&self, id: i64) -> Result<Option<Entry>> {
        self.read(|c| {
            c.prepare_cached(&format!("SELECT {COLUMNS} FROM entries WHERE id = ?1"))?
                .query_row([id], Entry::from_row)
                .optional()
        })
    }

    /// The bytes of an entry; a missing file means the row is broken, so it is dropped and `None` returned.
    pub fn read_bytes(&self, entry: &Entry) -> Result<Option<Vec<u8>>> {
        match fs::read(self.blob_path(&entry.hash)) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.forget_missing(entry)?;
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Drops the rows of a blob whose file vanished, so the read becomes a miss.
    pub fn forget_missing(&self, entry: &Entry) -> Result<()> {
        tracing::warn!(url = %entry.url, key = %entry.key, "cache file missing, dropping entry");
        let mut conn = self.write.lock();
        if !self.blob_path(&entry.hash).exists() {
            self.delete_locked(&mut conn, "hash = ?1", params![entry.hash])?;
        }
        Ok(())
    }

    pub fn put(&self, entry: &NewEntry, data: &[u8]) -> Result<Entry> {
        let hash = hash(data);
        let path = self.blob_path(&hash);
        let mut conn = self.write.lock();

        // A file left by a crash may be cut short; only reuse it when the size matches.
        let reusable = fs::metadata(&path).is_ok_and(|m| m.len() == data.len() as u64);
        if !reusable {
            self.write_file(&path, data)?;
        }

        let now = now();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO blobs (hash, size, refs) VALUES (?1, ?2, 0) ON CONFLICT (hash) DO NOTHING",
            params![hash, data.len() as i64],
        )?;
        let id: i64 = tx.query_row(
            "INSERT INTO entries (url, key, kind, hash, content_type, format, origin_format, size, created_at, hit_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT (url, key) DO UPDATE SET
               kind = excluded.kind, hash = excluded.hash, content_type = excluded.content_type,
               format = excluded.format, origin_format = excluded.origin_format, size = excluded.size,
               created_at = excluded.created_at, hit_at = excluded.hit_at
             RETURNING id",
            params![
                entry.url,
                entry.key,
                entry.kind.as_str(),
                hash,
                entry.content_type,
                entry.format,
                entry.origin_format,
                data.len() as i64,
                now
            ],
            |row| row.get(0),
        )?;
        let dead = Self::take_dead(&tx)?;
        tx.commit()?;
        self.unlink(&dead);

        Ok(Entry {
            id,
            url: entry.url.into(),
            key: entry.key.into(),
            kind: entry.kind.as_str().into(),
            hash,
            content_type: entry.content_type.into(),
            format: entry.format.into(),
            origin_format: entry.origin_format.into(),
            size: data.len() as i64,
            created_at: now,
            hit_at: now,
        })
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> Result<()> {
        let incoming = self.blobs.join("incoming");
        fs::create_dir_all(&incoming)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&incoming)?;
        tmp.write_all(data)?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Removes blob rows nobody points at and returns their hashes, inside the caller's transaction.
    fn take_dead(tx: &rusqlite::Transaction) -> rusqlite::Result<Vec<String>> {
        tx.prepare_cached("DELETE FROM blobs WHERE refs <= 0 RETURNING hash")?
            .query_map([], |row| row.get(0))?
            .collect()
    }

    fn unlink(&self, hashes: &[String]) {
        for hash in hashes {
            match fs::remove_file(self.blob_path(hash)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(%hash, error = %e, "cannot remove cache file"),
            }
        }
    }

    fn delete_locked(
        &self,
        conn: &mut Connection,
        where_sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<usize> {
        let tx = conn.transaction()?;
        let deleted = tx.execute(&format!("DELETE FROM entries WHERE {where_sql}"), params)?;
        let dead = Self::take_dead(&tx)?;
        tx.commit()?;
        self.unlink(&dead);
        if deleted > 100 {
            conn.execute_batch("PRAGMA incremental_vacuum")?;
        }
        Ok(deleted)
    }

    fn delete(&self, where_sql: &str, params: impl rusqlite::Params) -> Result<usize> {
        let mut conn = self.write.lock();
        self.delete_locked(&mut conn, where_sql, params)
    }

    pub fn delete_id(&self, id: i64) -> Result<usize> {
        self.delete("id = ?1", [id])
    }

    pub fn purge_url(&self, url: &str) -> Result<usize> {
        self.delete("url = ?1", [url])
    }

    /// `1h` and `24h` drop what was added in that window, `7d` drops what is older, anything else drops all.
    pub fn clear(&self, period: &str) -> Result<usize> {
        let now = now();
        match period {
            "1h" => self.delete("created_at >= ?1", [now - 3600]),
            "24h" => self.delete("created_at >= ?1", [now - 86_400]),
            "7d" => self.delete("created_at < ?1", [now - 7 * 86_400]),
            _ => self.delete("1 = 1", []),
        }
    }

    /// Sources and raw STEP files only seed variants; drop them after `max_age_secs`.
    pub fn expire_sources(&self, max_age_secs: i64) -> Result<usize> {
        self.delete(
            "kind IN ('source', 'step') AND created_at < ?1",
            [now() - max_age_secs],
        )
    }

    /// Deletes the `batch` least recently hit entries.
    pub fn evict_lru(&self, batch: usize) -> Result<usize> {
        self.delete(
            "id IN (SELECT id FROM entries ORDER BY hit_at ASC, id ASC LIMIT ?1)",
            [batch as i64],
        )
    }

    pub fn touch(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = now();
        let mut conn = self.write.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("UPDATE entries SET hit_at = ?1 WHERE id = ?2")?;
            for id in ids {
                stmt.execute(params![now, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64> {
        self.read(|c| c.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)))
    }

    pub fn blob_bytes(&self) -> Result<u64> {
        let sum: i64 = self
            .read(|c| c.query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |r| r.get(0)))?;
        Ok(sum as u64)
    }

    /// Database files (main + WAL) on disk.
    pub fn db_bytes(&self) -> u64 {
        let wal = self.db_path.with_extension("db-wal");
        [&self.db_path, &wal]
            .iter()
            .filter_map(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .sum()
    }

    pub fn total_bytes(&self) -> Result<u64> {
        Ok(self.blob_bytes()? + self.db_bytes())
    }

    /// Newest first; `page` is clamped to the last page. Returns the rows and the total count.
    pub fn list(
        &self,
        page: i64,
        per_page: i64,
        kind: Option<&str>,
        url: Option<&str>,
    ) -> Result<(Vec<Entry>, i64, i64)> {
        self.read(|c| {
            let total: i64 = c.query_row(
                "SELECT COUNT(*) FROM entries WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR instr(url, ?2) > 0)",
                params![kind, url],
                |r| r.get(0),
            )?;
            let pages = ((total + per_page - 1) / per_page).max(1);
            let page = page.clamp(1, pages);
            let rows = c
                .prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM entries
                     WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR instr(url, ?2) > 0)
                     ORDER BY created_at DESC, id DESC LIMIT ?3 OFFSET ?4"
                ))?
                .query_map(params![kind, url, per_page, (page - 1) * per_page], Entry::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((rows, total, page))
        })
    }

    /// Restores the invariant after a crash or outside tampering: empties `incoming/`, recounts refs,
    /// drops rows whose file is gone or cut short, and removes files no row knows.
    pub fn reconcile(&self) -> Result<ReconcileReport> {
        let mut conn = self.write.lock();
        let mut report = ReconcileReport::default();

        let incoming = self.blobs.join("incoming");
        if incoming.exists() {
            fs::remove_dir_all(&incoming)?;
        }
        fs::create_dir_all(&incoming)?;

        let tx = conn.transaction()?;
        report.refs_fixed = tx.execute(
            "UPDATE blobs SET refs = (SELECT COUNT(*) FROM entries e WHERE e.hash = blobs.hash)
             WHERE refs <> (SELECT COUNT(*) FROM entries e WHERE e.hash = blobs.hash)",
            [],
        )?;
        let rows: Vec<(String, i64)> = tx
            .prepare("SELECT hash, size FROM blobs")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut known = HashSet::with_capacity(rows.len());
        for (hash, size) in rows {
            let intact = fs::metadata(self.blob_path(&hash)).is_ok_and(|m| m.len() == size as u64);
            if intact {
                known.insert(hash);
            } else {
                report.broken_rows += tx.execute("DELETE FROM entries WHERE hash = ?1", [&hash])?;
                tx.execute("DELETE FROM blobs WHERE hash = ?1", [&hash])?;
                let _ = fs::remove_file(self.blob_path(&hash));
            }
        }
        let dead = Self::take_dead(&tx)?;
        tx.commit()?;
        for hash in &dead {
            known.remove(hash);
        }
        self.unlink(&dead);

        for shard in read_dirs(&self.blobs)? {
            if shard.file_name().is_some_and(|n| n == "incoming") {
                continue;
            }
            for sub in read_dirs(&shard)? {
                for file in fs::read_dir(&sub)? {
                    let path = file?.path();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if !known.contains(&name) {
                        if path.is_dir() {
                            fs::remove_dir_all(&path)?;
                        } else {
                            fs::remove_file(&path)?;
                        }
                        report.orphan_files += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Hashes of every blob file on disk, for tests.
    pub fn files_on_disk(&self) -> Result<HashSet<String>> {
        let mut files = HashSet::new();
        for shard in read_dirs(&self.blobs)? {
            if shard.file_name().is_some_and(|n| n == "incoming") {
                continue;
            }
            for sub in read_dirs(&shard)? {
                for file in fs::read_dir(&sub)? {
                    files.insert(file?.file_name().to_string_lossy().into_owned());
                }
            }
        }
        Ok(files)
    }
}

fn read_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for item in fs::read_dir(dir)? {
        let path = item?.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn put(store: &Store, url: &str, key: &str, data: &[u8]) -> Entry {
        store
            .put(
                &NewEntry {
                    url,
                    key,
                    kind: Kind::Variant,
                    content_type: "image/png",
                    format: "png",
                    origin_format: "png",
                },
                data,
            )
            .unwrap()
    }

    /// Files on disk are exactly the blob rows, and refs match the entries.
    fn assert_invariant(store: &Store) {
        let rows: HashSet<String> = store
            .read(|c| {
                c.prepare("SELECT hash FROM blobs")?
                    .query_map([], |r| r.get(0))?
                    .collect()
            })
            .unwrap();
        assert_eq!(store.files_on_disk().unwrap(), rows, "files == blob rows");
        let bad: i64 = store
            .read(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM blobs WHERE refs <> (SELECT COUNT(*) FROM entries e WHERE e.hash = blobs.hash) OR refs <= 0",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(bad, 0, "refs match entries");
    }

    #[test]
    fn put_get_replace_delete() {
        let (_dir, store) = store();
        let a = put(&store, "u", "k", b"one");
        assert_eq!(store.get("u", "k").unwrap().unwrap().hash, a.hash);
        assert_eq!(store.read_bytes(&a).unwrap().unwrap(), b"one");
        assert_invariant(&store);

        let b = put(&store, "u", "k", b"two");
        assert_eq!(a.id, b.id);
        assert!(
            !store.blob_path(&a.hash).exists(),
            "replaced bytes are gone"
        );
        assert_invariant(&store);

        put(&store, "u", "k2", b"two");
        assert_eq!(
            store.files_on_disk().unwrap().len(),
            1,
            "same bytes share one file"
        );
        store.delete_id(b.id).unwrap();
        assert!(store.blob_path(&b.hash).exists(), "still used by k2");
        store.purge_url("u").unwrap();
        assert!(store.files_on_disk().unwrap().is_empty());
        assert_invariant(&store);
    }

    #[test]
    fn random_operations_keep_the_invariant() {
        let (_dir, store) = store();
        let mut seed = 7u64;
        let mut next = |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        for _ in 0..400 {
            let url = format!("u{}", next(5));
            let key = format!("k{}", next(4));
            match next(10) {
                0..=5 => {
                    put(&store, &url, &key, format!("data{}", next(6)).as_bytes());
                }
                6 => {
                    store.purge_url(&url).unwrap();
                }
                7 => {
                    store.evict_lru(2).unwrap();
                }
                8 => {
                    if let Some(e) = store.get(&url, &key).unwrap() {
                        store.delete_id(e.id).unwrap();
                    }
                }
                _ => {
                    store.clear(if next(2) == 0 { "7d" } else { "1h" }).unwrap();
                }
            }
            assert_invariant(&store);
        }
    }

    #[test]
    fn reconcile_repairs_crashes_and_tampering() {
        let (_dir, store) = store();
        let kept = put(&store, "u", "kept", b"kept");
        let lost = put(&store, "u", "lost", b"lost");
        let cut = put(&store, "u", "cut", b"cut-short");

        // crash after rename, before commit: orphan file
        let orphan = hash(b"orphan");
        store
            .write_file(&store.blob_path(&orphan), b"orphan")
            .unwrap();
        // crash mid-write: leftover temp file
        fs::write(store.blobs.join("incoming").join("tmp123"), b"x").unwrap();
        // file deleted from outside, file truncated on power loss
        fs::remove_file(store.blob_path(&lost.hash)).unwrap();
        fs::write(store.blob_path(&cut.hash), b"cut").unwrap();
        // refs drift
        store
            .write(|c| c.execute("UPDATE blobs SET refs = 9 WHERE hash = ?1", [&kept.hash]))
            .unwrap();

        let report = store.reconcile().unwrap();
        assert_eq!(
            report,
            ReconcileReport {
                orphan_files: 1,
                broken_rows: 2,
                refs_fixed: 1
            }
        );
        assert!(store.get("u", "lost").unwrap().is_none());
        assert!(store.get("u", "cut").unwrap().is_none());
        assert_eq!(store.read_bytes(&kept).unwrap().unwrap(), b"kept");
        assert_eq!(
            fs::read_dir(store.blobs.join("incoming")).unwrap().count(),
            0
        );
        assert_invariant(&store);
    }

    #[test]
    fn missing_file_on_read_becomes_a_miss() {
        let (_dir, store) = store();
        let e = put(&store, "u", "k", b"gone");
        fs::remove_file(store.blob_path(&e.hash)).unwrap();
        assert!(store.read_bytes(&e).unwrap().is_none());
        assert!(store.get("u", "k").unwrap().is_none());
        assert_invariant(&store);
    }

    #[test]
    fn truncated_leftover_is_rewritten_on_put() {
        let (_dir, store) = store();
        let h = hash(b"full-bytes");
        store.write_file(&store.blob_path(&h), b"full").unwrap();
        let e = put(&store, "u", "k", b"full-bytes");
        assert_eq!(store.read_bytes(&e).unwrap().unwrap(), b"full-bytes");
    }

    #[test]
    fn expiry_eviction_and_listing() {
        let (_dir, store) = store();
        let src = store
            .put(
                &NewEntry {
                    url: "u",
                    key: "source",
                    kind: Kind::Source,
                    content_type: "image/webp",
                    format: "webp",
                    origin_format: "jpeg",
                },
                b"s",
            )
            .unwrap();
        put(&store, "u", "w_1_avif", b"v1");
        put(&store, "u", "w_2_avif", b"v2");
        store
            .write(|c| {
                c.execute(
                    "UPDATE entries SET created_at = created_at - 90000 WHERE id = ?1",
                    [src.id],
                )
            })
            .unwrap();
        assert_eq!(store.expire_sources(86_400).unwrap(), 1);

        let first = store.get("u", "w_1_avif").unwrap().unwrap();
        store
            .write(|c| c.execute("UPDATE entries SET hit_at = 0 WHERE key = 'w_2_avif'", []))
            .unwrap();
        store.touch(&[first.id]).unwrap();
        assert_eq!(store.evict_lru(1).unwrap(), 1);
        assert!(
            store.get("u", "w_2_avif").unwrap().is_none(),
            "least recently hit goes first"
        );

        let (rows, total, page) = store.list(9, 50, None, Some("u")).unwrap();
        assert_eq!((rows.len(), total, page), (1, 1, 1));
        assert_eq!(store.list(1, 50, Some("source"), None).unwrap().1, 0);
        assert_invariant(&store);
    }
}
