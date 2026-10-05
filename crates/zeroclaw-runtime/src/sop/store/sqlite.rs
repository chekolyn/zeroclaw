//! Durable SQLite-backed [`SopRunStore`] (EPIC B).

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use super::model::{
    ClaimToken, PersistedRun, ProposalRecord, ProposalStatus, RetentionPolicy, SOP_STORE_VERSION,
    SopEventRecord,
};
use super::{RETAINED_TERMINAL_ROLLBACK_HOLDER, SopRunStore, StoreError, pending_capacity_member};
use crate::sop::types::{SopEvent, SopRun, SopRunStatus, SopTriggerSource};

/// Default claim lease. The concurrency tick (EPIC A1) renews via `heartbeat_claim`;
/// the reaper reclaims claims past this without a heartbeat.
const DEFAULT_CLAIM_LEASE_SECS: i64 = 3600;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS sop_runs (
    run_id           TEXT PRIMARY KEY,
    revision         INTEGER NOT NULL,
    terminal         INTEGER NOT NULL DEFAULT 0,
    last_progress_at TEXT,
    json             TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sop_runs_terminal ON sop_runs(terminal);
CREATE TABLE IF NOT EXISTS sop_events (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id  TEXT NOT NULL,
    ts      TEXT NOT NULL,
    kind    TEXT NOT NULL,
    actor   TEXT,
    reason  TEXT,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sop_events_run ON sop_events(run_id);
CREATE TABLE IF NOT EXISTS sop_claims (
    run_id        TEXT PRIMARY KEY,
    sop_name      TEXT NOT NULL,
    lease_expires TEXT NOT NULL,
    json          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sop_claims_sop ON sop_claims(sop_name);
CREATE TABLE IF NOT EXISTS sop_proposals (
    id     TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    json   TEXT NOT NULL
);
";

fn sql_err(e: rusqlite::Error) -> StoreError {
    StoreError::Backend(format!("sqlite: {e}"))
}

fn status_str(s: ProposalStatus) -> &'static str {
    match s {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Applied => "applied",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Quarantined => "quarantined",
        ProposalStatus::Stale => "stale",
    }
}

fn guard_revision(
    run_id: &str,
    stored_rev: u64,
    stored_json: &str,
    incoming_rev: u64,
    incoming_json: &str,
) -> Result<(), StoreError> {
    if incoming_rev < stored_rev {
        return Err(StoreError::StaleRevision {
            run_id: run_id.to_string(),
            have: incoming_rev,
            found: stored_rev,
        });
    }
    if incoming_rev == stored_rev && stored_json != incoming_json {
        return Err(StoreError::RevisionConflict {
            run_id: run_id.to_string(),
            revision: incoming_rev,
        });
    }
    Ok(())
}

/// Durable run store. Selected by `build_run_store` when `persist_runs = true`
/// with the default `"sqlite"` backend.
pub struct SqliteRunStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteRunStore {
    /// Open (creating if absent) a database at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(sql_err)?;
        Self::init(conn)
    }

    /// In-memory database (tests + the no-durability fallback).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Backend("sqlite run store lock poisoned".into()))
    }
}

/// Quarantine a row whose `json` cannot be parsed as a [`PersistedRun`]
/// (a torn write — WAL `synchronous = NORMAL` under a process kill — or a
/// row persisted by an older store schema).
///
/// Left as-is such a row wedges the engine's whole self-heal chain: the
/// load surfaces used to abort on the first unparseable row, so no runs
/// rehydrated, the stuck-run reaper (an in-memory scan) never saw the
/// orphans, and the stale `sop_claims` left by the dead process blocked
/// the start-gate ("execution slots full") until lease expiry —
/// recurring on every pod restart. The row can never be resumed, so the
/// honest action is a terminal tombstone: mark it terminal, replace the
/// json with a minimal parseable envelope (so every reader — this store
/// AND the sidecar's `json_extract` collectors — heals), release any
/// claim it still holds, and preserve the original bytes plus the parse
/// error as a `run_quarantined` ledger event (pruned together with the
/// row). Best-effort: a quarantine write failure is logged and the scan
/// continues — a row that cannot be quarantined is still skipped, so the
/// load itself never fails. Returns the tombstone the row was replaced
/// with (`None` when the quarantine write failed), so a caller that just
/// scanned the row can surface it in its result instead of leaving the
/// row invisible until the next load.
fn quarantine_poison_row(
    conn: &Connection,
    run_id: &str,
    revision: u64,
    json: &str,
    error: &serde_json::Error,
) -> Option<PersistedRun> {
    let now = Utc::now().to_rfc3339();
    let tombstone = PersistedRun {
        version: SOP_STORE_VERSION,
        revision,
        run: SopRun {
            run_id: run_id.to_string(),
            // The original SOP name died with the torn bytes; a stable
            // sentinel keeps group-bys honest instead of guessing.
            sop_name: "quarantined".to_string(),
            initiating_agent: None,
            trigger_event: SopEvent {
                source: SopTriggerSource::Manual,
                topic: None,
                payload: None,
                timestamp: now.clone(),
            },
            frame_marker_id: String::new(),
            status: SopRunStatus::Failed,
            current_step: 0,
            total_steps: 0,
            started_at: now.clone(),
            completed_at: Some(now.clone()),
            failure_reason: Some(format!("quarantined: persisted row unparseable ({error})")),
            step_results: Vec::new(),
            waiting_since: None,
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
        },
        last_progress_at: now.clone(),
        redacted: false,
        trigger_source: SopTriggerSource::Manual,
    };
    let reason = format!("persisted row unparseable: {error}");
    let forensics = serde_json::json!({
        "parse_error": error.to_string(),
        "original_json": json,
    });
    let result: Result<(), StoreError> = (|| {
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        tx.execute(
            "UPDATE sop_runs SET terminal=1, last_progress_at=?2, json=?3 WHERE run_id=?1",
            params![run_id, now, serde_json::to_string(&tombstone)?],
        )
        .map_err(sql_err)?;
        tx.execute("DELETE FROM sop_claims WHERE run_id=?1", params![run_id])
            .map_err(sql_err)?;
        tx.execute(
            "INSERT INTO sop_events (run_id, ts, kind, actor, reason, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                now,
                "run_quarantined",
                None::<String>,
                reason,
                forensics.to_string()
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "parse_error": error.to_string(),
                    })),
                "SOP store: quarantined an unparseable run row (terminal tombstone written, \
                 stale claim released, forensics appended to sop_events)"
            );
            Some(tombstone)
        }
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "parse_error": error.to_string(),
                        "error": e.to_string(),
                    })),
                "SOP store: could not quarantine an unparseable run row; it stays skipped and \
                 the quarantine is retried on the next scan"
            );
            None
        }
    }
}

impl SopRunStore for SqliteRunStore {
    fn save_run(&self, run: &PersistedRun) -> Result<(), StoreError> {
        let g = self.lock()?;
        let id = run.run_id();
        let json = serde_json::to_string(run)?;
        let existing: Option<(i64, String)> = g
            .query_row(
                "SELECT revision, json FROM sop_runs WHERE run_id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((rev, existing_json)) = existing {
            guard_revision(id, rev as u64, &existing_json, run.revision, &json)?;
        }
        g.execute(
            "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
             VALUES (?1, ?2, 0, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET
                 revision=excluded.revision,
                 terminal=excluded.terminal,
                 last_progress_at=excluded.last_progress_at,
                 json=excluded.json",
            params![id, run.revision as i64, run.last_progress_at, json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn save_run_with_pending_capacity(
        &self,
        run: &PersistedRun,
        max_pending: usize,
    ) -> Result<bool, StoreError> {
        let mut g = self.lock()?;
        let id = run.run_id();
        let json = serde_json::to_string(run)?;
        let tx = g.transaction().map_err(sql_err)?;

        if max_pending > 0 {
            let pending = {
                let mut stmt = tx
                    .prepare("SELECT json FROM sop_runs WHERE terminal=0")
                    .map_err(sql_err)?;
                let mut rows = stmt.query([]).map_err(sql_err)?;
                let mut pending = 0usize;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    let raw: String = row.get(0).map_err(sql_err)?;
                    let existing: PersistedRun = serde_json::from_str(&raw)?;
                    if pending_capacity_member(&existing, run) {
                        pending += 1;
                    }
                }
                pending
            };
            if pending >= max_pending {
                return Ok(false);
            }
        }

        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT revision, json FROM sop_runs WHERE run_id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((rev, existing_json)) = existing {
            guard_revision(id, rev as u64, &existing_json, run.revision, &json)?;
        }
        tx.execute(
            "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
             VALUES (?1, ?2, 0, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET
                 revision=excluded.revision,
                 terminal=excluded.terminal,
                 last_progress_at=excluded.last_progress_at,
                 json=excluded.json",
            params![id, run.revision as i64, run.last_progress_at, json],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    fn save_run_with_event(
        &self,
        run: &PersistedRun,
        ev: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        let mut g = self.lock()?;
        let id = run.run_id();
        let json = serde_json::to_string(run)?;
        let payload = serde_json::to_string(&ev.payload)?;
        let tx = g.transaction().map_err(sql_err)?;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT revision, json FROM sop_runs WHERE run_id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((rev, existing_json)) = existing {
            guard_revision(id, rev as u64, &existing_json, run.revision, &json)?;
        }
        tx.execute(
            "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
             VALUES (?1, ?2, 0, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET
                 revision=excluded.revision,
                 terminal=excluded.terminal,
                 last_progress_at=excluded.last_progress_at,
                 json=excluded.json",
            params![id, run.revision as i64, run.last_progress_at, json],
        )
        .map_err(sql_err)?;
        tx.execute(
            "INSERT INTO sop_events (run_id, ts, kind, actor, reason, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ev.run_id, ev.ts, ev.kind, ev.actor, ev.reason, payload],
        )
        .map_err(sql_err)?;
        let seq = tx.last_insert_rowid() as u64;
        tx.commit().map_err(sql_err)?;
        Ok(seq)
    }

    fn finish_run(&self, run_id: &str, terminal: &PersistedRun) -> Result<(), StoreError> {
        let mut g = self.lock()?;
        let json = serde_json::to_string(terminal)?;
        let tx = g.transaction().map_err(sql_err)?;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT revision, json FROM sop_runs WHERE run_id=?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((rev, existing_json)) = existing {
            guard_revision(run_id, rev as u64, &existing_json, terminal.revision, &json)?;
        }
        tx.execute(
            "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
             VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET
                 revision=excluded.revision,
                 terminal=1,
                 last_progress_at=excluded.last_progress_at,
                 json=excluded.json",
            params![
                run_id,
                terminal.revision as i64,
                terminal.last_progress_at,
                json
            ],
        )
        .map_err(sql_err)?;
        tx.execute("DELETE FROM sop_claims WHERE run_id=?1", params![run_id])
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    fn finish_run_with_event(
        &self,
        run_id: &str,
        terminal: &PersistedRun,
        ev: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        let mut g = self.lock()?;
        let json = serde_json::to_string(terminal)?;
        let payload = serde_json::to_string(&ev.payload)?;
        let tx = g.transaction().map_err(sql_err)?;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT revision, json FROM sop_runs WHERE run_id=?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((rev, existing_json)) = existing {
            guard_revision(run_id, rev as u64, &existing_json, terminal.revision, &json)?;
        }
        tx.execute(
            "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
             VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET
                 revision=excluded.revision,
                 terminal=1,
                 last_progress_at=excluded.last_progress_at,
                 json=excluded.json",
            params![
                run_id,
                terminal.revision as i64,
                terminal.last_progress_at,
                json
            ],
        )
        .map_err(sql_err)?;
        tx.execute("DELETE FROM sop_claims WHERE run_id=?1", params![run_id])
            .map_err(sql_err)?;
        tx.execute(
            "INSERT INTO sop_events (run_id, ts, kind, actor, reason, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ev.run_id, ev.ts, ev.kind, ev.actor, ev.reason, payload],
        )
        .map_err(sql_err)?;
        let seq = tx.last_insert_rowid() as u64;
        tx.commit().map_err(sql_err)?;
        Ok(seq)
    }

    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError> {
        let g = self.lock()?;
        // Tolerant scan: a poison row (torn write or a legacy-schema row) must
        // not abort the restore — one unparseable row used to fail the WHOLE
        // load, so nothing rehydrated and the stale claims of every orphan
        // wedged the start-gate until lease expiry (2026-10-03..05 incident;
        // see `quarantine_poison_row`). Poison rows are collected during the
        // scan and quarantined once the cursor is closed (a write under an
        // open read cursor on the same table is avoidable; avoid it).
        let mut stmt = g
            .prepare("SELECT run_id, revision, json FROM sop_runs WHERE terminal=0")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        let mut poison: Vec<(String, u64, String, serde_json::Error)> = Vec::new();
        for row in rows {
            let (run_id, revision, json) = row.map_err(sql_err)?;
            match serde_json::from_str(&json) {
                Ok(pr) => out.push(pr),
                Err(e) => poison.push((run_id, revision as u64, json, e)),
            }
        }
        drop(stmt);
        for (run_id, revision, json, error) in poison {
            // The tombstone is terminal — deliberately NOT added to the
            // active set.
            let _ = quarantine_poison_row(&g, &run_id, revision, &json, &error);
        }
        Ok(out)
    }

    fn load_terminal_runs(&self, limit: usize) -> Result<Vec<PersistedRun>, StoreError> {
        let g = self.lock()?;
        // Tolerant scan (see `load_active_runs`): a terminal poison row — e.g.
        // a torn write the reaper marked terminal — must not abort the boot
        // seeding of the finished-runs window. It is quarantined (tombstoned)
        // so the next reader, including the sidecar's `json_extract`
        // collectors on this same DB, sees a parseable row.
        let mut stmt = g
            .prepare("SELECT run_id, revision, json FROM sop_runs WHERE terminal=1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out: Vec<PersistedRun> = Vec::new();
        let mut poison: Vec<(String, u64, String, serde_json::Error)> = Vec::new();
        for row in rows {
            let (run_id, revision, json) = row.map_err(sql_err)?;
            match serde_json::from_str(&json) {
                Ok(pr) => out.push(pr),
                Err(e) => poison.push((run_id, revision as u64, json, e)),
            }
        }
        drop(stmt);
        for (run_id, revision, json, error) in poison {
            if let Some(tombstone) = quarantine_poison_row(&g, &run_id, revision, &json, &error) {
                // The row IS terminal — surface the tombstone so this load
                // already includes it (a boot seed must not silently drop
                // rows it just healed).
                out.push(tombstone);
            }
        }
        out.sort_by(|a, b| b.run.started_at.cmp(&a.run.started_at));
        if limit > 0 && out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    fn load_run(&self, run_id: &str) -> Result<Option<PersistedRun>, StoreError> {
        let g = self.lock()?;
        let json: Option<String> = g
            .query_row(
                "SELECT json FROM sop_runs WHERE run_id=?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    fn last_terminal_completed_at(&self, sop_name: &str) -> Result<Option<String>, StoreError> {
        let g = self.lock()?;
        // `completed_at` lives inside the run JSON, not a column. Pull the
        // successful terminal rows for this SOP (bounded by retention) and take
        // the max completion. ISO-8601 UTC ("...Z") timestamps sort lexically
        // in completion order. A poison row is skipped, not fatal: one bad row
        // must not break every SOP's cooldown read (it is tombstoned by the
        // boot-time terminal scan in `load_terminal_runs`).
        let mut stmt = g
            .prepare("SELECT json FROM sop_runs WHERE terminal=1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut latest: Option<String> = None;
        for row in rows {
            let Ok(pr) = serde_json::from_str::<PersistedRun>(&row.map_err(sql_err)?) else {
                continue;
            };
            if pr.run.sop_name != sop_name
                || pr.run.status != crate::sop::types::SopRunStatus::Completed
            {
                continue;
            }
            if let Some(completed) = pr.run.completed_at
                && latest.as_deref().is_none_or(|cur| cur < completed.as_str())
            {
                latest = Some(completed);
            }
        }
        Ok(latest)
    }

    fn try_claim_run(
        &self,
        run_id: &str,
        sop_name: &str,
        per_sop_cap: usize,
        global_cap: usize,
    ) -> Result<Option<ClaimToken>, StoreError> {
        let g = self.lock()?;
        // A terminal run is not re-claimable.
        let terminal: Option<i64> = g
            .query_row(
                "SELECT terminal FROM sop_runs WHERE run_id=?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if terminal == Some(1) {
            return Ok(None);
        }
        // Already claimed?
        let claimed: Option<i64> = g
            .query_row(
                "SELECT 1 FROM sop_claims WHERE run_id=?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if claimed.is_some() {
            return Ok(None);
        }
        // Both caps are enforced while the connection lock is held, so the
        // read-counts + insert are atomic within the process (mirrors the engine
        // `can_start`). A cap of 0 admits nothing.
        let per_sop: i64 = g
            .query_row(
                "SELECT COUNT(*) FROM sop_claims WHERE sop_name=?1",
                params![sop_name],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if per_sop as usize >= per_sop_cap {
            return Ok(None);
        }
        let total: i64 = g
            .query_row("SELECT COUNT(*) FROM sop_claims", [], |r| r.get(0))
            .map_err(sql_err)?;
        if total as usize >= global_cap {
            return Ok(None);
        }
        let now = Utc::now();
        let token = ClaimToken {
            run_id: run_id.to_string(),
            sop_name: sop_name.to_string(),
            claimed_at: now.to_rfc3339(),
            lease_expires: (now + Duration::seconds(DEFAULT_CLAIM_LEASE_SECS)).to_rfc3339(),
            holder: format!("pid-{}", std::process::id()),
        };
        let json = serde_json::to_string(&token)?;
        g.execute(
            "INSERT INTO sop_claims (run_id, sop_name, lease_expires, json) VALUES (?1, ?2, ?3, ?4)",
            params![token.run_id, token.sop_name, token.lease_expires, json],
        )
        .map_err(sql_err)?;
        Ok(Some(token))
    }

    fn renew_claim_for_restore(
        &self,
        run_id: &str,
        sop_name: &str,
    ) -> Result<ClaimToken, StoreError> {
        let g = self.lock()?;
        // No cap check: a restored run was already admitted before the restart.
        // Upsert so a re-run of restore is idempotent and a stale lease is refreshed.
        let now = Utc::now();
        let token = ClaimToken {
            run_id: run_id.to_string(),
            sop_name: sop_name.to_string(),
            claimed_at: now.to_rfc3339(),
            lease_expires: (now + Duration::seconds(DEFAULT_CLAIM_LEASE_SECS)).to_rfc3339(),
            holder: format!("pid-{}", std::process::id()),
        };
        let json = serde_json::to_string(&token)?;
        g.execute(
            "INSERT INTO sop_claims (run_id, sop_name, lease_expires, json) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET
                 sop_name=excluded.sop_name,
                 lease_expires=excluded.lease_expires,
                 json=excluded.json",
            params![token.run_id, token.sop_name, token.lease_expires, json],
        )
        .map_err(sql_err)?;
        Ok(token)
    }

    fn mark_claim_retained_after_terminal_rollback(&self, run_id: &str) -> Result<(), StoreError> {
        let g = self.lock()?;
        let raw: Option<String> = g
            .query_row(
                "SELECT json FROM sop_claims WHERE run_id=?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(raw) = raw else {
            return Ok(());
        };
        let mut token: ClaimToken = serde_json::from_str(&raw)?;
        token.holder = RETAINED_TERMINAL_ROLLBACK_HOLDER.to_string();
        g.execute(
            "UPDATE sop_claims SET json=?1 WHERE run_id=?2",
            params![serde_json::to_string(&token)?, run_id],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn has_retained_terminal_rollback_claim(&self, run_id: &str) -> Result<bool, StoreError> {
        let g = self.lock()?;
        let raw: Option<String> = g
            .query_row(
                "SELECT json FROM sop_claims WHERE run_id=?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let token: ClaimToken = serde_json::from_str(&raw)?;
        Ok(token.holder == RETAINED_TERMINAL_ROLLBACK_HOLDER)
    }

    fn claim_counts(&self, sop_name: &str) -> Result<(usize, usize), StoreError> {
        let g = self.lock()?;
        let per_sop: i64 = g
            .query_row(
                "SELECT COUNT(*) FROM sop_claims WHERE sop_name=?1",
                params![sop_name],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let total: i64 = g
            .query_row("SELECT COUNT(*) FROM sop_claims", [], |r| r.get(0))
            .map_err(sql_err)?;
        Ok((per_sop as usize, total as usize))
    }

    fn heartbeat_claim(&self, token: &ClaimToken) -> Result<(), StoreError> {
        let g = self.lock()?;
        let lease = (Utc::now() + Duration::seconds(DEFAULT_CLAIM_LEASE_SECS)).to_rfc3339();
        g.execute(
            "UPDATE sop_claims SET lease_expires=?1 WHERE run_id=?2",
            params![lease, token.run_id],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn release_claim(&self, token: &ClaimToken) -> Result<(), StoreError> {
        self.lock()?
            .execute(
                "DELETE FROM sop_claims WHERE run_id=?1",
                params![token.run_id],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    fn expired_claims(&self, now_iso: &str) -> Result<Vec<ClaimToken>, StoreError> {
        let g = self.lock()?;
        let mut stmt = g
            .prepare("SELECT run_id, json FROM sop_claims WHERE lease_expires <= ?1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![now_iso], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        let mut torn: Vec<(String, String)> = Vec::new();
        for row in rows {
            let (run_id, json) = row.map_err(sql_err)?;
            match serde_json::from_str(&json) {
                Ok(token) => out.push(token),
                Err(e) => {
                    // A torn CLAIM row (same WAL crash-window class as a torn
                    // run row) must not abort the whole expired-claims scan —
                    // that would silently disable lease reaping and wedge the
                    // start-gate on every stale claim. The row is garbage its
                    // holder died writing; its run_id column is the only
                    // field needed to release the slot.
                    torn.push((run_id, e.to_string()));
                }
            }
        }
        drop(stmt);
        for (run_id, parse_error) in torn {
            let _ = g.execute("DELETE FROM sop_claims WHERE run_id=?1", params![run_id]);
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "parse_error": parse_error,
                    })),
                "SOP store: released a torn claim row during the expired-claims scan"
            );
        }
        Ok(out)
    }

    fn append_event(&self, ev: &SopEventRecord) -> Result<u64, StoreError> {
        let g = self.lock()?;
        let payload = serde_json::to_string(&ev.payload)?;
        g.execute(
            "INSERT INTO sop_events (run_id, ts, kind, actor, reason, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ev.run_id, ev.ts, ev.kind, ev.actor, ev.reason, payload],
        )
        .map_err(sql_err)?;
        Ok(g.last_insert_rowid() as u64)
    }

    fn list_events(&self, run_id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        let g = self.lock()?;
        let mut stmt = g
            .prepare(
                "SELECT seq, ts, kind, actor, reason, payload FROM sop_events
                 WHERE run_id=?1 ORDER BY seq",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, ts, kind, actor, reason, payload_s) = row.map_err(sql_err)?;
            out.push(SopEventRecord {
                run_id: run_id.to_string(),
                seq: seq as u64,
                ts,
                kind,
                actor,
                reason,
                payload: serde_json::from_str(&payload_s)?,
            });
        }
        Ok(out)
    }

    fn save_proposal(&self, p: &ProposalRecord) -> Result<(), StoreError> {
        let g = self.lock()?;
        let json = serde_json::to_string(p)?;
        g.execute(
            "INSERT INTO sop_proposals (id, status, json) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, json=excluded.json",
            params![p.id, status_str(p.status), json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        let g = self.lock()?;
        let json: Option<String> = g
            .query_row(
                "SELECT json FROM sop_proposals WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    fn list_proposals(
        &self,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<ProposalRecord>, StoreError> {
        let g = self.lock()?;
        let (sql, bind): (&str, Option<&'static str>) = match status {
            Some(s) => (
                "SELECT json FROM sop_proposals WHERE status=?1",
                Some(status_str(s)),
            ),
            None => ("SELECT json FROM sop_proposals", None),
        };
        let mut stmt = g.prepare(sql).map_err(sql_err)?;
        let mut out = Vec::new();
        if let Some(s) = bind {
            let rows = stmt
                .query_map(params![s], |r| r.get::<_, String>(0))
                .map_err(sql_err)?;
            for row in rows {
                out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
            }
        } else {
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(sql_err)?;
            for row in rows {
                out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
            }
        }
        Ok(out)
    }

    fn prune(&self, policy: &RetentionPolicy) -> Result<usize, StoreError> {
        let g = self.lock()?;
        let mut dropped = 0usize;
        if policy.max_terminal > 0 {
            let total: i64 = g
                .query_row("SELECT COUNT(*) FROM sop_runs WHERE terminal=1", [], |r| {
                    r.get(0)
                })
                .map_err(sql_err)?;
            if total as usize > policy.max_terminal {
                let drop_n = (total as usize - policy.max_terminal) as i64;
                // Oldest terminal runs first (by last_progress_at). Events before runs.
                g.execute(
                    "DELETE FROM sop_events WHERE run_id IN
                       (SELECT run_id FROM sop_runs WHERE terminal=1
                        ORDER BY last_progress_at ASC LIMIT ?1)",
                    params![drop_n],
                )
                .map_err(sql_err)?;
                dropped += g
                    .execute(
                        "DELETE FROM sop_runs WHERE run_id IN
                           (SELECT run_id FROM sop_runs WHERE terminal=1
                            ORDER BY last_progress_at ASC LIMIT ?1)",
                        params![drop_n],
                    )
                    .map_err(sql_err)?;
            }
        }
        if let Some(keep) = policy.keep_secs {
            let cutoff = (Utc::now() - Duration::seconds(keep as i64)).to_rfc3339();
            g.execute(
                "DELETE FROM sop_events WHERE run_id IN
                   (SELECT run_id FROM sop_runs WHERE terminal=1 AND last_progress_at < ?1)",
                params![cutoff],
            )
            .map_err(sql_err)?;
            dropped += g
                .execute(
                    "DELETE FROM sop_runs WHERE terminal=1 AND last_progress_at < ?1",
                    params![cutoff],
                )
                .map_err(sql_err)?;
        }
        Ok(dropped)
    }

    fn health_check(&self) -> bool {
        match self.lock() {
            Ok(g) => g.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok(),
            Err(_) => false,
        }
    }

    fn backend(&self) -> &'static str {
        "sqlite"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sop::types::{SopEvent, SopRun, SopRunStatus, SopTriggerSource};
    use serde_json::json;

    fn run(id: &str, status: SopRunStatus, last_progress: &str) -> PersistedRun {
        let r = SopRun {
            run_id: id.to_string(),
            sop_name: "deploy".to_string(),
            initiating_agent: None,
            trigger_event: SopEvent {
                source: SopTriggerSource::Manual,
                topic: None,
                payload: None,
                timestamp: "t".to_string(),
            },
            frame_marker_id: format!("marker-{id}"),
            status,
            current_step: 0,
            total_steps: 1,
            started_at: last_progress.to_string(),
            completed_at: None,
            failure_reason: None,
            step_results: vec![],
            waiting_since: None,
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
        };
        PersistedRun::new(r, last_progress.to_string(), SopTriggerSource::Manual)
    }

    fn ev(run_id: &str, kind: &str) -> SopEventRecord {
        SopEventRecord {
            run_id: run_id.to_string(),
            seq: 0,
            ts: "t".to_string(),
            kind: kind.to_string(),
            actor: None,
            reason: None,
            payload: json!({}),
        }
    }

    #[test]
    fn runs_save_load_finish_roundtrip() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        s.save_run(&run("r1", SopRunStatus::Running, "1")).unwrap();
        assert_eq!(s.load_active_runs().unwrap().len(), 1);
        assert!(s.load_run("r1").unwrap().is_some());
        // Finishing is a state transition, so it carries a bumped revision.
        let mut terminal = run("r1", SopRunStatus::Completed, "2");
        terminal.revision = 1;
        s.finish_run("r1", &terminal).unwrap();
        assert_eq!(
            s.load_active_runs().unwrap().len(),
            0,
            "terminal excluded from active"
        );
        assert!(
            s.load_run("r1").unwrap().is_some(),
            "terminal still loadable"
        );
        assert_eq!(s.backend(), "sqlite");
        assert!(s.health_check());
    }

    /// Incident regression (2026-10-03..05): ONE poison row in `sop_runs` must
    /// not abort `load_active_runs` — the wholesale failure wedged the engine
    /// (no runs rehydrated → the stuck-run reaper never saw the orphans →
    /// their stale `sop_claims` blocked the start-gate "execution slots full"
    /// until lease expiry, recurring on every pod restart).
    #[test]
    fn load_active_runs_quarantines_poison_row_instead_of_failing() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        s.save_run(&run("good", SopRunStatus::Running, "1"))
            .unwrap();
        // A torn write: non-terminal, invalid JSON, with a stale claim (what
        // the dead process left behind on the durable store).
        s.lock()
            .unwrap()
            .execute(
                "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
                 VALUES ('poison', 3, 0, NULL, ?1)",
                params![r#"{"version":1,"run":{"run_id":"poison"#],
            )
            .unwrap();
        s.lock().unwrap()
            .execute(
                "INSERT INTO sop_claims (run_id, sop_name, lease_expires, json)
                 VALUES ('poison', 'deploy', '2999-01-01T00:00:00Z', ?1)",
                params![r#"{"run_id":"poison","sop_name":"deploy","claimed_at":"t","lease_expires":"2999-01-01T00:00:00Z","holder":"pid-1"}"#],
            )
            .unwrap();

        // Pre-fix this call failed wholesale (StoreError) — now the parseable
        // run still loads.
        let active = s.load_active_runs().unwrap();
        assert_eq!(active.len(), 1, "the parseable run still loads");
        assert_eq!(active[0].run.run_id, "good");

        // The poison row is quarantined: terminal, tombstoned (parseable),
        // and its stale claim released — the gate slot is freed.
        let poison = s.load_run("poison").unwrap().expect("tombstone kept");
        assert_eq!(poison.revision, 3, "tombstone keeps the row's revision");
        assert!(matches!(poison.run.status, SopRunStatus::Failed));
        let claims: i64 = s
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sop_claims", [], |r| r.get(0))
            .unwrap();
        assert_eq!(claims, 0, "quarantine released the stale claim");
        // The tombstone is valid JSON for EVERY reader of this shared DB
        // (the sidecar's json_extract collectors crash on invalid JSON).
        let json_valid: i64 = s
            .lock()
            .unwrap()
            .query_row(
                "SELECT json_valid(json) FROM sop_runs WHERE run_id='poison'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(json_valid, 1, "the tombstone is valid JSON");
        // Forensics: the quarantine is on the ledger, not just in the logs.
        let events: i64 = s
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sop_events WHERE run_id='poison' AND kind='run_quarantined'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1, "a run_quarantined event records the quarantine");
        // Idempotent: a second scan no longer sees the row as active and
        // does not re-quarantine (no second event).
        let active = s.load_active_runs().unwrap();
        assert_eq!(active.len(), 1, "the quarantined row stays terminal");
        let events: i64 = s
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sop_events WHERE run_id='poison' AND kind='run_quarantined'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
    }

    /// The terminal surface must survive (and heal) a poison row too: the
    /// observed torn row was marked terminal by the one-shot reaper and kept
    /// crashing every reader of the terminal set.
    #[test]
    fn load_terminal_runs_quarantines_poison_row_instead_of_failing() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        let mut terminal = run("done", SopRunStatus::Completed, "2");
        terminal.revision = 1;
        s.save_run(&run("done", SopRunStatus::Running, "1"))
            .unwrap();
        s.finish_run("done", &terminal).unwrap();
        s.lock()
            .unwrap()
            .execute(
                "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
                 VALUES ('poison', 0, 1, NULL, ?1)",
                params![r#"{"version":1,"run":{"run_id":"poison"#],
            )
            .unwrap();

        let loaded = s.load_terminal_runs(10).unwrap();
        assert_eq!(
            loaded.len(),
            2,
            "the parseable terminal run loads and the poison row is tombstoned in place"
        );
        assert!(
            loaded.iter().any(
                |pr| pr.run.run_id == "poison" && matches!(pr.run.status, SopRunStatus::Failed)
            ),
            "the poison row's tombstone is parseable and Failed"
        );
        // And the shared-DB invariant: the tombstone is valid JSON.
        let json_valid: i64 = s
            .lock()
            .unwrap()
            .query_row(
                "SELECT json_valid(json) FROM sop_runs WHERE run_id='poison'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(json_valid, 1);
    }

    /// The cooldown read must skip a poison terminal row instead of erroring
    /// (pre-fix one bad row made EVERY SOP's cooldown check fall back to the
    /// local view).
    #[test]
    fn last_terminal_completed_at_skips_poison_rows() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        s.save_run(&run("done", SopRunStatus::Running, "1"))
            .unwrap();
        let mut terminal = run("done", SopRunStatus::Completed, "2");
        terminal.revision = 1;
        terminal.run.completed_at = Some("2026-10-01T00:00:00Z".to_string());
        s.finish_run("done", &terminal).unwrap();
        s.lock()
            .unwrap()
            .execute(
                "INSERT INTO sop_runs (run_id, revision, terminal, last_progress_at, json)
                 VALUES ('poison', 0, 1, NULL, ?1)",
                params![r#"{"version":1,"run":{"run_id":"poison"#],
            )
            .unwrap();

        let last = s.last_terminal_completed_at("deploy").unwrap();
        assert_eq!(
            last.as_deref(),
            Some("2026-10-01T00:00:00Z"),
            "the cooldown read skips the poison row and still answers"
        );
    }

    /// A torn CLAIM row must not abort the expired-claims scan — pre-fix the
    /// whole reaper errored out each tick, silently disabling lease reaping
    /// and wedging the start-gate on every stale claim.
    #[test]
    fn expired_claims_scan_releases_torn_claim_rows_instead_of_failing() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        // An expired claim with a parseable token: the normal reaper path.
        s.try_claim_run("r-live", "deploy", 2, 2).unwrap().unwrap();
        s.lock()
            .unwrap()
            .execute(
                "UPDATE sop_claims SET lease_expires='2000-01-01T00:00:00Z' WHERE run_id='r-live'",
                [],
            )
            .unwrap();
        // A torn claim row: expired, invalid JSON.
        s.lock()
            .unwrap()
            .execute(
                "INSERT INTO sop_claims (run_id, sop_name, lease_expires, json)
                 VALUES ('r-torn', 'deploy', '2000-01-01T00:00:00Z', ?1)",
                params![r#"{"run_id":"r-torn""#],
            )
            .unwrap();

        let reaped = s.expired_claims("2999-01-01T00:00:00Z").unwrap();
        assert_eq!(
            reaped.len(),
            1,
            "the parseable expired claim is reaped despite the torn row"
        );
        assert_eq!(reaped[0].run_id, "r-live");
        let torn_left: i64 = s
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sop_claims WHERE run_id='r-torn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(torn_left, 0, "the torn claim row is released outright");
    }

    #[test]
    fn cancelled_run_is_skipped_on_rehydration_and_releases_claim() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        let mut r1 = run("r1", SopRunStatus::Running, "1");
        r1.revision = 1;
        s.save_run(&r1).unwrap();
        s.try_claim_run("r1", "deploy", 1, 1).unwrap().unwrap();
        assert_eq!(s.load_active_runs().unwrap().len(), 1);

        // A terminal run is never re-claimable (`claim_single_winner_cap_and_terminal`),
        // so the release is proven through the per-sop cap instead: a second run
        // cannot claim while r1 holds the sole slot.
        s.save_run(&run("r2", SopRunStatus::Running, "1")).unwrap();
        assert!(
            s.try_claim_run("r2", "deploy", 1, 1).unwrap().is_none(),
            "the cap of 1 is exhausted while r1 holds its claim"
        );

        // Cancellation persists terminal state and its audit event atomically,
        // the same store call an operator cancel endpoint uses.
        let mut cancelled = run("r1", SopRunStatus::Cancelled, "2");
        cancelled.revision = 2;
        s.finish_run_with_event("r1", &cancelled, &ev("r1", "run_cancelled"))
            .unwrap();

        let active_ids: Vec<String> = s
            .load_active_runs()
            .unwrap()
            .into_iter()
            .map(|p| p.run.run_id)
            .collect();
        assert_eq!(
            active_ids,
            vec!["r2".to_string()],
            "a cancelled run must be skipped on restart rehydration; r2 stays active"
        );
        assert!(
            s.try_claim_run("r2", "deploy", 1, 1).unwrap().is_some(),
            "cancellation must release r1's claim, freeing the per-sop cap slot for r2"
        );
        assert!(
            s.list_events("r1")
                .unwrap()
                .iter()
                .any(|e| e.kind == "run_cancelled"),
            "the cancellation audit event must be durable"
        );
    }

    #[test]
    fn failed_run_reason_survives_file_backed_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("runs.db");

        {
            let store = SqliteRunStore::open(&db).unwrap();
            store
                .save_run(&run("failed", SopRunStatus::Running, "1"))
                .unwrap();

            let mut terminal = run("failed", SopRunStatus::Failed, "2");
            terminal.revision = 1;
            terminal.run.failure_reason = Some("disk quota exceeded".to_string());
            store.finish_run("failed", &terminal).unwrap();
        }

        let reopened = SqliteRunStore::open(&db).unwrap();
        let persisted = reopened
            .load_run("failed")
            .unwrap()
            .expect("failed terminal run remains readable after reopening SQLite");
        assert_eq!(persisted.run.status, SopRunStatus::Failed);
        assert_eq!(
            persisted.run.failure_reason.as_deref(),
            Some("disk quota exceeded")
        );
    }

    #[test]
    fn load_terminal_runs_filters_orders_and_limits() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        // One active run must never surface in the terminal load.
        s.save_run(&run("active", SopRunStatus::Running, "1"))
            .unwrap();
        // Three terminal runs finished at ascending progress marks.
        for (id, prog) in [("t-old", "1"), ("t-mid", "2"), ("t-new", "3")] {
            s.save_run(&run(id, SopRunStatus::Running, prog)).unwrap();
            let mut terminal = run(id, SopRunStatus::Completed, prog);
            terminal.revision = 1;
            s.finish_run(id, &terminal).unwrap();
        }

        let all = s.load_terminal_runs(0).unwrap();
        assert_eq!(all.len(), 3, "unbounded load returns every terminal run");
        assert!(
            all.iter().all(|r| r.run.run_id != "active"),
            "active run excluded from terminal load"
        );
        let ids: Vec<&str> = all.iter().map(|r| r.run.run_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["t-new", "t-mid", "t-old"],
            "terminal runs ordered newest-first by started_at"
        );

        let capped = s.load_terminal_runs(2).unwrap();
        assert_eq!(capped.len(), 2, "limit truncates the tail");
        assert_eq!(
            capped
                .iter()
                .map(|r| r.run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["t-new", "t-mid"],
            "limit keeps the newest runs"
        );
    }

    #[test]
    fn save_run_rejects_stale_revision() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        let mut newer = run("r1", SopRunStatus::Running, "1");
        newer.revision = 5;
        s.save_run(&newer).unwrap();
        let older = run("r1", SopRunStatus::Running, "1"); // revision 0
        assert!(matches!(
            s.save_run(&older),
            Err(StoreError::StaleRevision { .. })
        ));
    }

    #[test]
    fn claim_single_winner_cap_and_terminal() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        assert!(s.try_claim_run("r1", "deploy", 2, 2).unwrap().is_some());
        assert!(
            s.try_claim_run("r1", "deploy", 2, 2).unwrap().is_none(),
            "dup refused"
        );
        assert!(s.try_claim_run("r2", "deploy", 2, 2).unwrap().is_some());
        assert!(
            s.try_claim_run("r3", "deploy", 2, 2).unwrap().is_none(),
            "cap reached"
        );
        // terminal run not re-claimable
        s.finish_run("rT", &run("rT", SopRunStatus::Completed, "1"))
            .unwrap();
        assert!(
            s.try_claim_run("rT", "deploy", 0, 0).unwrap().is_none(),
            "terminal not claimable"
        );
    }

    #[test]
    fn save_run_rejects_divergent_same_revision() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        let mut base = run("r1", SopRunStatus::Running, "1");
        base.revision = 5;
        s.save_run(&base).unwrap();
        // A byte-identical same-revision write is an idempotent retry.
        s.save_run(&base).unwrap();
        // A divergent same-revision write is refused.
        let mut divergent = run("r1", SopRunStatus::Running, "2");
        divergent.revision = 5;
        assert!(matches!(
            s.save_run(&divergent),
            Err(StoreError::RevisionConflict { revision: 5, .. })
        ));
        // The stored run is unchanged (still revision 5, last_progress "1").
        let stored = s.load_run("r1").unwrap().unwrap();
        assert_eq!(stored.revision, 5);
        assert_eq!(stored.last_progress_at, "1");
    }

    #[test]
    fn finish_run_revision_guard_protects_state_and_claim() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        let mut base = run("r1", SopRunStatus::Running, "1");
        base.revision = 5;
        s.save_run(&base).unwrap();
        s.try_claim_run("r1", "deploy", 4, 4).unwrap().unwrap();

        // A stale terminal write (older revision) is refused.
        let mut stale = run("r1", SopRunStatus::Completed, "2");
        stale.revision = 4;
        assert!(matches!(
            s.finish_run("r1", &stale),
            Err(StoreError::StaleRevision { .. })
        ));
        // A divergent same-revision terminal write is refused too.
        let mut divergent = run("r1", SopRunStatus::Completed, "2");
        divergent.revision = 5;
        assert!(matches!(
            s.finish_run("r1", &divergent),
            Err(StoreError::RevisionConflict { .. })
        ));
        // State survived: still active at revision 5, claim still held.
        assert_eq!(s.load_active_runs().unwrap().len(), 1);
        assert_eq!(s.load_run("r1").unwrap().unwrap().revision, 5);
        assert!(
            s.try_claim_run("r1", "deploy", 4, 4).unwrap().is_none(),
            "claim slot still held"
        );

        // A proper terminal write at a newer revision succeeds and releases.
        let mut done = run("r1", SopRunStatus::Completed, "2");
        done.revision = 6;
        s.finish_run("r1", &done).unwrap();
        assert_eq!(s.load_active_runs().unwrap().len(), 0);
        assert_eq!(s.load_run("r1").unwrap().unwrap().revision, 6);
    }

    #[test]
    fn claim_caps_isolate_per_sop_yet_share_global() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        // per_sop_cap = 1, global_cap = 3.
        assert!(s.try_claim_run("a1", "a", 1, 3).unwrap().is_some());
        // A second "a" run is blocked by the per-SOP cap...
        assert!(s.try_claim_run("a2", "a", 1, 3).unwrap().is_none());
        // ...but a different SOP is not blocked by "a" being at its cap.
        assert!(s.try_claim_run("b1", "b", 1, 3).unwrap().is_some());
        assert!(s.try_claim_run("c1", "c", 1, 3).unwrap().is_some());
        // Global cap = 3 is reached across all SOPs; a fourth distinct SOP
        // is refused even though its own per-SOP slot is free.
        assert!(s.try_claim_run("d1", "d", 1, 3).unwrap().is_none());
    }

    #[test]
    fn events_append_only_monotonic_and_ordered() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        assert_eq!(s.append_event(&ev("r1", "run_started")).unwrap(), 1);
        assert_eq!(s.append_event(&ev("r1", "step_completed")).unwrap(), 2);
        assert_eq!(s.append_event(&ev("r2", "run_started")).unwrap(), 3);
        let r1 = s.list_events("r1").unwrap();
        assert_eq!(r1.len(), 2);
        assert_eq!(r1[0].seq, 1);
        assert_eq!(r1[1].kind, "step_completed");
        assert_eq!(s.list_events("r2").unwrap().len(), 1);
    }

    #[test]
    fn prune_evicts_oldest_terminal_first() {
        let s = SqliteRunStore::open_in_memory().unwrap();
        for (id, ts) in [("a", "1"), ("b", "2"), ("c", "3")] {
            s.finish_run(id, &run(id, SopRunStatus::Completed, ts))
                .unwrap();
        }
        let dropped = s
            .prune(&RetentionPolicy {
                max_terminal: 1,
                keep_secs: None,
            })
            .unwrap();
        assert_eq!(dropped, 2);
        assert!(s.load_run("c").unwrap().is_some(), "newest kept");
        assert!(s.load_run("a").unwrap().is_none(), "oldest dropped");
        assert!(s.load_run("b").unwrap().is_none());
    }

    #[test]
    fn runs_survive_reopen() {
        // Durability: a run written by one instance is visible to a fresh
        // instance opening the same file (the restart-resume guarantee).
        let path = std::env::temp_dir().join(format!("zc-sop-durable-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let a = SqliteRunStore::open(&path).unwrap();
            a.save_run(&run("r1", SopRunStatus::WaitingApproval, "1"))
                .unwrap();
        } // drop `a` - simulates daemon shutdown
        let b = SqliteRunStore::open(&path).unwrap();
        let active = b.load_active_runs().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run.run_id, "r1");
        assert_eq!(active[0].run.status, SopRunStatus::WaitingApproval);
        let _ = std::fs::remove_file(&path);
    }
}
