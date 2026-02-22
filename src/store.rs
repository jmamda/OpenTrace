use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CallRecord {
    pub id: String,
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub status_code: u16,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub error: Option<String>,
    pub provider_request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Stats {
    pub total_calls: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cost_usd: f64,
    pub avg_latency_ms: f64,
    pub error_count: i64,
    pub calls_last_hour: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelStats {
    pub model: String,
    pub calls: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cost_usd: f64,
    pub avg_latency_ms: f64,
    pub error_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointStats {
    pub endpoint: String,
    pub calls: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cost_usd: f64,
    pub avg_latency_ms: f64,
    pub error_count: i64,
}

#[derive(Default)]
pub struct QueryFilter {
    pub errors_only: bool,
    pub model: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

pub struct Store {
    conn: Connection,
}

/// Column mapping helper — returns a CallRecord from a rusqlite Row.
/// Used by every SELECT query to avoid duplicating the index-to-field mapping.
///
/// Column order must match all SELECT statements:
///   0=id  1=timestamp  2=provider  3=model  4=endpoint
///   5=status_code  6=latency_ms  7=ttft_ms
///   8=input_tokens  9=output_tokens  10=cost_usd
///   11=request_body  12=response_body  13=error
///   14=provider_request_id
fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallRecord> {
    Ok(CallRecord {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        endpoint: row.get(4)?,
        status_code: row.get::<_, u16>(5)?,
        latency_ms: row.get::<_, i64>(6)? as u64,
        ttft_ms: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        input_tokens: row.get(8)?,
        output_tokens: row.get(9)?,
        cost_usd: row.get(10)?,
        request_body: row.get(11)?,
        response_body: row.get(12)?,
        error: row.get(13)?,
        provider_request_id: row.get(14)?,
    })
}

const SELECT_COLS: &str = "
    id, timestamp, provider, model, endpoint,
    status_code, latency_ms, ttft_ms,
    input_tokens, output_tokens, cost_usd,
    request_body, response_body, error,
    provider_request_id";

impl Store {
    /// Open an in-memory SQLite database.
    /// This avoids touching the filesystem and keeps every test isolated.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("failed to open in-memory DB")?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// Open (or create) a SQLite database at a caller-specified path.
    /// Intended for integration tests that need a real file DB in a temp dir.
    pub fn open_at(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .context("failed to create parent directory for DB")?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open DB at {}", path.display()))?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    pub fn open() -> Result<Self> {
        let db_path = db_path()?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .context("failed to create .trace directory")?;
        }
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open DB at {}", db_path.display()))?;

        // Restrict file permissions to owner-only on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = std::fs::set_permissions(&db_path, perms);
        }

        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch("
            PRAGMA busy_timeout = 5000;
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA cache_size=-32000;
            PRAGMA temp_store=MEMORY;
            PRAGMA mmap_size=67108864;

            CREATE TABLE IF NOT EXISTS calls (
                id                  TEXT PRIMARY KEY,
                timestamp           TEXT NOT NULL,
                provider            TEXT NOT NULL DEFAULT 'openai',
                model               TEXT NOT NULL DEFAULT 'unknown',
                endpoint            TEXT NOT NULL DEFAULT '/v1/chat/completions',
                status_code         INTEGER NOT NULL DEFAULT 200,
                latency_ms          INTEGER NOT NULL DEFAULT 0,
                ttft_ms             INTEGER,
                input_tokens        INTEGER,
                output_tokens       INTEGER,
                cost_usd            REAL,
                request_body        TEXT,
                response_body       TEXT,
                error               TEXT,
                provider_request_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_calls_timestamp ON calls(timestamp);
            CREATE INDEX IF NOT EXISTS idx_calls_model ON calls(model);

            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
        ")?;

        // Migrate existing databases that predate ttft_ms / provider_request_id.
        // ALTER TABLE ADD COLUMN fails with "duplicate column name" if the column
        // already exists — ignore that error so new installs succeed too.
        for sql in &[
            "ALTER TABLE calls ADD COLUMN ttft_ms INTEGER",
            "ALTER TABLE calls ADD COLUMN provider_request_id TEXT",
        ] {
            if let Err(e) = self.conn.execute_batch(sql) {
                let msg = e.to_string().to_lowercase();
                if !msg.contains("duplicate column name") {
                    return Err(anyhow::anyhow!("schema migration failed: {e}"));
                }
            }
        }

        Ok(())
    }

    pub fn insert(&self, r: &CallRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO calls (
                id, timestamp, provider, model, endpoint,
                status_code, latency_ms, ttft_ms,
                input_tokens, output_tokens,
                cost_usd, request_body, response_body, error,
                provider_request_id
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                r.id, r.timestamp, r.provider, r.model, r.endpoint,
                r.status_code, r.latency_ms, r.ttft_ms.map(|v| v as i64),
                r.input_tokens, r.output_tokens,
                r.cost_usd, r.request_body, r.response_body, r.error,
                r.provider_request_id,
            ],
        )?;
        Ok(())
    }

    /// Query with optional filters. All parameters are fully SQL-parameterized.
    pub fn query_filtered(&self, limit: usize, filter: &QueryFilter) -> Result<Vec<CallRecord>> {
        let limit_i64 = limit.min(10_000) as i64;
        let errors_only_flag: i64 = if filter.errors_only { 1 } else { 0 };
        // Escape SQL LIKE wildcards so a model filter of "gpt_4" matches
        // "gpt_4" literally rather than "gpt-4" or "gpt14" etc.
        let model_filter: Option<String> = filter.model.as_deref()
            .map(|m| m.replace('%', "\\%").replace('_', "\\_"));
        let since: Option<String> = filter.since.clone();
        let until: Option<String> = filter.until.clone();

        // status_code = 0 means upstream connection failure — include in --errors.
        let sql = format!("
            SELECT {SELECT_COLS}
            FROM calls
            WHERE (?1 = 0 OR (error IS NOT NULL OR status_code >= 400 OR status_code = 0))
              AND (?2 IS NULL OR model LIKE '%' || ?2 || '%' ESCAPE '\\')
              AND (?3 IS NULL OR timestamp >= ?3)
              AND (?4 IS NULL OR timestamp <= ?4)
            ORDER BY timestamp DESC
            LIMIT ?5");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![errors_only_flag, model_filter, since, until, limit_i64],
            row_to_record,
        )?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Fetch a single record by full ID or unambiguous prefix.
    pub fn get_by_id(&self, id: &str) -> Result<Option<CallRecord>> {
        let sql = format!("SELECT {SELECT_COLS} FROM calls WHERE id LIKE ?1 || '%' LIMIT 1");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![id], row_to_record)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Return records with timestamp strictly after `after_ts`, ordered ASC.
    /// Used by `trace watch` to poll for new records.
    pub fn query_after(
        &self,
        after_ts: &str,
        filter: &QueryFilter,
        limit: usize,
    ) -> Result<Vec<CallRecord>> {
        let limit_i64 = limit.min(10_000) as i64;
        let errors_only_flag: i64 = if filter.errors_only { 1 } else { 0 };
        let model_filter: Option<String> = filter.model.as_deref()
            .map(|m| m.replace('%', "\\%").replace('_', "\\_"));

        let sql = format!("
            SELECT {SELECT_COLS}
            FROM calls
            WHERE timestamp > ?1
              AND (?2 = 0 OR (error IS NOT NULL OR status_code >= 400 OR status_code = 0))
              AND (?3 IS NULL OR model LIKE '%' || ?3 || '%' ESCAPE '\\')
            ORDER BY timestamp ASC
            LIMIT ?4");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![after_ts, errors_only_flag, model_filter, limit_i64],
            row_to_record,
        )?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Write (or overwrite) a key-value pair in the meta table.
    /// Used to persist operational counters (e.g., dropped_records) across
    /// process boundaries so `trace stats` can read them from the proxy process.
    pub fn update_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Read a value from the meta table. Returns Ok(None) if the key is absent.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        match stmt.query_row(params![key], |row| row.get(0)) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete records older than `days` days. Returns the number of rows deleted.
    pub fn prune_older_than(&self, days: u32) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM calls WHERE timestamp < datetime('now', ?1)",
            params![format!("-{} days", days)],
        )?;
        Ok(count)
    }

    pub fn stats(&self) -> Result<Stats> {
        let total_calls: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM calls", [], |r| r.get(0))?;
        let total_input_tokens: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(input_tokens), 0) FROM calls", [], |r| r.get(0))?;
        let total_output_tokens: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(output_tokens), 0) FROM calls", [], |r| r.get(0))?;
        let total_cost_usd: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM calls", [], |r| r.get(0))?;
        let avg_latency_ms: f64 = self.conn.query_row(
            "SELECT COALESCE(AVG(latency_ms), 0.0) FROM calls", [], |r| r.get(0))?;
        let error_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM calls WHERE error IS NOT NULL OR status_code >= 400 OR status_code = 0",
            [], |r| r.get(0))?;
        let calls_last_hour: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM calls WHERE timestamp >= datetime('now', '-1 hour')", [], |r| r.get(0))?;

        Ok(Stats {
            total_calls,
            total_input_tokens,
            total_output_tokens,
            total_cost_usd,
            avg_latency_ms,
            error_count,
            calls_last_hour,
        })
    }

    /// Per-model cost and usage breakdown, ordered by total cost descending.
    pub fn stats_by_model(&self) -> Result<Vec<ModelStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT model,
                    COUNT(*) as calls,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(AVG(latency_ms), 0.0),
                    COUNT(CASE WHEN error IS NOT NULL OR status_code >= 400 OR status_code = 0 THEN 1 END)
             FROM calls
             GROUP BY model
             ORDER BY SUM(cost_usd) DESC NULLS LAST",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ModelStats {
                model: row.get(0)?,
                calls: row.get(1)?,
                total_input_tokens: row.get(2)?,
                total_output_tokens: row.get(3)?,
                total_cost_usd: row.get(4)?,
                avg_latency_ms: row.get(5)?,
                error_count: row.get(6)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Per-endpoint cost and usage breakdown, ordered by total cost descending.
    pub fn stats_by_endpoint(&self) -> Result<Vec<EndpointStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT endpoint,
                    COUNT(*) as calls,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(AVG(latency_ms), 0.0),
                    COUNT(CASE WHEN error IS NOT NULL OR status_code >= 400 OR status_code = 0 THEN 1 END)
             FROM calls
             GROUP BY endpoint
             ORDER BY SUM(cost_usd) DESC NULLS LAST",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(EndpointStats {
                endpoint: row.get(0)?,
                calls: row.get(1)?,
                total_input_tokens: row.get(2)?,
                total_output_tokens: row.get(3)?,
                total_cost_usd: row.get(4)?,
                avg_latency_ms: row.get(5)?,
                error_count: row.get(6)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Delete all records and compact the database file.
    pub fn clear(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM calls", [])?;
        self.conn.execute_batch("VACUUM")?;
        Ok(count)
    }

    pub fn total_calls(&self) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM calls", [], |r| r.get(0))?;
        Ok(count)
    }
}

pub fn db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    Ok(PathBuf::from(home).join(".trace").join("trace.db"))
}

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Build a minimal valid CallRecord with sensible defaults.
    fn make_record(id: &str, model: &str, status_code: u16) -> CallRecord {
        CallRecord {
            id: id.to_string(),
            timestamp: now_iso(),
            provider: "openai".to_string(),
            model: model.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code,
            latency_ms: 120,
            ttft_ms: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.001),
            request_body: Some(r#"{"model":"gpt-4o"}"#.to_string()),
            response_body: Some(r#"{"choices":[]}"#.to_string()),
            error: None,
            provider_request_id: None,
        }
    }

    fn make_error_record(id: &str, model: &str, status_code: u16, error: &str) -> CallRecord {
        let mut r = make_record(id, model, status_code);
        r.error = Some(error.to_string());
        r
    }

    // -------------------------------------------------------------------------
    // open_in_memory / schema
    // -------------------------------------------------------------------------

    #[test]
    fn open_in_memory_creates_schema() {
        // If open_in_memory succeeds the calls table exists — verify by running
        // a query that would fail if the schema were absent.
        let store = Store::open_in_memory().expect("should open in-memory DB");
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
            .expect("calls table should exist after init");
        assert_eq!(count, 0);
    }

    #[test]
    fn open_in_memory_indexes_exist() {
        let store = Store::open_in_memory().unwrap();
        // sqlite_master holds the DDL; both indexes must be present.
        let idx_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND tbl_name='calls' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // idx_calls_timestamp + idx_calls_model = 2
        // (Excludes SQLite auto-generated indexes such as the PRIMARY KEY index.)
        assert_eq!(idx_count, 2, "both indexes should exist after init");
    }

    // -------------------------------------------------------------------------
    // insert + round-trip query
    // -------------------------------------------------------------------------

    #[test]
    fn insert_and_query_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let mut record = make_record("id-001", "gpt-4o", 200);
        record.ttft_ms = Some(50);
        record.provider_request_id = Some("req-abc123".to_string());
        store.insert(&record).expect("insert should succeed");

        let filter = QueryFilter::default();
        let results = store.query_filtered(10, &filter).unwrap();
        assert_eq!(results.len(), 1);

        let r = &results[0];
        assert_eq!(r.id, "id-001");
        assert_eq!(r.model, "gpt-4o");
        assert_eq!(r.status_code, 200);
        assert_eq!(r.latency_ms, 120);
        assert_eq!(r.ttft_ms, Some(50));
        assert_eq!(r.input_tokens, Some(100));
        assert_eq!(r.output_tokens, Some(50));
        assert!((r.cost_usd.unwrap() - 0.001).abs() < 1e-9);
        assert_eq!(r.provider, "openai");
        assert_eq!(r.endpoint, "/v1/chat/completions");
        assert_eq!(r.provider_request_id, Some("req-abc123".to_string()));
        assert!(r.error.is_none());
    }

    #[test]
    fn insert_preserves_null_optional_fields() {
        let store = Store::open_in_memory().unwrap();
        let mut record = make_record("id-null", "gpt-4o", 200);
        record.input_tokens = None;
        record.output_tokens = None;
        record.cost_usd = None;
        record.request_body = None;
        record.response_body = None;
        record.ttft_ms = None;
        record.provider_request_id = None;
        store.insert(&record).unwrap();

        let results = store.query_filtered(10, &QueryFilter::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].input_tokens.is_none());
        assert!(results[0].output_tokens.is_none());
        assert!(results[0].cost_usd.is_none());
        assert!(results[0].request_body.is_none());
        assert!(results[0].response_body.is_none());
        assert!(results[0].ttft_ms.is_none());
        assert!(results[0].provider_request_id.is_none());
    }

    #[test]
    fn insert_multiple_records_query_returns_all() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5u32 {
            store.insert(&make_record(&format!("id-{i}"), "gpt-4o", 200)).unwrap();
        }
        let results = store.query_filtered(100, &QueryFilter::default()).unwrap();
        assert_eq!(results.len(), 5);
    }

    // -------------------------------------------------------------------------
    // query_filtered — errors_only
    // -------------------------------------------------------------------------

    #[test]
    fn query_filtered_errors_only_excludes_successful_calls() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("ok-1", "gpt-4o", 200)).unwrap();
        store.insert(&make_record("ok-2", "gpt-4o", 201)).unwrap();

        let filter = QueryFilter { errors_only: true, ..Default::default() };
        let results = store.query_filtered(100, &filter).unwrap();
        assert!(results.is_empty(), "no errors should be returned for 2xx calls");
    }

    #[test]
    fn query_filtered_errors_only_includes_status_400() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("ok", "gpt-4o", 200)).unwrap();
        store.insert(&make_record("bad", "gpt-4o", 400)).unwrap();

        let filter = QueryFilter { errors_only: true, ..Default::default() };
        let results = store.query_filtered(100, &filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bad");
    }

    #[test]
    fn query_filtered_errors_only_includes_status_500() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("ok", "gpt-4o", 200)).unwrap();
        store.insert(&make_record("err", "gpt-4o", 500)).unwrap();

        let filter = QueryFilter { errors_only: true, ..Default::default() };
        let results = store.query_filtered(100, &filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "err");
    }

    #[test]
    fn query_filtered_errors_only_includes_status_code_zero() {
        // status_code = 0 means the upstream connection failed before any HTTP
        // response was received — must be treated as an error.
        let store = Store::open_in_memory().unwrap();
        let mut r = make_record("conn-fail", "gpt-4o", 0);
        r.error = Some("connection refused".to_string());
        store.insert(&r).unwrap();
        store.insert(&make_record("ok", "gpt-4o", 200)).unwrap();

        let filter = QueryFilter { errors_only: true, ..Default::default() };
        let results = store.query_filtered(100, &filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "conn-fail");
    }

    #[test]
    fn query_filtered_errors_only_includes_record_with_error_field_set() {
        // A record can have status 200 but still carry an error message — it
        // should be included in errors_only results.
        let store = Store::open_in_memory().unwrap();
        let r = make_error_record("warn", "gpt-4o", 200, "upstream returned truncated body");
        store.insert(&r).unwrap();
        store.insert(&make_record("ok", "gpt-4o", 200)).unwrap();

        let filter = QueryFilter { errors_only: true, ..Default::default() };
        let results = store.query_filtered(100, &filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "warn");
    }

    // -------------------------------------------------------------------------
    // query_filtered — model substring filter
    // -------------------------------------------------------------------------

    #[test]
    fn query_filtered_model_exact_match() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("a", "gpt-4o", 200)).unwrap();
        store.insert(&make_record("b", "claude-3-5-sonnet-20241022", 200)).unwrap();

        let filter = QueryFilter {
            model: Some("gpt-4o".to_string()),
            ..Default::default()
        };
        let results = store.query_filtered(100, &filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[test]
    fn query_filtered_model_substring_match() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("a", "gpt-4o-2024-11-20", 200)).unwrap();
        store.insert(&make_record("b", "gpt-4o-mini", 200)).unwrap();
        store.insert(&make_record("c", "claude-opus-4", 200)).unwrap();

        // "gpt-4o" is a substring of both gpt-4o-... records.
        let filter = QueryFilter {
            model: Some("gpt-4o".to_string()),
            ..Default::default()
        };
        let results = store.query_filtered(100, &filter).unwrap();
        assert_eq!(results.len(), 2);
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
    }

    #[test]
    fn query_filtered_model_no_match_returns_empty() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("a", "gpt-4o", 200)).unwrap();

        let filter = QueryFilter {
            model: Some("claude".to_string()),
            ..Default::default()
        };
        let results = store.query_filtered(100, &filter).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn query_filtered_no_model_filter_returns_all() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("a", "gpt-4o", 200)).unwrap();
        store.insert(&make_record("b", "claude-opus-4", 200)).unwrap();

        let filter = QueryFilter::default();
        let results = store.query_filtered(100, &filter).unwrap();
        assert_eq!(results.len(), 2);
    }

    // -------------------------------------------------------------------------
    // query_filtered — limit
    // -------------------------------------------------------------------------

    #[test]
    fn query_filtered_limit_respected() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..10u32 {
            store.insert(&make_record(&format!("id-{i}"), "gpt-4o", 200)).unwrap();
        }
        let results = store.query_filtered(3, &QueryFilter::default()).unwrap();
        assert_eq!(results.len(), 3);
    }

    // -------------------------------------------------------------------------
    // get_by_id
    // -------------------------------------------------------------------------

    #[test]
    fn get_by_id_returns_correct_record() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("aabbccdd-1111-2222-3333-444455556666", "gpt-4o", 200)).unwrap();
        store.insert(&make_record("bbcc0000-0000-0000-0000-000000000000", "claude", 200)).unwrap();

        let result = store.get_by_id("aabbccdd").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, "aabbccdd-1111-2222-3333-444455556666");
    }

    #[test]
    fn get_by_id_returns_none_for_missing() {
        let store = Store::open_in_memory().unwrap();
        let result = store.get_by_id("nonexistent").unwrap();
        assert!(result.is_none());
    }

    // -------------------------------------------------------------------------
    // query_after
    // -------------------------------------------------------------------------

    #[test]
    fn query_after_returns_only_newer_records() {
        let store = Store::open_in_memory().unwrap();

        // Insert a record with an explicit old timestamp.
        let mut old = make_record("old-id", "gpt-4o", 200);
        old.timestamp = "2020-01-01T00:00:00.000Z".to_string();
        store.insert(&old).unwrap();

        // Insert a "new" record with the current time.
        store.insert(&make_record("new-id", "gpt-4o", 200)).unwrap();

        let filter = QueryFilter::default();
        let results = store.query_after("2020-06-01T00:00:00.000Z", &filter, 100).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "new-id");
    }

    #[test]
    fn query_after_returns_empty_when_nothing_newer() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("id-1", "gpt-4o", 200)).unwrap();

        let filter = QueryFilter::default();
        // After a far-future timestamp — nothing should match.
        let results = store.query_after("9999-01-01T00:00:00.000Z", &filter, 100).unwrap();
        assert!(results.is_empty());
    }

    // -------------------------------------------------------------------------
    // prune_older_than
    // -------------------------------------------------------------------------

    #[test]
    fn prune_older_than_removes_old_records() {
        let store = Store::open_in_memory().unwrap();

        // Insert a record with an explicit old timestamp (definitely > 1 day old).
        let mut old = make_record("old-id", "gpt-4o", 200);
        old.timestamp = "2020-01-01T00:00:00.000Z".to_string();
        store.insert(&old).unwrap();

        // Insert a fresh record (current time — will NOT be pruned).
        store.insert(&make_record("new-id", "gpt-4o", 200)).unwrap();

        let deleted = store.prune_older_than(1).unwrap();
        assert_eq!(deleted, 1, "one old record should be pruned");

        let remaining = store.total_calls().unwrap();
        assert_eq!(remaining, 1, "one new record should remain");
    }

    #[test]
    fn prune_older_than_empty_db_returns_zero() {
        let store = Store::open_in_memory().unwrap();
        let deleted = store.prune_older_than(30).unwrap();
        assert_eq!(deleted, 0);
    }

    // -------------------------------------------------------------------------
    // stats
    // -------------------------------------------------------------------------

    #[test]
    fn stats_empty_db_returns_zeroes() {
        let store = Store::open_in_memory().unwrap();
        let s = store.stats().unwrap();
        assert_eq!(s.total_calls, 0);
        assert_eq!(s.total_input_tokens, 0);
        assert_eq!(s.total_output_tokens, 0);
        assert_eq!(s.total_cost_usd, 0.0);
        assert_eq!(s.avg_latency_ms, 0.0);
        assert_eq!(s.error_count, 0);
        assert_eq!(s.calls_last_hour, 0);
    }

    #[test]
    fn stats_correct_aggregates() {
        let store = Store::open_in_memory().unwrap();

        // Record 1: 100 in, 50 out, $0.001, 100 ms, success
        let mut r1 = make_record("r1", "gpt-4o", 200);
        r1.latency_ms = 100;
        r1.input_tokens = Some(100);
        r1.output_tokens = Some(50);
        r1.cost_usd = Some(0.001);
        store.insert(&r1).unwrap();

        // Record 2: 200 in, 100 out, $0.002, 300 ms, success
        let mut r2 = make_record("r2", "gpt-4o", 200);
        r2.latency_ms = 300;
        r2.input_tokens = Some(200);
        r2.output_tokens = Some(100);
        r2.cost_usd = Some(0.002);
        store.insert(&r2).unwrap();

        // Record 3: error
        let r3 = make_error_record("r3", "gpt-4o", 500, "server error");
        store.insert(&r3).unwrap();

        let s = store.stats().unwrap();
        assert_eq!(s.total_calls, 3);
        assert_eq!(s.total_input_tokens, 100 + 200 + 100); // r3 also has tokens from make_error_record
        assert_eq!(s.total_output_tokens, 50 + 100 + 50);
        // cost: 0.001 + 0.002 + 0.001 = 0.004
        assert!((s.total_cost_usd - 0.004).abs() < 1e-9);
        // avg latency: (100 + 300 + 120) / 3 = 173.33...
        assert!((s.avg_latency_ms - (100.0 + 300.0 + 120.0) / 3.0).abs() < 0.01);
        assert_eq!(s.error_count, 1);
        // calls_last_hour: all 3 were inserted with now_iso() which is current time.
        assert_eq!(s.calls_last_hour, 3);
    }

    #[test]
    fn stats_error_count_includes_status_400_and_status_0() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("ok", "gpt-4o", 200)).unwrap();
        store.insert(&make_record("bad-req", "gpt-4o", 400)).unwrap();
        // status 0 = connection failure
        let mut r = make_record("conn-fail", "gpt-4o", 0);
        r.error = Some("timeout".to_string());
        store.insert(&r).unwrap();

        let s = store.stats().unwrap();
        assert_eq!(s.total_calls, 3);
        assert_eq!(s.error_count, 2);
    }

    // -------------------------------------------------------------------------
    // stats_by_endpoint
    // -------------------------------------------------------------------------

    #[test]
    fn stats_by_endpoint_groups_correctly() {
        let store = Store::open_in_memory().unwrap();

        let mut r1 = make_record("a", "gpt-4o", 200);
        r1.endpoint = "/v1/chat/completions".to_string();
        r1.cost_usd = Some(0.001);
        store.insert(&r1).unwrap();

        let mut r2 = make_record("b", "text-embedding-3-small", 200);
        r2.endpoint = "/v1/embeddings".to_string();
        r2.cost_usd = Some(0.0001);
        store.insert(&r2).unwrap();

        let endpoint_stats = store.stats_by_endpoint().unwrap();
        assert_eq!(endpoint_stats.len(), 2);

        let chat = endpoint_stats.iter().find(|e| e.endpoint == "/v1/chat/completions").unwrap();
        assert_eq!(chat.calls, 1);

        let emb = endpoint_stats.iter().find(|e| e.endpoint == "/v1/embeddings").unwrap();
        assert_eq!(emb.calls, 1);
    }

    #[test]
    fn stats_by_endpoint_empty_db_returns_empty() {
        let store = Store::open_in_memory().unwrap();
        let result = store.stats_by_endpoint().unwrap();
        assert!(result.is_empty());
    }

    // -------------------------------------------------------------------------
    // clear
    // -------------------------------------------------------------------------

    #[test]
    fn clear_removes_all_records_and_returns_count() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5u32 {
            store.insert(&make_record(&format!("id-{i}"), "gpt-4o", 200)).unwrap();
        }

        let deleted = store.clear().unwrap();
        assert_eq!(deleted, 5, "clear should report the number of deleted rows");

        let total = store.total_calls().unwrap();
        assert_eq!(total, 0, "no records should remain after clear");
    }

    #[test]
    fn clear_on_empty_db_returns_zero() {
        let store = Store::open_in_memory().unwrap();
        let deleted = store.clear().unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn clear_allows_new_inserts_after() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("before", "gpt-4o", 200)).unwrap();
        store.clear().unwrap();

        store.insert(&make_record("after", "gpt-4o", 200)).unwrap();
        let total = store.total_calls().unwrap();
        assert_eq!(total, 1);
    }

    // -------------------------------------------------------------------------
    // total_calls
    // -------------------------------------------------------------------------

    #[test]
    fn total_calls_matches_number_of_inserts() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.total_calls().unwrap(), 0);

        for i in 0..7u32 {
            store.insert(&make_record(&format!("id-{i}"), "gpt-4o", 200)).unwrap();
        }
        assert_eq!(store.total_calls().unwrap(), 7);
    }

    // -------------------------------------------------------------------------
    // stats_by_model
    // -------------------------------------------------------------------------

    #[test]
    fn stats_by_model_groups_correctly() {
        let store = Store::open_in_memory().unwrap();

        let mut r1 = make_record("a", "gpt-4o", 200);
        r1.input_tokens = Some(100);
        r1.output_tokens = Some(50);
        r1.cost_usd = Some(0.001);
        store.insert(&r1).unwrap();

        let mut r2 = make_record("b", "gpt-4o", 200);
        r2.input_tokens = Some(200);
        r2.output_tokens = Some(80);
        r2.cost_usd = Some(0.002);
        store.insert(&r2).unwrap();

        let mut r3 = make_record("c", "claude-opus-4", 200);
        r3.input_tokens = Some(50);
        r3.output_tokens = Some(20);
        r3.cost_usd = Some(0.005);
        store.insert(&r3).unwrap();

        let model_stats = store.stats_by_model().unwrap();
        assert_eq!(model_stats.len(), 2);

        // Results are ordered by total cost DESC — claude-opus-4 costs more.
        let top = &model_stats[0];
        assert_eq!(top.model, "claude-opus-4");
        assert_eq!(top.calls, 1);
        assert!((top.total_cost_usd - 0.005).abs() < 1e-9);

        let second = &model_stats[1];
        assert_eq!(second.model, "gpt-4o");
        assert_eq!(second.calls, 2);
        assert_eq!(second.total_input_tokens, 300);
        assert_eq!(second.total_output_tokens, 130);
        assert!((second.total_cost_usd - 0.003).abs() < 1e-9);
    }

    #[test]
    fn stats_by_model_empty_db_returns_empty_vec() {
        let store = Store::open_in_memory().unwrap();
        let result = store.stats_by_model().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn stats_by_model_error_count_per_model() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_record("ok", "gpt-4o", 200)).unwrap();
        store.insert(&make_error_record("bad", "gpt-4o", 500, "err")).unwrap();

        let model_stats = store.stats_by_model().unwrap();
        assert_eq!(model_stats.len(), 1);
        assert_eq!(model_stats[0].error_count, 1);
    }
}
