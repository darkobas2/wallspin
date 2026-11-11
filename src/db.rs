use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

/// Small SQLite store. Each task owns its own connection (WAL mode).
pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS history (id INTEGER PRIMARY KEY, path TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS banned  (key TEXT PRIMARY KEY, ts INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS origin  (path TEXT PRIMARY KEY, origin TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS state   (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    /// A history entry is a frame: one image per monitor, primary first ('\n'-joined).
    pub fn push_history(&self, frame: &[PathBuf]) -> Result<()> {
        let joined = frame.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n");
        self.conn.execute("INSERT INTO history (path, ts) VALUES (?1, ?2)", params![joined, now()])?;
        // Keep the table bounded.
        self.conn.execute("DELETE FROM history WHERE id <= (SELECT MAX(id) FROM history) - 2000", [])?;
        Ok(())
    }

    /// Most recent `n` frames, oldest first.
    pub fn recent_history(&self, n: usize) -> Result<Vec<Vec<PathBuf>>> {
        let mut st = self.conn.prepare("SELECT path FROM history ORDER BY id DESC LIMIT ?1")?;
        let mut v: Vec<Vec<PathBuf>> =
            st.query_map([n as i64], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).map(|s| split_frame(&s)).collect();
        v.reverse();
        Ok(v)
    }

    /// Images currently on screen (all monitors).
    pub fn last_frame(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .conn
            .query_row("SELECT path FROM history ORDER BY id DESC LIMIT 1", [], |r| r.get::<_, String>(0))
            .optional()?
            .map(|s| split_frame(&s))
            .unwrap_or_default())
    }

    pub fn ban(&self, key: &str) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO banned (key, ts) VALUES (?1, ?2)", params![key, now()])?;
        Ok(())
    }

    pub fn is_banned(&self, key: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM banned WHERE key = ?1", [key], |_| Ok(())).optional()?.is_some())
    }

    pub fn set_origin(&self, path: &Path, origin: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO origin (path, origin) VALUES (?1, ?2)",
            params![path.to_string_lossy(), origin],
        )?;
        Ok(())
    }

    /// Where an image came from (page URL), if known. Falls back to Wallhaven's file naming.
    pub fn origin(&self, path: &Path) -> Result<Option<String>> {
        let o: Option<String> = self
            .conn
            .query_row("SELECT origin FROM origin WHERE path = ?1", [path.to_string_lossy()], |r| r.get(0))
            .optional()?;
        Ok(o.or_else(|| wallhaven_origin(path)))
    }

    pub fn forget_path(&self, path: &Path) -> Result<()> {
        self.conn.execute("DELETE FROM origin WHERE path = ?1", [path.to_string_lossy()])?;
        Ok(())
    }

    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM state WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_state(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(v) => self.conn.execute("INSERT OR REPLACE INTO state (key, value) VALUES (?1, ?2)", [key, v])?,
            None => self.conn.execute("DELETE FROM state WHERE key = ?1", [key])?,
        };
        Ok(())
    }

    pub fn transaction<T>(&mut self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let tx = self.conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

fn split_frame(s: &str) -> Vec<PathBuf> {
    s.split('\n').filter(|p| !p.is_empty()).map(PathBuf::from).collect()
}

/// `wallhaven-abc123.jpg` -> `https://wallhaven.cc/w/abc123`
pub fn wallhaven_origin(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let id = stem.strip_prefix("wallhaven-")?;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| format!("https://wallhaven.cc/w/{id}"))
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
