//! Local persistence: peers, jobs, results, settings. SQLite, single file,
//! inside the app data directory. Nothing leaves this machine on its own.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rootmode_core::{JobKind, JobPayload, JobStatus, ModelDescriptor};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;

/// Reserved endpoint of the in-process mock worker (dev mode, no GPU needed).
pub const MOCK_ENDPOINT: &str = "mock://local";
pub const MOCK_PEER_ID: &str = "mock";

pub const SOURCE_MANUAL: &str = "manual";
pub const SOURCE_DISCOVERED: &str = "discovered";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// Local row id. Stable across renames and re-announces.
    pub id: String,
    pub label: String,
    /// ISO 3166-1 alpha-2, as the worker declared it. `None` means it did not
    /// say — which is shown as "not stated", never guessed at from the address.
    pub country: Option<String>,
    /// Where this peer asked to be paid. Named by the worker; locking funds
    /// for a priced job uses this address, not a baked-in chain config.
    pub payout: Option<String>,
    /// `ws://host:port/path`, `wss://…`, or [`MOCK_ENDPOINT`].
    pub endpoint: String,
    /// Expected ed25519 public key, if the user pinned one. When set, an
    /// announce from a different key is treated as a mismatch, not a rename.
    pub public_key: Option<String>,
    /// Peer id last announced by the endpoint.
    pub peer_id: Option<String>,
    /// "online" | "offline" | "unknown" | "mismatch"
    pub status: String,
    pub latency_ms: Option<u32>,
    pub caps: Vec<String>,
    pub models: Vec<ModelDescriptor>,
    pub max_concurrent: u32,
    pub last_seen: Option<i64>,
    pub last_error: Option<String>,
    /// "manual" (you typed it) or "discovered" (found on the network). Shown
    /// in the UI, because a peer you found is not a peer you trust.
    pub source: String,
    pub added_at: i64,
}

impl Peer {
    pub fn is_mock(&self) -> bool {
        self.endpoint == MOCK_ENDPOINT
    }

    pub fn is_discovered(&self) -> bool {
        self.source == SOURCE_DISCOVERED
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub job_id: Uuid,
    /// The chat this job answers, when it came from one. Persisted so the
    /// reply is filed by the job pipeline rather than by the UI.
    #[serde(default)]
    pub conversation_id: Option<String>,
    pub peer_id: String,
    pub peer_label: String,
    pub kind: JobKind,
    pub payload: JobPayload,
    pub summary: String,
    pub model: String,
    pub status: JobStatus,
    pub progress: f32,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultRecord {
    pub job_id: Uuid,
    pub kind: JobKind,
    pub sha256: String,
    pub text: Option<String>,
    pub image_path: Option<String>,
    pub meta: serde_json::Value,
    pub created_at: i64,
}

/// A chat. Grouping messages is what makes the app feel like somewhere you
/// come back to, rather than a form you submit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    /// "llm" or "image" — image generations get their own history too.
    pub kind: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// First line of the last message, for the list.
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub message_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub job_id: Option<String>,
    pub sha256: Option<String>,
    pub model: Option<String>,
    /// Which peer answered, so "who ran this" is recoverable later.
    pub peer: Option<String>,
    /// Tokens the provider reported for this answer, when it reported any.
    pub tokens: Option<u64>,
    /// What this answer actually cost, in millionths of a USDC. `Some(0)` is a
    /// priced job that billed nothing; `None` is a free provider or a reply
    /// from before costs were recorded — unmeasured, not zero.
    #[serde(default)]
    pub cost_micros: Option<u64>,
    /// What the model said to itself before answering, when it said anything.
    #[serde(default)]
    pub thinking: Option<String>,
    pub created_at: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        };
        db.migrate()?;
        // The built-in fake worker is a development tool. It is not seeded,
        // and any leftover row is removed, because a product that lists a
        // pretend provider alongside real ones is lying to the person using it.
        // Opt in with ROOTMODE_MOCK=1 or Settings → "Show local mock worker".
        let mock = std::env::var("ROOTMODE_MOCK").is_ok()
            || db
                .get_setting("mock_worker")
                .ok()
                .flatten()
                .as_deref()
                == Some("true");
        if mock {
            db.enable_mock_peer()?;
        } else {
            db.remove_mock_peer()?;
        }
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS peers (
                id             TEXT PRIMARY KEY,
                label          TEXT NOT NULL,
                endpoint       TEXT NOT NULL UNIQUE,
                public_key     TEXT,
                peer_id        TEXT,
                status         TEXT NOT NULL DEFAULT 'unknown',
                latency_ms     INTEGER,
                caps           TEXT NOT NULL DEFAULT '[]',
                models         TEXT NOT NULL DEFAULT '[]',
                max_concurrent INTEGER NOT NULL DEFAULT 1,
                last_seen      INTEGER,
                last_error     TEXT,
                added_at       INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS jobs (
                job_id     TEXT PRIMARY KEY,
                peer_id    TEXT NOT NULL,
                kind       TEXT NOT NULL,
                payload    TEXT NOT NULL,
                status     TEXT NOT NULL,
                progress   REAL NOT NULL DEFAULT 0,
                error      TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS jobs_created_at ON jobs (created_at DESC);

            CREATE TABLE IF NOT EXISTS results (
                job_id     TEXT PRIMARY KEY,
                kind       TEXT NOT NULL,
                sha256     TEXT NOT NULL,
                text       TEXT,
                image_path TEXT,
                meta       TEXT NOT NULL DEFAULT '{}',
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS conversations (
                id         TEXT PRIMARY KEY,
                title      TEXT NOT NULL,
                kind       TEXT NOT NULL DEFAULT 'llm',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                role            TEXT NOT NULL,
                content         TEXT NOT NULL,
                job_id          TEXT,
                sha256          TEXT,
                model           TEXT,
                peer            TEXT,
                tokens          INTEGER,
                thinking        TEXT,
                created_at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS messages_by_conversation
                ON messages (conversation_id, id);

            CREATE TABLE IF NOT EXISTS settings (
                k TEXT PRIMARY KEY,
                v TEXT NOT NULL
            );

            -- One row per bill a priced provider charged the pot, written by
            -- the payment pipeline itself. Chat replies and jobs from
            -- connected tools (the local endpoint) both land here — this is
            -- the ledger the user audits their spend against, so it cannot
            -- depend on a conversation existing.
            CREATE TABLE IF NOT EXISTS spends (
                job_id      TEXT PRIMARY KEY,
                model       TEXT NOT NULL,
                peer        TEXT,
                tokens      INTEGER,
                cost_micros INTEGER NOT NULL,
                at          INTEGER NOT NULL
            );

            -- The Pot's Settled events for this wallet: which on-chain
            -- transaction collected which cumulative ticket. Joined to spends
            -- by cumulative, so every reply can point at the tx that took
            -- its money — even when one tx collected several replies.
            CREATE TABLE IF NOT EXISTS settlements (
                tx_hash        TEXT PRIMARY KEY,
                payout         TEXT NOT NULL,
                cumulative     INTEGER NOT NULL,
                block          INTEGER NOT NULL,
                paid_to_worker INTEGER NOT NULL DEFAULT 0,
                fee            INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS settlements_by_channel ON settlements (payout, cumulative);

            -- Written when MetaMask confirms a deposit, not by scanning the chain.
            CREATE TABLE IF NOT EXISTS deposits (
                tx_hash              TEXT PRIMARY KEY,
                amount_micros        INTEGER NOT NULL,
                max_per_job_micros   INTEGER NOT NULL,
                max_per_day_micros   INTEGER NOT NULL,
                block                INTEGER NOT NULL DEFAULT 0,
                at                   INTEGER NOT NULL,
                chain_id             INTEGER NOT NULL DEFAULT 0,
                client               TEXT NOT NULL DEFAULT ''
            );
            "#,
        )?;

        // `CREATE TABLE IF NOT EXISTS` does nothing to a table that already
        // exists, so columns added after a release need adding by hand.
        // Existing rows keep the default, which is the honest answer for data
        // recorded before we measured it.
        self.add_column(&conn, "peers", "source", "TEXT NOT NULL DEFAULT 'manual'")?;
        self.add_column(&conn, "peers", "country", "TEXT")?;
        self.add_column(&conn, "peers", "payout", "TEXT")?;
        self.add_column(&conn, "messages", "tokens", "INTEGER")?;
        self.add_column(&conn, "messages", "thinking", "TEXT")?;
        // What the job behind a reply was actually billed, in µUSDC. This is
        // the user's money; a wallet page that shows tokens but not dollars
        // is not auditable by the person paying.
        self.add_column(&conn, "messages", "cost_micros", "INTEGER")?;
        // Which signed ticket carried a reply's charge, and on which payout
        // channel. This is what ties a reply to the settle tx that collected it.
        self.add_column(&conn, "spends", "cumulative_micros", "INTEGER")?;
        self.add_column(&conn, "spends", "payout", "TEXT")?;
        // A reply that ended without a bill — stopped, or the stream broke —
        // may still cost its prepaid chunk: the worker keeps it. Whether it
        // did is settled by the chain (a settle of exactly the bond ticket),
        // so the row carries the bond and the chunk, and resolves later.
        self.add_column(&conn, "spends", "abandoned", "INTEGER NOT NULL DEFAULT 0")?;
        self.add_column(&conn, "spends", "bond_cumulative", "INTEGER")?;
        self.add_column(&conn, "spends", "chunk_micros", "INTEGER")?;
        // Which conversation, if any, a job belongs to. Without it a reply
        // can only be filed by whatever screen happened to be open when the
        // job finished — and if that screen was closed, the answer is lost.
        self.add_column(&conn, "jobs", "conversation_id", "TEXT")?;

        Ok(())
    }

    /// Add a column unless it is already there. Sqlite has no
    /// `ADD COLUMN IF NOT EXISTS`, and the only way to ask is to try.
    fn add_column(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<()> {
        match conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        ) {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("duplicate column") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------- peers

    /// Opt in with `ROOTMODE_MOCK=1` — lets you exercise the UI with no GPU
    /// and no network.
    pub fn enable_mock_peer(&self) -> Result<()> {
        let models = serde_json::to_string(&crate::mock::MockTransport::announce().models)?;
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO peers
               (id, label, endpoint, public_key, peer_id, status, caps, models, max_concurrent, added_at)
               VALUES (?1, ?2, ?3, NULL, ?4, 'online', ?5, ?6, 2, ?7)
               ON CONFLICT(id) DO UPDATE SET
                 label = excluded.label,
                 status = 'online',
                 caps = excluded.caps,
                 models = excluded.models"#,
            params![
                MOCK_PEER_ID,
                "mock worker (local)",
                MOCK_ENDPOINT,
                MOCK_PEER_ID,
                r#"["llm","image","video"]"#,
                models,
                now(),
            ],
        )?;
        Ok(())
    }

    pub fn remove_mock_peer(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM peers WHERE endpoint = ?1",
            params![MOCK_ENDPOINT],
        )?;
        Ok(())
    }

    /// Forget discovered peers that stopped answering.
    ///
    /// A peer id is per-installation, so a worker that is recreated without
    /// its key arrives as a new peer and the old one lingers forever. Anything
    /// genuinely alive is rediscovered within seconds, so forgetting is cheap
    /// and a list of dead entries is not.
    pub fn prune_dead_discovered(&self, silent_for: Duration) -> Result<usize> {
        let cutoff = now() - silent_for.as_secs() as i64;
        let conn = self.lock();
        let removed = conn.execute(
            r#"DELETE FROM peers
               WHERE source = ?1
                 AND status <> 'online'
                 AND (last_seen IS NULL OR last_seen < ?2)"#,
            params![SOURCE_DISCOVERED, cutoff],
        )?;
        Ok(removed)
    }

    pub fn list_peers(&self) -> Result<Vec<Peer>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, label, endpoint, public_key, peer_id, status, latency_ms,
                      caps, models, max_concurrent, last_seen, last_error, added_at, source, country, payout
               FROM peers ORDER BY (endpoint = ?1) DESC, added_at ASC"#,
        )?;
        let rows = stmt.query_map(params![MOCK_ENDPOINT], row_to_peer)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_peer(&self, id: &str) -> Result<Option<Peer>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, label, endpoint, public_key, peer_id, status, latency_ms,
                      caps, models, max_concurrent, last_seen, last_error, added_at, source, country, payout
               FROM peers WHERE id = ?1"#,
        )?;
        Ok(stmt.query_row(params![id], row_to_peer).optional()?)
    }

    pub fn add_peer(&self, label: &str, endpoint: &str, public_key: Option<&str>) -> Result<Peer> {
        self.insert_peer(label, endpoint, public_key, SOURCE_MANUAL)
    }

    fn insert_peer(
        &self,
        label: &str,
        endpoint: &str,
        public_key: Option<&str>,
        source: &str,
    ) -> Result<Peer> {
        let id = Uuid::new_v4().to_string();
        {
            let conn = self.lock();
            conn.execute(
                r#"INSERT INTO peers (id, label, endpoint, public_key, status, added_at, source)
                   VALUES (?1, ?2, ?3, ?4, 'unknown', ?5, ?6)"#,
                params![id, label, endpoint, public_key, now(), source],
            )?;
        }
        Ok(self.get_peer(&id)?.expect("just inserted"))
    }

    /// Record a peer found on the network. Returns the existing row if this
    /// endpoint is already known — a peer you added by hand does not become a
    /// stranger because discovery also found it.
    pub fn upsert_discovered_peer(&self, label: &str, endpoint: &str) -> Result<Peer> {
        match self.peer_by_endpoint(endpoint)? {
            Some(existing) => Ok(existing),
            None => self.insert_peer(label, endpoint, None, SOURCE_DISCOVERED),
        }
    }

    pub fn peer_by_endpoint(&self, endpoint: &str) -> Result<Option<Peer>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, label, endpoint, public_key, peer_id, status, latency_ms,
                      caps, models, max_concurrent, last_seen, last_error, added_at, source, country, payout
               FROM peers WHERE endpoint = ?1"#,
        )?;
        Ok(stmt.query_row(params![endpoint], row_to_peer).optional()?)
    }

    pub fn rename_peer(&self, id: &str, label: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE peers SET label = ?2 WHERE id = ?1",
            params![id, label.chars().take(40).collect::<String>()],
        )?;
        Ok(())
    }

    pub fn remove_peer(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM peers WHERE id = ?1 AND endpoint <> ?2",
            params![id, MOCK_ENDPOINT],
        )?;
        Ok(())
    }

    /// Record the outcome of a reachability probe.
    #[allow(clippy::too_many_arguments)]
    pub fn update_peer_status(
        &self,
        id: &str,
        status: &str,
        latency_ms: Option<u32>,
        peer_id: Option<&str>,
        caps: Option<&[String]>,
        models: Option<&[ModelDescriptor]>,
        max_concurrent: Option<u32>,
        country: Option<&str>,
        last_error: Option<&str>,
        payout: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"UPDATE peers SET
                 status         = ?2,
                 latency_ms     = ?3,
                 peer_id        = COALESCE(?4, peer_id),
                 caps           = COALESCE(?5, caps),
                 models         = COALESCE(?6, models),
                 max_concurrent = COALESCE(?7, max_concurrent),
                 country        = COALESCE(?8, country),
                 last_error     = ?9,
                 payout         = COALESCE(?11, payout),
                 last_seen      = CASE WHEN ?2 = 'online' THEN ?10 ELSE last_seen END
               WHERE id = ?1"#,
            params![
                id,
                status,
                latency_ms,
                peer_id,
                caps.map(|c| serde_json::to_string(c).unwrap_or_else(|_| "[]".into())),
                models.map(|m| serde_json::to_string(m).unwrap_or_else(|_| "[]".into())),
                max_concurrent,
                country,
                last_error,
                now(),
                payout,
            ],
        )?;
        Ok(())
    }

    // -------------------------------------------------------------- jobs

    pub fn insert_job(&self, job: &JobRecord) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO jobs (job_id, conversation_id, peer_id, kind, payload, status, progress, error, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
            params![
                job.job_id.to_string(),
                job.conversation_id,
                job.peer_id,
                job.kind.as_str(),
                serde_json::to_string(&job.payload)?,
                job.status.as_str(),
                job.progress,
                job.error,
                job.created_at,
                job.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_job_status(
        &self,
        job_id: Uuid,
        status: JobStatus,
        progress: f32,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"UPDATE jobs SET status = ?2, progress = ?3, error = COALESCE(?4, error), updated_at = ?5
               WHERE job_id = ?1"#,
            params![job_id.to_string(), status.as_str(), progress, error, now()],
        )?;
        Ok(())
    }

    /// Any job left mid-flight by a crash or quit is not resumable — the
    /// connection that owned it is gone. Mark it failed at startup rather
    /// than showing a spinner that will never resolve.
    pub fn fail_orphaned_jobs(&self) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            r#"UPDATE jobs SET status = 'failed', error = 'interrupted (client restarted)', updated_at = ?1
               WHERE status IN ('queued','running')"#,
            params![now()],
        )?;
        Ok(n)
    }

    pub fn list_jobs(&self, limit: u32) -> Result<Vec<JobRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT j.job_id, j.peer_id, COALESCE(p.label, j.peer_id), j.payload,
                      j.status, j.progress, j.error, j.created_at, j.updated_at,
                      j.conversation_id
               FROM jobs j LEFT JOIN peers p ON p.id = j.peer_id
               ORDER BY j.created_at DESC LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![limit], row_to_job)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_job(&self, job_id: Uuid) -> Result<Option<JobRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT j.job_id, j.peer_id, COALESCE(p.label, j.peer_id), j.payload,
                      j.status, j.progress, j.error, j.created_at, j.updated_at,
                      j.conversation_id
               FROM jobs j LEFT JOIN peers p ON p.id = j.peer_id
               WHERE j.job_id = ?1"#,
        )?;
        Ok(stmt
            .query_row(params![job_id.to_string()], row_to_job)
            .optional()?)
    }

    // ----------------------------------------------------------- results

    pub fn insert_result(&self, r: &ResultRecord) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT OR REPLACE INTO results (job_id, kind, sha256, text, image_path, meta, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                r.job_id.to_string(),
                r.kind.as_str(),
                r.sha256,
                r.text,
                r.image_path,
                serde_json::to_string(&r.meta)?,
                r.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_result(&self, job_id: Uuid) -> Result<Option<ResultRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT job_id, kind, sha256, text, image_path, meta, created_at FROM results WHERE job_id = ?1",
        )?;
        Ok(stmt
            .query_row(params![job_id.to_string()], row_to_result)
            .optional()?)
    }

    pub fn list_results(&self, kind: Option<JobKind>, limit: u32) -> Result<Vec<ResultRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT job_id, kind, sha256, text, image_path, meta, created_at
               FROM results
               WHERE (?1 IS NULL OR kind = ?1)
               ORDER BY created_at DESC LIMIT ?2"#,
        )?;
        let rows = stmt.query_map(params![kind.map(|k| k.as_str()), limit], row_to_result)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ----------------------------------------------------- conversations

    pub fn create_conversation(&self, title: &str, kind: &str) -> Result<Conversation> {
        let id = Uuid::new_v4().to_string();
        let ts = now();
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO conversations (id, title, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![id, title, kind, ts],
            )?;
        }
        Ok(Conversation {
            id,
            title: title.to_string(),
            kind: kind.to_string(),
            created_at: ts,
            updated_at: ts,
            preview: String::new(),
            message_count: 0,
        })
    }

    pub fn list_conversations(&self, kind: Option<&str>, limit: u32) -> Result<Vec<Conversation>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT c.id, c.title, c.kind, c.created_at, c.updated_at,
                      COALESCE((SELECT content FROM messages m
                                WHERE m.conversation_id = c.id
                                ORDER BY m.id DESC LIMIT 1), ''),
                      (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id)
               FROM conversations c
               WHERE (?1 IS NULL OR c.kind = ?1)
               ORDER BY c.updated_at DESC
               LIMIT ?2"#,
        )?;
        let rows = stmt.query_map(params![kind, limit], |row| {
            let preview: String = row.get(5)?;
            Ok(Conversation {
                id: row.get(0)?,
                title: row.get(1)?,
                kind: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                preview: preview
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect(),
                message_count: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn rename_conversation(&self, id: &str, title: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, now()],
        )?;
        Ok(())
    }

    /// Every job this conversation produced, so a caller can clean up what
    /// they left on disk before the rows go.
    pub fn conversation_job_ids(&self, id: &str) -> Result<Vec<Uuid>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT job_id FROM jobs WHERE conversation_id = ?1")?;
        let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter_map(|s| Uuid::parse_str(&s).ok())
            .collect())
    }

    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        // Messages go with it: foreign keys are on, but be explicit.
        conn.execute(
            "DELETE FROM messages WHERE conversation_id = ?1",
            params![id],
        )?;
        conn.execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Every file the app has kept: one path per stored result.
    ///
    /// Used by "delete everything", which has to reach results whose
    /// conversation is already gone — an orphan picture is still a picture on
    /// somebody's disk.
    pub fn all_result_paths(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT image_path FROM results WHERE image_path IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Empty the history: chats, their messages, and every job and result
    /// behind them, in one transaction so a failure half way leaves nothing
    /// dangling. Peers and settings are not history and are left alone.
    ///
    /// The files are the caller's job, and go first — see
    /// [`Self::delete_result`] for why that order is the only safe one.
    pub fn clear_history(&self) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM messages", [])?;
        tx.execute("DELETE FROM conversations", [])?;
        tx.execute("DELETE FROM results", [])?;
        tx.execute("DELETE FROM jobs", [])?;
        tx.commit()?;
        // Deleted rows leave their pages in the file, and a database that
        // still measures the size of everything you asked it to forget is not
        // much of a forgetting.
        conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Forget one result and the job that made it.
    ///
    /// The file on disk is *not* touched here — deleting rows is this type's
    /// business, and destroying bytes is [`crate::erase`]'s. The caller does
    /// both, in that order, so a failed unlink never leaves a row pointing at
    /// a file the user believes is gone.
    pub fn delete_result(&self, job_id: Uuid) -> Result<()> {
        let conn = self.lock();
        let id = job_id.to_string();
        conn.execute("DELETE FROM results WHERE job_id = ?1", params![id])?;
        conn.execute("DELETE FROM messages WHERE job_id = ?1", params![id])?;
        conn.execute("DELETE FROM jobs WHERE job_id = ?1", params![id])?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_message(
        &self,
        conversation_id: &str,
        role: &str,
        content: &str,
        job_id: Option<&str>,
        sha256: Option<&str>,
        model: Option<&str>,
        peer: Option<&str>,
        tokens: Option<u64>,
        cost_micros: Option<u64>,
        thinking: Option<&str>,
    ) -> Result<Message> {
        let ts = now();
        let id = {
            let conn = self.lock();
            conn.execute(
                r#"INSERT INTO messages
                   (conversation_id, role, content, job_id, sha256, model, peer, tokens,
                    cost_micros, thinking, created_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
                params![
                    conversation_id,
                    role,
                    content,
                    job_id,
                    sha256,
                    model,
                    peer,
                    tokens,
                    cost_micros,
                    thinking,
                    ts
                ],
            )?;
            // A conversation's position in the list is "when did something
            // last happen in it".
            conn.execute(
                "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
                params![conversation_id, ts],
            )?;
            conn.last_insert_rowid()
        };

        Ok(Message {
            id,
            conversation_id: conversation_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            job_id: job_id.map(str::to_string),
            sha256: sha256.map(str::to_string),
            model: model.map(str::to_string),
            peer: peer.map(str::to_string),
            tokens,
            cost_micros,
            thinking: thinking.map(str::to_string),
            created_at: ts,
        })
    }

    /// Record what a job was actually billed, on the reply it produced.
    ///
    /// Exists because for a provider that never invoices (the mock), the
    /// charge is only computed after the reply is already filed.
    pub fn set_job_cost(&self, job_id: &str, cost_micros: u64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE messages SET cost_micros = ?2 WHERE job_id = ?1 AND role = 'assistant'",
            params![job_id, cost_micros],
        )?;
        Ok(())
    }

    pub fn conversation_messages(&self, conversation_id: &str) -> Result<Vec<Message>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, conversation_id, role, content, job_id, sha256, model, peer,
                      tokens, cost_micros, thinking, created_at
               FROM messages WHERE conversation_id = ?1 ORDER BY id ASC"#,
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| {
            Ok(Message {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                job_id: row.get(4)?,
                sha256: row.get(5)?,
                model: row.get(6)?,
                peer: row.get(7)?,
                tokens: row.get(8)?,
                cost_micros: row.get(9)?,
                thinking: row.get(10)?,
                created_at: row.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---------------------------------------------------------- settings

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT v FROM settings WHERE k = ?1")?;
        Ok(stmt.query_row(params![key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn counts(&self) -> Result<(u32, u32, u32)> {
        let conn = self.lock();
        let peers: u32 = conn.query_row(
            "SELECT COUNT(*) FROM peers WHERE endpoint <> ?1",
            params![MOCK_ENDPOINT],
            |r| r.get(0),
        )?;
        let open: u32 = conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status IN ('queued','running')",
            [],
            |r| r.get(0),
        )?;
        let results: u32 = conn.query_row("SELECT COUNT(*) FROM results", [], |r| r.get(0))?;
        Ok((peers, open, results))
    }

    /// Tokens the providers reported, grouped by the model that used them.
    ///
    /// Chat replies live in `messages`; jobs from connected tools only ever
    /// touch the spend ledger. A model's row combines both — `MAX` rather
    /// than `+`, because a priced chat reply appears in each and summing
    /// would count it twice. Cost always comes from the ledger: it is the
    /// one record of money actually deducted, whichever path spent it.
    pub fn token_usage(&self) -> Result<Vec<ModelUsage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT model,
                      MAX(SUM(mtokens), SUM(stokens)) AS tokens,
                      MAX(SUM(mreplies), SUM(sreplies)) AS replies,
                      SUM(cost) AS cost
               FROM (
                 SELECT COALESCE(NULLIF(model, ''), 'unknown') AS model,
                        tokens AS mtokens, 0 AS stokens,
                        1 AS mreplies, 0 AS sreplies, 0 AS cost
                   FROM messages
                  WHERE role = 'assistant' AND tokens IS NOT NULL AND tokens > 0
                 UNION ALL
                 SELECT model, 0, COALESCE(tokens, 0), 0, 1, cost_micros
                   FROM spends
               )
               GROUP BY model
               ORDER BY tokens DESC"#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ModelUsage {
                model: row.get(0)?,
                tokens: row.get::<_, i64>(1)? as u64,
                replies: row.get(2)?,
                cost_micros: row.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Record one job's bill in the spend ledger. `INSERT OR REPLACE` because
    /// a bill can be refined (a settle after an invoice) but a job is billed
    /// once — two rows for one job would double what the user thinks they paid.
    pub fn record_spend(&self, s: &SpendEntry) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT OR REPLACE INTO spends
               (job_id, model, peer, tokens, cost_micros, at, cumulative_micros, payout,
                abandoned, bond_cumulative, chunk_micros)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
            params![
                s.job_id,
                s.model,
                s.peer,
                s.tokens,
                s.cost_micros as i64,
                s.at,
                s.cumulative_micros.map(|c| c as i64),
                s.payout.as_deref().map(|p| p.to_lowercase()),
                s.abandoned as i64,
                s.bond_cumulative.map(|c| c as i64),
                s.chunk_micros.map(|c| c as i64),
            ],
        )?;
        Ok(())
    }

    /// Record one `Settled` event from the chain.
    pub fn record_settlement(&self, t: &Settlement) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT OR REPLACE INTO settlements
               (tx_hash, payout, cumulative, block, paid_to_worker, fee)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            params![
                t.tx_hash.to_lowercase(),
                t.payout.to_lowercase(),
                t.cumulative as i64,
                t.block as i64,
                t.paid_to_worker as i64,
                t.fee as i64,
            ],
        )?;
        Ok(())
    }

    /// Resolve abandoned replies against the chain: a settle of exactly a
    /// reply's bond ticket means the worker kept the prepaid chunk, and that
    /// is what the reply cost. Returns how many were resolved.
    pub fn resolve_abandoned(&self) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            r#"UPDATE spends
               SET cost_micros = chunk_micros, cumulative_micros = bond_cumulative
               WHERE abandoned = 1 AND cost_micros = 0 AND chunk_micros IS NOT NULL
                 AND EXISTS (SELECT 1 FROM settlements t
                              WHERE t.payout = spends.payout
                                AND t.cumulative = spends.bond_cumulative)"#,
            [],
        )?;
        Ok(n)
    }

    /// The block a settlement scan starts from: wherever the last scan
    /// stopped, else this chain's first deposit — and never below `floor`,
    /// the pot's deployment block. A deposit recorded on another chain (a
    /// local Anvil, say) has a block number that means nothing here; without
    /// the floor it once sent the scan to block 10 of Base mainnet.
    pub fn settlement_scan_from(&self, chain_id: u64, floor: u64) -> Result<u64> {
        if let Some(v) = self.get_setting("settle_scan_block")? {
            if let Ok(n) = v.parse::<u64>() {
                return Ok(n.max(floor));
            }
        }
        let conn = self.lock();
        let first: Option<i64> = conn
            .query_row(
                "SELECT MIN(block) FROM deposits WHERE block > 0 AND chain_id = ?1",
                params![chain_id as i64],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(first.map(|b| b as u64).unwrap_or(floor).max(floor))
    }

    /// Every bill a priced provider charged, newest first: the per-job audit
    /// trail of money actually deducted from the pot. Free providers never
    /// bill and do not appear — this is a ledger, not a chat history.
    pub fn spend_history(&self, limit: u32) -> Result<Vec<SpendEntry>> {
        let conn = self.lock();
        // The settle that collected a reply is the first one on its channel
        // whose cumulative reaches the reply's ticket. An abandoned reply
        // appears only once the chain shows its chunk was kept; until then
        // nothing was deducted and there is nothing to list.
        let mut stmt = conn.prepare(
            r#"SELECT s.job_id, s.model, s.peer, s.tokens, s.cost_micros, s.at,
                      s.cumulative_micros, s.payout, s.abandoned,
                      (SELECT t.tx_hash FROM settlements t
                        WHERE t.payout = s.payout AND t.cumulative >= s.cumulative_micros
                        ORDER BY t.cumulative ASC, t.block ASC LIMIT 1),
                      (SELECT t.block FROM settlements t
                        WHERE t.payout = s.payout AND t.cumulative >= s.cumulative_micros
                        ORDER BY t.cumulative ASC, t.block ASC LIMIT 1)
               FROM spends s
               WHERE NOT (s.abandoned = 1 AND s.cost_micros = 0)
               ORDER BY s.at DESC, s.rowid DESC LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(SpendEntry {
                job_id: row.get(0)?,
                model: row.get(1)?,
                peer: row.get(2)?,
                tokens: row.get(3)?,
                cost_micros: row.get::<_, i64>(4)? as u64,
                at: row.get(5)?,
                cumulative_micros: row.get::<_, Option<i64>>(6)?.map(|c| c as u64),
                payout: row.get(7)?,
                abandoned: row.get::<_, i64>(8)? != 0,
                bond_cumulative: None,
                chunk_micros: None,
                settle_tx: row.get(9)?,
                settle_block: row.get::<_, Option<i64>>(10)?.map(|b| b as u64),
                settle_url: None,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn record_deposit(&self, d: &StoredDeposit) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO deposits
               (tx_hash, amount_micros, max_per_job_micros, max_per_day_micros,
                block, at, chain_id, client)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(tx_hash) DO UPDATE SET
                 amount_micros = excluded.amount_micros,
                 max_per_job_micros = excluded.max_per_job_micros,
                 max_per_day_micros = excluded.max_per_day_micros,
                 block = excluded.block,
                 at = excluded.at,
                 chain_id = excluded.chain_id,
                 client = excluded.client"#,
            params![
                d.tx_hash,
                d.amount_micros as i64,
                d.max_per_job_micros as i64,
                d.max_per_day_micros as i64,
                d.block as i64,
                d.at,
                d.chain_id as i64,
                d.client,
            ],
        )?;
        Ok(())
    }

    pub fn list_deposits(&self) -> Result<Vec<StoredDeposit>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT tx_hash, amount_micros, max_per_job_micros, max_per_day_micros,
                      block, at, chain_id, client
               FROM deposits ORDER BY at DESC, block DESC"#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StoredDeposit {
                tx_hash: row.get(0)?,
                amount_micros: row.get::<_, i64>(1)? as u64,
                max_per_job_micros: row.get::<_, i64>(2)? as u64,
                max_per_day_micros: row.get::<_, i64>(3)? as u64,
                block: row.get::<_, i64>(4)? as u64,
                at: row.get(5)?,
                chain_id: row.get::<_, i64>(6)? as u64,
                client: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The MetaMask that last deposited on this chain, if this machine has one.
    pub fn last_deposit_client(&self, chain_id: u64) -> Result<Option<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT client FROM deposits WHERE client <> '' AND chain_id = ?1 ORDER BY at DESC LIMIT 1",
        )?;
        Ok(stmt.query_row([chain_id as i64], |r| r.get(0)).optional()?)
    }
}

/// A deposit this app watched MetaMask confirm. The chain is the source of
/// the money; this row is just the receipt so the wallet page does not have
/// to ask a node for every `Deposited` since genesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDeposit {
    pub tx_hash: String,
    pub amount_micros: u64,
    pub max_per_job_micros: u64,
    pub max_per_day_micros: u64,
    pub block: u64,
    pub at: i64,
    pub chain_id: u64,
    pub client: String,
}

/// One model's share of billed tokens on this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: u64,
    pub replies: u32,
    /// Sum of the recorded bills for this model, in µUSDC. Replies from free
    /// providers, or from before costs were recorded, contribute nothing.
    #[serde(default)]
    pub cost_micros: u64,
}

/// One priced job and what it was billed — a row in the spend ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendEntry {
    pub job_id: String,
    pub model: String,
    /// The provider that answered, as labeled when the funds were locked.
    pub peer: Option<String>,
    /// Tokens the bill covered (prompt + completion), when the provider said.
    pub tokens: Option<u64>,
    pub cost_micros: u64,
    pub at: i64,
    /// The signed cumulative ticket this charge rode on, and its channel.
    /// Together they name the on-chain settle that collected it.
    #[serde(default)]
    pub cumulative_micros: Option<u64>,
    #[serde(default)]
    pub payout: Option<String>,
    /// True when the reply ended without a bill and the worker kept the
    /// prepaid chunk instead — `cost_micros` is then the chunk.
    #[serde(default)]
    pub abandoned: bool,
    /// For an abandoned reply: the bond ticket and chunk at stake, until the
    /// chain says whether the chunk was kept.
    #[serde(default)]
    pub bond_cumulative: Option<u64>,
    #[serde(default)]
    pub chunk_micros: Option<u64>,
    /// The transaction that collected this charge, once one has. A reply
    /// with none is charged but not yet collected — or predates tracking.
    #[serde(default)]
    pub settle_tx: Option<String>,
    #[serde(default)]
    pub settle_block: Option<u64>,
    #[serde(default)]
    pub settle_url: Option<String>,
}

/// One `Settled` event from the Pot: which tx collected up to which ticket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settlement {
    pub tx_hash: String,
    pub payout: String,
    pub cumulative: u64,
    pub block: u64,
    pub paid_to_worker: u64,
    pub fee: u64,
}

// --------------------------------------------------------------- row mapping

fn row_to_peer(row: &rusqlite::Row<'_>) -> rusqlite::Result<Peer> {
    let caps: String = row.get(7)?;
    let models: String = row.get(8)?;
    Ok(Peer {
        id: row.get(0)?,
        label: row.get(1)?,
        endpoint: row.get(2)?,
        public_key: row.get(3)?,
        peer_id: row.get(4)?,
        status: row.get(5)?,
        latency_ms: row.get(6)?,
        caps: serde_json::from_str(&caps).unwrap_or_default(),
        models: serde_json::from_str(&models).unwrap_or_default(),
        max_concurrent: row.get(9)?,
        last_seen: row.get(10)?,
        last_error: row.get(11)?,
        added_at: row.get(12)?,
        source: row.get(13)?,
        country: row.get(14)?,
        payout: row.get(15)?,
    })
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
    let job_id: String = row.get(0)?;
    let payload_json: String = row.get(3)?;
    let status: String = row.get(4)?;
    let payload: JobPayload = serde_json::from_str(&payload_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status: JobStatus =
        serde_json::from_value(serde_json::Value::String(status)).unwrap_or(JobStatus::Failed);
    Ok(JobRecord {
        job_id: Uuid::parse_str(&job_id).unwrap_or(Uuid::nil()),
        peer_id: row.get(1)?,
        peer_label: row.get(2)?,
        kind: payload.kind(),
        summary: payload.summary(),
        model: payload.model_label(),
        payload,
        status,
        progress: row.get(5)?,
        error: row.get(6)?,
        conversation_id: row.get(9)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn row_to_result(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResultRecord> {
    let job_id: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let meta: String = row.get(5)?;
    Ok(ResultRecord {
        job_id: Uuid::parse_str(&job_id).unwrap_or(Uuid::nil()),
        kind: match kind.as_str() {
            "image" => JobKind::Image,
            "video" => JobKind::Video,
            _ => JobKind::Llm,
        },
        sha256: row.get(2)?,
        text: row.get(3)?,
        image_path: row.get(4)?,
        meta: serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null),
        created_at: row.get(6)?,
    })
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootmode_core::{ChatMessage, LlmParams};

    fn db() -> Db {
        let path = std::env::temp_dir().join(format!("rootmode-test-{}.sqlite", Uuid::new_v4()));
        Db::open(&path).unwrap()
    }

    fn payload() -> JobPayload {
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("mock-llm".into()),
            messages: vec![ChatMessage::new("user", "ping")],
            tools: Vec::new(),
            max_tokens: 32,
            temperature: 0.0,
        })
    }

    #[test]
    fn the_fake_worker_is_off_unless_asked_for() {
        let db = db();
        assert!(
            db.list_peers().unwrap().is_empty(),
            "a fresh install lists no providers at all, real or pretend"
        );

        db.enable_mock_peer().unwrap();
        let peers = db.list_peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].is_mock());

        db.remove_peer(MOCK_PEER_ID).unwrap();
        assert_eq!(
            db.list_peers().unwrap().len(),
            1,
            "while enabled it is not removable by hand"
        );
    }

    #[test]
    fn dead_discovered_peers_are_forgotten() {
        let db = db();
        let alive = db.upsert_discovered_peer("alive", "p2p://aa").unwrap();
        let dead = db.upsert_discovered_peer("dead", "p2p://bb").unwrap();
        let typed = db
            .add_peer("typed by hand", "ws://10.0.0.9:9944", None)
            .unwrap();

        db.update_peer_status(&alive.id, "online", None, None, None, None, None, None, None, None)
            .unwrap();
        db.update_peer_status(&dead.id, "offline", None, None, None, None, None, None, None, None)
            .unwrap();
        db.update_peer_status(&typed.id, "offline", None, None, None, None, None, None, None, None)
            .unwrap();

        assert_eq!(db.prune_dead_discovered(Duration::from_secs(0)).unwrap(), 1);

        let left: Vec<String> = db
            .list_peers()
            .unwrap()
            .into_iter()
            .map(|p| p.label)
            .collect();
        assert!(left.contains(&"alive".to_string()));
        assert!(
            left.contains(&"typed by hand".to_string()),
            "a peer you added yourself is never forgotten for you"
        );
        assert!(!left.contains(&"dead".to_string()));
    }

    #[test]
    fn job_roundtrip_and_orphan_sweep() {
        let db = db();
        // This exercises the job → peer label join, so it needs a peer.
        db.enable_mock_peer().unwrap();
        let p = payload();
        let rec = JobRecord {
            job_id: Uuid::new_v4(),
            conversation_id: None,
            peer_id: MOCK_PEER_ID.into(),
            peer_label: String::new(),
            kind: p.kind(),
            summary: p.summary(),
            model: p.model_label(),
            payload: p,
            status: JobStatus::Running,
            progress: 0.5,
            error: None,
            created_at: now(),
            updated_at: now(),
        };
        db.insert_job(&rec).unwrap();

        let listed = db.list_jobs(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].summary, "ping");
        assert_eq!(listed[0].peer_label, "mock worker (local)");

        assert_eq!(db.fail_orphaned_jobs().unwrap(), 1);
        let after = db.get_job(rec.job_id).unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Failed);
        assert!(after.error.unwrap().contains("interrupted"));
    }

    #[test]
    fn an_older_database_gains_the_columns_it_is_missing() {
        // Exactly what a user upgrading hits: tables that already exist, so
        // CREATE TABLE IF NOT EXISTS does nothing and the new column is absent
        // until something adds it.
        let path = std::env::temp_dir().join(format!("rootmode-old-{}.sqlite", Uuid::new_v4()));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE peers (
                    id TEXT PRIMARY KEY, label TEXT NOT NULL, endpoint TEXT NOT NULL UNIQUE,
                    public_key TEXT, peer_id TEXT, status TEXT NOT NULL DEFAULT 'unknown',
                    latency_ms INTEGER, caps TEXT NOT NULL DEFAULT '[]',
                    models TEXT NOT NULL DEFAULT '[]', max_concurrent INTEGER NOT NULL DEFAULT 1,
                    last_seen INTEGER, last_error TEXT, added_at INTEGER NOT NULL
                );
                CREATE TABLE conversations (
                    id TEXT PRIMARY KEY, title TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'llm',
                    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
                );
                CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, conversation_id TEXT NOT NULL,
                    role TEXT NOT NULL, content TEXT NOT NULL, job_id TEXT, sha256 TEXT,
                    model TEXT, peer TEXT, created_at INTEGER NOT NULL
                );
                INSERT INTO conversations VALUES ('c1', 'from before', 'llm', 1, 1);
                INSERT INTO messages (conversation_id, role, content, created_at)
                    VALUES ('c1', 'user', 'said before the upgrade', 1);
                "#,
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();

        // Reading it back is what actually failed for a real user.
        let messages = db.conversation_messages("c1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "said before the upgrade");
        assert_eq!(messages[0].tokens, None, "unmeasured, not zero");

        // And the new columns are usable.
        db.add_message(
            "c1",
            "assistant",
            "after",
            None,
            None,
            None,
            None,
            Some(42),
            Some(1300),
            None,
        )
        .unwrap();
        let after = &db.conversation_messages("c1").unwrap()[1];
        assert_eq!(after.tokens, Some(42));
        assert_eq!(after.cost_micros, Some(1300));
    }

    #[test]
    fn deleting_a_chat_takes_its_messages_with_it() {
        let db = db();
        let keep = db.create_conversation("keep me", "llm").unwrap();
        let bin = db.create_conversation("delete me", "llm").unwrap();

        db.add_message(&keep.id, "user", "still here", None, None, None, None, None, None, None)
            .unwrap();
        db.add_message(&bin.id, "user", "question", None, None, None, None, None, None, None)
            .unwrap();
        db.add_message(&bin.id, "assistant", "answer", None, None, None, None, None, None, None)
            .unwrap();

        db.delete_conversation(&bin.id).unwrap();

        let left = db.list_conversations(None, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].title, "keep me");
        assert!(
            db.conversation_messages(&bin.id).unwrap().is_empty(),
            "messages do not outlive the chat they belonged to"
        );
        assert_eq!(db.conversation_messages(&keep.id).unwrap().len(), 1);
    }

    #[test]
    fn token_usage_groups_by_model_and_skips_unmeasured() {
        let db = db();
        let chat = db.create_conversation("talk", "llm").unwrap();
        db.add_message(&chat.id, "user", "hi", None, None, None, None, None, None, None)
            .unwrap();
        db.add_message(
            &chat.id,
            "assistant",
            "a",
            None,
            None,
            Some("flash"),
            None,
            Some(120),
            Some(2400),
            None,
        )
        .unwrap();
        db.record_spend(&SpendEntry {
            job_id: "flash-a".into(),
            model: "flash".into(),
            peer: None,
            tokens: Some(120),
            cost_micros: 2400,
            at: 100,
            cumulative_micros: None,
            payout: None,
            abandoned: false,
            bond_cumulative: None,
            chunk_micros: None,
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        })
        .unwrap();
        db.add_message(
            &chat.id,
            "assistant",
            "b",
            None,
            None,
            Some("flash"),
            None,
            Some(80),
            Some(1600),
            None,
        )
        .unwrap();
        db.record_spend(&SpendEntry {
            job_id: "flash-b".into(),
            model: "flash".into(),
            peer: None,
            tokens: Some(80),
            cost_micros: 1600,
            at: 101,
            cumulative_micros: None,
            payout: None,
            abandoned: false,
            bond_cumulative: None,
            chunk_micros: None,
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        })
        .unwrap();
        db.add_message(
            &chat.id,
            "assistant",
            "c",
            None,
            None,
            Some("opus"),
            None,
            Some(50),
            None,
            None,
        )
        .unwrap();
        db.add_message(
            &chat.id,
            "assistant",
            "no count",
            None,
            None,
            Some("opus"),
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let usage = db.token_usage().unwrap();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].model, "flash");
        assert_eq!(usage[0].tokens, 200);
        assert_eq!(usage[0].replies, 2, "a billed chat reply is not counted twice");
        assert_eq!(usage[0].cost_micros, 4000);
        assert_eq!(usage[1].model, "opus");
        assert_eq!(usage[1].tokens, 50);
        assert_eq!(usage[1].replies, 1);
        assert_eq!(usage[1].cost_micros, 0, "an unrecorded bill sums as nothing, not a guess");

        // A job from a connected tool bills the ledger but files no chat
        // message — it still owes the user a row on the usage card.
        db.record_spend(&SpendEntry {
            job_id: "gw-1".into(),
            model: "gateway-only".into(),
            peer: Some("gpu box".into()),
            tokens: Some(500),
            cost_micros: 990,
            at: 102,
            cumulative_micros: None,
            payout: None,
            abandoned: false,
            bond_cumulative: None,
            chunk_micros: None,
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        })
        .unwrap();
        let usage = db.token_usage().unwrap();
        let gw = usage.iter().find(|u| u.model == "gateway-only").unwrap();
        assert_eq!(gw.tokens, 500);
        assert_eq!(gw.replies, 1);
        assert_eq!(gw.cost_micros, 990);
    }

    #[test]
    fn the_spend_ledger_lists_bills_newest_first_and_never_doubles_a_job() {
        let db = db();
        db.record_spend(&SpendEntry {
            job_id: "job-1".into(),
            model: "deepseek-v4-flash-0731".into(),
            peer: Some("gpu box".into()),
            tokens: Some(1367),
            cost_micros: 2734,
            at: 100,
            cumulative_micros: None,
            payout: None,
            abandoned: false,
            bond_cumulative: None,
            chunk_micros: None,
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        })
        .unwrap();
        db.record_spend(&SpendEntry {
            job_id: "job-2".into(),
            model: "deepseek-v4-flash-0731".into(),
            peer: Some("gpu box".into()),
            tokens: None,
            cost_micros: 0,
            at: 200,
            cumulative_micros: None,
            payout: None,
            abandoned: false,
            bond_cumulative: None,
            chunk_micros: None,
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        })
        .unwrap();

        let ledger = db.spend_history(10).unwrap();
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0].job_id, "job-2");
        assert_eq!(
            ledger[0].cost_micros, 0,
            "a priced job that billed nothing still shows"
        );
        assert_eq!(ledger[1].job_id, "job-1");
        assert_eq!(ledger[1].peer.as_deref(), Some("gpu box"));
        assert_eq!(ledger[1].tokens, Some(1367));
        assert_eq!(ledger[1].cost_micros, 2734);

        // Billing the same job again refines the row instead of adding one —
        // a ledger that lists one payment twice overstates what was spent.
        db.record_spend(&SpendEntry {
            job_id: "job-2".into(),
            model: "deepseek-v4-flash-0731".into(),
            peer: Some("gpu box".into()),
            tokens: Some(90),
            cost_micros: 180,
            at: 201,
            cumulative_micros: None,
            payout: None,
            abandoned: false,
            bond_cumulative: None,
            chunk_micros: None,
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        })
        .unwrap();
        let ledger = db.spend_history(10).unwrap();
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0].cost_micros, 180);
    }

    #[test]
    fn a_reply_points_at_the_settle_that_first_covered_its_ticket() {
        let db = db();
        let spend = |job: &str, cumulative: u64| SpendEntry {
            job_id: job.into(),
            model: "glm-5.2".into(),
            peer: Some("perch".into()),
            tokens: Some(1000),
            cost_micros: 400,
            at: 100,
            cumulative_micros: Some(cumulative),
            payout: Some("0xAbC".into()),
            abandoned: false,
            bond_cumulative: None,
            chunk_micros: None,
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        };
        db.record_spend(&spend("j1", 1_000)).unwrap();
        db.record_spend(&spend("j2", 1_400)).unwrap();
        db.record_spend(&spend("j3", 1_800)).unwrap();
        // One batched settle collected j1 and j2; j3 is still outstanding.
        db.record_settlement(&Settlement {
            tx_hash: "0xTX1".into(),
            payout: "0xabc".into(),
            cumulative: 1_400,
            block: 7,
            paid_to_worker: 1_260,
            fee: 140,
        })
        .unwrap();

        let by_job = |rows: &Vec<SpendEntry>, id: &str| rows.iter().find(|r| r.job_id == id).unwrap().clone();
        let rows = db.spend_history(10).unwrap();
        assert_eq!(by_job(&rows, "j1").settle_tx.as_deref(), Some("0xtx1"));
        assert_eq!(by_job(&rows, "j2").settle_tx.as_deref(), Some("0xtx1"), "shares the batch");
        assert_eq!(by_job(&rows, "j3").settle_tx, None, "charged, not yet collected");

        db.record_settlement(&Settlement {
            tx_hash: "0xTX2".into(),
            payout: "0xabc".into(),
            cumulative: 2_000,
            block: 9,
            paid_to_worker: 540,
            fee: 60,
        })
        .unwrap();
        let rows = db.spend_history(10).unwrap();
        assert_eq!(by_job(&rows, "j3").settle_tx.as_deref(), Some("0xtx2"));
        assert_eq!(by_job(&rows, "j1").settle_tx.as_deref(), Some("0xtx1"), "the first covering tx, not the newest");
    }

    #[test]
    fn a_stopped_reply_costs_its_chunk_only_once_the_chain_says_so() {
        let db = db();
        db.record_spend(&SpendEntry {
            job_id: "stopped".into(),
            model: "kimi-k3".into(),
            peer: Some("anvil".into()),
            tokens: None,
            cost_micros: 0,
            at: 100,
            cumulative_micros: None,
            payout: Some("0xabc".into()),
            abandoned: true,
            bond_cumulative: Some(2_111_626),
            chunk_micros: Some(500_000),
            settle_tx: None,
            settle_block: None,
            settle_url: None,
        })
        .unwrap();
        assert!(
            db.spend_history(10).unwrap().is_empty(),
            "nothing deducted yet, so nothing to list"
        );
        // A settle past the bond, but not of it, is other work being collected.
        db.record_settlement(&Settlement {
            tx_hash: "0xother".into(), payout: "0xabc".into(), cumulative: 2_111_700,
            block: 5, paid_to_worker: 0, fee: 0,
        })
        .unwrap();
        assert_eq!(db.resolve_abandoned().unwrap(), 0);
        // The bond ticket itself settling is the worker keeping the chunk.
        db.record_settlement(&Settlement {
            tx_hash: "0xchunk".into(), payout: "0xabc".into(), cumulative: 2_111_626,
            block: 4, paid_to_worker: 450_000, fee: 50_000,
        })
        .unwrap();
        assert_eq!(db.resolve_abandoned().unwrap(), 1);
        let rows = db.spend_history(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].abandoned);
        assert_eq!(rows[0].cost_micros, 500_000);
        assert_eq!(rows[0].settle_tx.as_deref(), Some("0xchunk"));
    }

    #[test]
    fn a_bill_recorded_after_the_reply_lands_on_that_reply() {
        let db = db();
        let chat = db.create_conversation("talk", "llm").unwrap();
        db.add_message(
            &chat.id,
            "assistant",
            "answer",
            Some("job-1"),
            None,
            Some("flash"),
            None,
            Some(10),
            None,
            None,
        )
        .unwrap();
        db.set_job_cost("job-1", 55).unwrap();
        assert_eq!(
            db.conversation_messages(&chat.id).unwrap()[0].cost_micros,
            Some(55)
        );
    }

    #[test]
    fn a_deposit_is_listed_newest_first_and_not_duplicated() {
        let db = db();
        let first = StoredDeposit {
            tx_hash: "0xaaa".into(),
            amount_micros: 5_000_000,
            max_per_job_micros: 500_000,
            max_per_day_micros: 2_000_000,
            block: 10,
            at: 100,
            chain_id: 8453,
            client: "0xabc".into(),
        };
        let second = StoredDeposit {
            tx_hash: "0xbbb".into(),
            amount_micros: 1_000_000,
            max_per_job_micros: 500_000,
            max_per_day_micros: 2_000_000,
            block: 11,
            at: 200,
            chain_id: 8453,
            client: "0xabc".into(),
        };
        db.record_deposit(&first).unwrap();
        db.record_deposit(&second).unwrap();
        db.record_deposit(&second).unwrap();

        let list = db.list_deposits().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].tx_hash, "0xbbb");
        assert_eq!(list[1].amount_micros, 5_000_000);
        assert_eq!(db.last_deposit_client(8453).unwrap().as_deref(), Some("0xabc"));
    }

    #[test]
    fn deleting_everything_leaves_nothing_behind() {
        let db = db();
        db.enable_mock_peer().unwrap();

        let chat = db.create_conversation("talk", "llm").unwrap();
        db.add_message(&chat.id, "user", "hello", None, None, None, None, None, None, None)
            .unwrap();

        let p = payload();
        let job = JobRecord {
            job_id: Uuid::new_v4(),
            conversation_id: Some(chat.id.clone()),
            peer_id: MOCK_PEER_ID.into(),
            peer_label: String::new(),
            kind: p.kind(),
            summary: p.summary(),
            model: p.model_label(),
            payload: p,
            status: JobStatus::Done,
            progress: 1.0,
            error: None,
            created_at: now(),
            updated_at: now(),
        };
        db.insert_job(&job).unwrap();
        db.insert_result(&ResultRecord {
            job_id: job.job_id,
            kind: JobKind::Image,
            sha256: "abc".into(),
            text: None,
            image_path: Some("/tmp/rootmode-test-picture.png".into()),
            meta: serde_json::json!({}),
            created_at: now(),
        })
        .unwrap();

        // The paths are handed out so the caller can erase the files first;
        // a wipe that forgot them would leave pictures on disk that nothing
        // in the app can even see any more.
        assert_eq!(
            db.all_result_paths().unwrap(),
            vec!["/tmp/rootmode-test-picture.png".to_string()]
        );

        db.clear_history().unwrap();

        assert!(db.list_conversations(None, 10).unwrap().is_empty());
        assert!(db.conversation_messages(&chat.id).unwrap().is_empty());
        assert!(db.list_jobs(10).unwrap().is_empty());
        assert!(db.get_result(job.job_id).unwrap().is_none());
        assert!(
            !db.list_peers().unwrap().is_empty(),
            "providers are not history and outlive a wipe"
        );
    }

    #[test]
    fn a_chat_is_listed_by_when_it_last_had_activity() {
        let db = db();
        let first = db.create_conversation("older", "llm").unwrap();
        let second = db.create_conversation("newer", "llm").unwrap();

        // Replying in the older chat should float it to the top.
        db.add_message(
            &first.id,
            "user",
            "hello again",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let listed: Vec<String> = db
            .list_conversations(None, 10)
            .unwrap()
            .into_iter()
            .map(|c| c.title)
            .collect();
        assert_eq!(listed.first().map(String::as_str), Some("older"));
        assert!(listed.contains(&second.title));
    }

    #[test]
    fn settings_upsert() {
        let db = db();
        assert!(db.get_setting("theme").unwrap().is_none());
        db.set_setting("theme", "dark").unwrap();
        db.set_setting("theme", "dark-amber").unwrap();
        assert_eq!(
            db.get_setting("theme").unwrap().as_deref(),
            Some("dark-amber")
        );
    }
}
