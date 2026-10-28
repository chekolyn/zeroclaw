//! PostgreSQL-backed memory implementation.

use super::traits::{Memory, MemoryCategory, MemoryEntry, normalize_recent_recall_query};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use postgres::{Client, NoTls, Row};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;
use zeroclaw_api::session_keys::sanitize_session_key;

/// Maximum allowed connect timeout (seconds) to avoid unreasonable waits.
const POSTGRES_CONNECT_TIMEOUT_CAP_SECS: u64 = 300;

/// Default per-operation timeout (seconds) bounding each memory query.
const DEFAULT_OP_TIMEOUT_SECS: u64 = 30;

/// Maximum allowed per-operation timeout (seconds).
const POSTGRES_OP_TIMEOUT_CAP_SECS: u64 = 300;

struct DropOnThread<T: Send + 'static>(Option<T>);

impl<T: Send + 'static> DropOnThread<T> {
    fn new(value: T) -> Self {
        Self(Some(value))
    }
    fn get(&self) -> &T {
        self.0.as_ref().expect("DropOnThread value already taken")
    }
}

impl<T: Send + 'static> Drop for DropOnThread<T> {
    fn drop(&mut self) {
        let Some(value) = self.0.take() else { return };
        // Wrap in ManuallyDrop so the value is NOT dropped on the current
        // thread if spawn fails — ManuallyDrop's own Drop is a no-op.
        let slot = std::mem::ManuallyDrop::new(value);
        if std::thread::Builder::new()
            .name("postgres-client-drop".to_string())
            .spawn(move || drop(std::mem::ManuallyDrop::into_inner(slot)))
            .is_err()
        {
            // The OS refused to spawn a thread. Intentionally leak the value
            // rather than drop it here: postgres::Client::drop calls
            // Runtime::block_on, which panics on a Tokio runtime thread.
            // A controlled leak is preferable to an unrecoverable panic.
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "postgres-client-drop thread spawn failed; leaking client to avoid nested-runtime panic"
            );
            // `slot` is ManuallyDrop — T is intentionally not dropped.
        }
    }
}

/// The one operation the boot recall-index attempts need, so the
/// warn-and-continue path is unit-testable with a mock executor instead of a
/// live PostgreSQL instance.
trait DdlExecutor {
    fn exec_batch(&mut self, sql: &str) -> Result<()>;
}

impl DdlExecutor for Client {
    fn exec_batch(&mut self, sql: &str) -> Result<()> {
        Client::batch_execute(self, sql).map_err(Into::into)
    }
}

pub struct PostgresMemory {
    alias: String,
    client: DropOnThread<Arc<Mutex<Client>>>,
    qualified_table: String,
    qualified_agents: String,
    /// Category → namespace map applied at write time (`memory.category_namespaces`).
    /// When a memory is stored with a category present in this map, the mapped
    /// namespace overrides the default. Enables granular namespaces excluded
    /// from recall via `exclude_namespaces` prefix matching.
    category_namespaces: HashMap<String, String>,
    op_timeout: Duration,
    /// Soft wall-clock budget for the RANKED recall attempts. When set (and
    /// below `op_timeout`), each ranked recall runs with a per-attempt
    /// server-side `statement_timeout` so an over-budget attempt is cancelled
    /// by the server, freeing the shared connection for the recall budget's
    /// recent-entries fallback instead of holding it until `op_timeout`.
    /// The empty-query recall path is exempt: it is the indexed recent
    /// fallback itself.
    recall_soft_timeout: Option<Duration>,
}

impl PostgresMemory {
    pub fn new(
        alias: &str,
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
        op_timeout_secs: Option<u64>,
        pgvector_enabled: Option<bool>,
        pgvector_dimensions: Option<usize>,
        recall_soft_timeout_ms: Option<u64>,
    ) -> Result<Self> {
        validate_identifier(schema, "storage schema")?;
        validate_identifier(table, "storage table")?;

        let schema_ident = quote_identifier(schema);
        let table_ident = quote_identifier(table);
        let qualified_table = format!("{schema_ident}.{table_ident}");
        let qualified_agents = format!("{schema_ident}.agents");

        let pgvector = if pgvector_enabled.unwrap_or(false) {
            Some(pgvector_dimensions.unwrap_or(1536))
        } else {
            None
        };

        let op_timeout = Duration::from_secs(
            op_timeout_secs
                .unwrap_or(DEFAULT_OP_TIMEOUT_SECS)
                .min(POSTGRES_OP_TIMEOUT_CAP_SECS),
        );
        // A soft budget at or above `op_timeout` is inert: the session-level
        // `statement_timeout` (op_timeout) is already the tighter bound.
        let recall_soft_timeout = recall_soft_timeout_ms
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .filter(|soft| *soft < op_timeout);

        let (client, pgvector_ok) = Self::initialize_client(
            db_url.to_string(),
            connect_timeout_secs,
            op_timeout,
            schema_ident.clone(),
            qualified_table.clone(),
            pgvector,
        )?;

        if !pgvector_ok {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "pgvector extension not available; falling back to keyword-only recall"
            );
        }

        Ok(Self {
            alias: alias.to_string(),
            client: DropOnThread::new(Arc::new(Mutex::new(client))),
            qualified_table,
            qualified_agents,
            category_namespaces: HashMap::new(),
            op_timeout,
            recall_soft_timeout,
        })
    }

    /// Apply the `[memory] category_namespaces` config so writes auto-tag the
    /// namespace from the category. Builder-style: consumes and returns `self`.
    /// Called once at backend construction from `build_postgres_memory`.
    pub fn with_category_namespaces(
        mut self,
        category_namespaces: HashMap<String, String>,
    ) -> Self {
        self.category_namespaces = category_namespaces;
        self
    }

    fn initialize_client(
        db_url: String,
        connect_timeout_secs: Option<u64>,
        op_timeout: Duration,
        schema_ident: String,
        qualified_table: String,
        pgvector: Option<usize>,
    ) -> Result<(Client, bool)> {
        let init_handle = std::thread::Builder::new()
            .name("postgres-memory-init".to_string())
            .spawn(move || -> Result<(Client, bool)> {
                let mut config: postgres::Config = db_url
                    .parse()
                    .context("invalid PostgreSQL connection URL")?;

                if let Some(timeout_secs) = connect_timeout_secs {
                    let bounded = timeout_secs.min(POSTGRES_CONNECT_TIMEOUT_CAP_SECS);
                    config.connect_timeout(Duration::from_secs(bounded));
                }

                config.keepalives(true);
                config.keepalives_idle(Duration::from_secs(15));
                config.keepalives_retries(3);

                let mut client = config
                    .connect(NoTls)
                    .context("failed to connect to PostgreSQL memory backend")?;

                let migration_started = std::time::Instant::now();

                Self::init_schema(&mut client, &schema_ident, &qualified_table)?;
                zeroclaw_config::schema::v2::migrate_postgres_memory_to_v3(
                    &mut client,
                    &schema_ident,
                    &qualified_table,
                )?;
                // The synchronous postgres client uses block_on internally,
                // so pgvector setup must also stay on this plain OS thread —
                // calling it from a Tokio runtime thread panics.
                let pgvector_ok = match pgvector {
                    Some(dimensions) => {
                        Self::try_enable_pgvector(&mut client, &qualified_table, dimensions).is_ok()
                    }
                    None => true,
                };

                // The boot DDL duration at INFO so future boots are measurable
                // (the 2026-10-28 canary incident surfaced as an 88 s index
                // build that was invisible in the boot logs).
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                        .with_outcome(::zeroclaw_log::EventOutcome::Success)
                        .with_attrs(::serde_json::json!({
                            "duration_secs": migration_started.elapsed().as_secs_f64()
                        })),
                    "PostgreSQL boot schema migration completed"
                );

                // Restore the latency-bounded session for normal memory
                // operations. The recall soft-budget wrapper
                // (`with_statement_timeout`) still re-bounds each ranked
                // recall attempt below this session default.
                client
                    .batch_execute(&Self::bounded_session_sql(op_timeout))
                    .context("failed to apply PostgreSQL session timeouts")?;

                Ok((client, pgvector_ok))
            })
            .context("failed to spawn PostgreSQL initializer thread")?;

        init_handle.join().map_err(|_| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "PostgreSQL initializer thread panicked"
            );
            anyhow::Error::msg("PostgreSQL initializer thread panicked")
        })?
    }

    fn init_schema(client: &mut Client, schema_ident: &str, qualified_table: &str) -> Result<()> {
        // Boot DDL is a one-time migration, not a latency-bounded query:
        // exempt it from statement timeouts BEFORE any DDL runs. Without
        // this, the session `op_timeout` — or a server/role default —
        // cancels a long index build and, because a multi-statement
        // `batch_execute` is a single implicit transaction, the cancel rolls
        // the whole schema batch back so every boot retry re-attempts the
        // same doomed build: a permanent crash-loop (canary 2026-10-28, an
        // 88 s GIN build vs a 30 s `op_timeout`). The caller restores the
        // bounded session with `bounded_session_sql` after the full boot
        // migration sequence.
        client
            .batch_execute(Self::BOOT_DDL_SESSION_SQL)
            .context("failed to exempt the boot schema DDL from statement timeouts")?;

        // Load-bearing core: the schema and the table itself. A failure here
        // is a genuine boot failure — every memory operation depends on it.
        client.batch_execute(&Self::ensure_core_schema_sql(schema_ident, qualified_table))?;

        // Optional recall indexes: a failed (or cancelled) build is a LOUD
        // warning and CONTINUES — a boot must never die to an optional index.
        Self::attempt_recall_indexes(client, qualified_table);

        Self::migrate_session_ids_to_sanitized(client, qualified_table)?;

        Ok(())
    }

    /// The session guard for the boot-DDL migration window: unbounded
    /// statement and idle-in-transaction timeouts. Issued at the start of
    /// [`Self::init_schema`]; restored by `initialize_client` with
    /// [`Self::bounded_session_sql`] once the boot migration sequence is
    /// done. Recall queries are never affected: they only run after the
    /// restore (and the per-attempt recall wrapper re-bounds them further).
    const BOOT_DDL_SESSION_SQL: &str =
        "SET statement_timeout = 0; SET idle_in_transaction_session_timeout = 0;";

    /// The bounded session for normal (post-migration) memory operations.
    /// The inverse of [`Self::BOOT_DDL_SESSION_SQL`].
    fn bounded_session_sql(op_timeout: Duration) -> String {
        let timeout_ms = op_timeout.as_millis();
        format!(
            "SET statement_timeout = {timeout_ms}; \
             SET idle_in_transaction_session_timeout = {timeout_ms};"
        )
    }

    /// The load-bearing boot DDL: the schema and the table. A failure here is
    /// a genuine boot failure.
    fn ensure_core_schema_sql(schema_ident: &str, qualified_table: &str) -> String {
        format!(
            "
            CREATE SCHEMA IF NOT EXISTS {schema_ident};

            CREATE TABLE IF NOT EXISTS {qualified_table} (
                id TEXT PRIMARY KEY,
                key TEXT NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                session_id TEXT
            );
            -- Composite (agent_id, key) uniqueness lands in the V3 migration
            -- once the `agent_id` column is added and backfilled.
            "
        )
    }

    /// The optional recall indexes as ONE standalone `IF NOT EXISTS` statement
    /// each (each its own implicit transaction, each individually retryable on
    /// a later boot). Attempted individually by
    /// [`Self::attempt_recall_indexes`].
    fn recall_index_statements(qualified_table: &str) -> Vec<(&'static str, String)> {
        vec![
            (
                "idx_memories_category",
                format!(
                    "CREATE INDEX IF NOT EXISTS idx_memories_category ON {qualified_table}(category);"
                ),
            ),
            (
                "idx_memories_session_id",
                format!(
                    "CREATE INDEX IF NOT EXISTS idx_memories_session_id ON {qualified_table}(session_id);"
                ),
            ),
            (
                "idx_memories_updated_at",
                format!(
                    "CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON {qualified_table}(updated_at DESC);"
                ),
            ),
            (
                "idx_memories_content_fts",
                format!(
                    "CREATE INDEX IF NOT EXISTS idx_memories_content_fts ON {qualified_table} USING gin(to_tsvector('simple', content));"
                ),
            ),
            (
                "idx_memories_key_fts",
                format!(
                    "CREATE INDEX IF NOT EXISTS idx_memories_key_fts ON {qualified_table} USING gin(to_tsvector('simple', key));"
                ),
            ),
            // The recall WHERE clause matches on the CONCATENATION
            // `to_tsvector('simple', key || ' ' || content)`, which matches
            // neither single-column FTS index above: every recall seq-scanned
            // the whole table computing tsvectors per row (measured
            // 2026-10-27: 6.3–6.7s per recall on a 50K-row store). This index
            // makes the query a first-class filter.
            (
                "idx_memories_key_content_fts",
                format!(
                    "CREATE INDEX IF NOT EXISTS idx_memories_key_content_fts ON {qualified_table} USING gin(to_tsvector('simple', key || ' ' || content));"
                ),
            ),
            (
                "idx_memories_key_btree",
                format!(
                    "CREATE INDEX IF NOT EXISTS idx_memories_key_btree ON {qualified_table}(key text_pattern_ops);"
                ),
            ),
        ]
    }

    /// Attempt each optional recall index independently; a failure logs a
    /// LOUD warning and CONTINUES. Recalls then degrade to the un-indexed
    /// seq-scan shape (bounded by the session `op_timeout` and the recall
    /// soft budget's recent-entries fallback) and the next boot retries the
    /// build — a boot must never die to an optional index.
    fn attempt_recall_indexes(executor: &mut impl DdlExecutor, qualified_table: &str) {
        for (index_name, statement) in Self::recall_index_statements(qualified_table) {
            if let Err(err) = executor.exec_batch(&statement) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "index": index_name,
                            "error": err.to_string(),
                        })),
                    "Recall index failed to build at boot; continuing WITHOUT it — \
                     recall degrades to the un-indexed/recent shapes and the next \
                     boot retries the build"
                );
            }
        }
    }

    fn migrate_session_ids_to_sanitized(client: &mut Client, qualified_table: &str) -> Result<()> {
        let select = format!(
            "SELECT DISTINCT session_id FROM {qualified_table} WHERE session_id IS NOT NULL"
        );
        let rows = client.query(&select, &[])?;
        let distinct: Vec<String> = rows.iter().map(|r| r.get(0)).collect();

        let rewrites = Self::compute_session_id_rewrites(&distinct);
        if rewrites.is_empty() {
            return Ok(());
        }

        let update = format!("UPDATE {qualified_table} SET session_id = $1 WHERE session_id = $2");
        let stmt = client.prepare(&update)?;
        for (old, new) in &rewrites {
            client.execute(&stmt, &[new, old])?;
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"rewritten": rewrites.len()})),
            "Normalized session_id values in memories table to sanitized form"
        );

        Ok(())
    }

    /// Pure plan of `(old, new)` `session_id` rewrites for the rows whose
    /// stored value differs from its sanitized form. Extracted from
    /// `migrate_session_ids_to_sanitized` so the rewrite logic is
    /// unit-testable without a live PostgreSQL instance.
    fn compute_session_id_rewrites(distinct: &[String]) -> Vec<(String, String)> {
        distinct
            .iter()
            .filter_map(|old| {
                let new = sanitize_session_key(old);
                if new == *old {
                    None
                } else {
                    Some((old.clone(), new))
                }
            })
            .collect()
    }

    fn category_to_str(category: &MemoryCategory) -> String {
        match category {
            MemoryCategory::Core => "core".to_string(),
            MemoryCategory::Daily => "daily".to_string(),
            MemoryCategory::Conversation => "conversation".to_string(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    fn parse_category(value: &str) -> MemoryCategory {
        match value {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    fn try_enable_pgvector(
        client: &mut Client,
        qualified_table: &str,
        dimensions: usize,
    ) -> Result<()> {
        client.batch_execute("CREATE EXTENSION IF NOT EXISTS vector")?;
        client.batch_execute(&format!(
            r#"
            DO $$ BEGIN
                ALTER TABLE {qualified_table} ADD COLUMN IF NOT EXISTS namespace TEXT DEFAULT 'default';
                ALTER TABLE {qualified_table} ADD COLUMN IF NOT EXISTS importance REAL;
                ALTER TABLE {qualified_table} ADD COLUMN IF NOT EXISTS embedding vector({dimensions});
            EXCEPTION WHEN OTHERS THEN
                RAISE NOTICE 'pgvector columns could not be added: %', SQLERRM;
            END $$;
            CREATE INDEX IF NOT EXISTS idx_memories_namespace ON {qualified_table}(namespace);
            "#
        ))?;
        Ok(())
    }

    fn row_to_entry(row: &Row) -> Result<MemoryEntry> {
        // Named access is used throughout so row_to_entry is immune to SELECT
        // column reordering and does not depend on matching the DDL ordering.
        let timestamp: DateTime<Utc> = row.get("created_at");

        Ok(MemoryEntry {
            principal_id: None,
            id: row.get("id"),
            key: row.get("key"),
            content: row.get("content"),
            category: Self::parse_category(&row.get::<_, String>("category")),
            timestamp: timestamp.to_rfc3339(),
            session_id: row.get("session_id"),
            score: row.try_get("score").ok(),
            namespace: row
                .try_get::<_, String>("namespace")
                .unwrap_or_else(|_| "default".into()),
            importance: row.try_get("importance").ok(),
            superseded_by: None,
            kind: None,
            pinned: false,
            tenant_id: None,
            agent_alias: row.try_get("agent_alias").ok(),
            agent_id: row.try_get("agent_id").ok(),
        })
    }
}

async fn run_on_os_thread<F, T>(op_timeout: Duration, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();

    std::thread::Builder::new()
        .name("postgres-memory-op".to_string())
        .spawn(move || {
            let result = f();
            let _ = tx.send(result);
        })
        .context("failed to spawn PostgreSQL operation thread")?;

    let awaited = match tokio::time::timeout(op_timeout, rx).await {
        Ok(inner) => inner,
        Err(_elapsed) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "PostgreSQL memory operation exceeded its timeout; the operation thread was abandoned"
            );
            anyhow::bail!(
                "PostgreSQL memory operation exceeded {}s timeout",
                op_timeout.as_secs()
            );
        }
    };

    awaited.map_err(|_| {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure),
            "PostgreSQL operation thread terminated unexpectedly"
        );
        anyhow::Error::msg("PostgreSQL operation thread terminated unexpectedly")
    })?
}

pub(super) fn validate_identifier(value: &str, field_name: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field_name} must not be empty");
    }

    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("{field_name} must not be empty");
    };

    if !(first.is_ascii_alphabetic() || first == '_') {
        anyhow::bail!("{field_name} must start with an ASCII letter or underscore; got '{value}'");
    }

    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        anyhow::bail!(
            "{field_name} can only contain ASCII letters, numbers, and underscores; got '{value}'"
        );
    }

    Ok(())
}

pub(super) fn quote_identifier(value: &str) -> String {
    format!("\"{value}\"")
}

fn recall_time_filter(since: bool, until: bool, first_placeholder: usize) -> String {
    match (since, until) {
        (true, true) => format!(
            " AND m.created_at >= ${first_placeholder}::TIMESTAMPTZ AND m.created_at <= ${}::TIMESTAMPTZ",
            first_placeholder + 1
        ),
        (true, false) => {
            format!(" AND m.created_at >= ${first_placeholder}::TIMESTAMPTZ")
        }
        (false, true) => {
            format!(" AND m.created_at <= ${first_placeholder}::TIMESTAMPTZ")
        }
        (false, false) => String::new(),
    }
}

/// The recent-entries recall: indexed recency (`idx_memories_updated_at`),
/// NO full-text scoring. The empty-query contract (bare `*` / omitted query
/// returns recent entries) must not pay the per-row tsvector cost of the
/// ranked query — on the measured 50K-row store the ranked shape seq-scanned
/// every row even for a bare recall because the constant-zero score
/// expressions still computed `to_tsvector` per row. This is also the
/// degraded shape the recall budget's fallback lands on.
///
/// Placeholders: `$1` session_id, `$2..` the enabled time bounds, LIMIT last.
fn recent_recall_sql(qualified_table: &str, qualified_agents: &str, since: bool, until: bool) -> String {
    let time_filter = recall_time_filter(since, until, 2);
    let limit_placeholder = 2 + usize::from(since) + usize::from(until);
    format!(
        "
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
        FROM {qualified_table} m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
        WHERE ($1::TEXT IS NULL OR m.session_id = $1)
          {time_filter}
        ORDER BY m.updated_at DESC
        LIMIT ${limit_placeholder}
        "
    )
}

/// The agent-scoped variant of [`recent_recall_sql`]. Placeholders: `$1` the
/// agent allowlist, `$2..` the enabled time bounds, LIMIT last.
fn recent_recall_sql_for_agents(
    qualified_table: &str,
    qualified_agents: &str,
    since: bool,
    until: bool,
) -> String {
    let time_filter = recall_time_filter(since, until, 2);
    let limit_placeholder = 2 + usize::from(since) + usize::from(until);
    format!(
        "
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
        FROM {qualified_table} m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
        WHERE m.agent_id = ANY($1)
          {time_filter}
        ORDER BY m.updated_at DESC
        LIMIT ${limit_placeholder}
        "
    )
}

/// Bound ONE ranked recall attempt: sets `statement_timeout` to the soft
/// recall budget, runs `run`, then restores the session default
/// (`op_timeout`). The whole SET → query → restore sequence runs inside the
/// shared client's mutex critical section, so it cannot interleave with
/// another operation. Best-effort restore: a failed restore only leaves the
/// tighter bound in place for subsequent statements until the next guarded
/// attempt resets it. `soft == None` runs the query with no per-attempt
/// re-bound (the session-level `op_timeout` already applies).
fn with_statement_timeout<T>(
    client: &mut Client,
    soft: Option<Duration>,
    op_timeout: Duration,
    run: impl FnOnce(&mut Client) -> Result<T>,
) -> Result<T> {
    let Some(soft) = soft else {
        return run(client);
    };
    client
        .batch_execute(&format!("SET statement_timeout = {}", soft.as_millis()))
        .context("failed to bound the recall attempt at the soft budget")?;
    let result = run(client);
    let _ = client.batch_execute(&format!(
        "SET statement_timeout = {}",
        op_timeout.as_millis()
    ));
    result
}

/// INSERT/UPSERT used by [`Memory::store_with_agent`].
///
/// The `importance` placeholder must carry an explicit `::FLOAT8` annotation
/// (both the parameter and the fallback literal). Without it, PostgreSQL
/// infers `$10` as `NUMERIC` from the untyped `0.5` literal, and the Rust
/// `f64` bind — which only implements `ToSql` for `FLOAT8` — fails with
/// "error serializing parameter 9" (0-based bind index of the 10th
/// parameter). The column itself is `REAL`; the `FLOAT8` result is
/// assignment-cast on INSERT (`float8 → real` context 'a').
fn store_insert_sql(qualified_table: &str, qualified_agents: &str) -> String {
    format!(
        "
        INSERT INTO {qualified_table}
            (id, key, content, category, created_at, updated_at, session_id, agent_id, namespace, importance)
        VALUES
            ($1, $2, $3, $4, $5, $6, $7,
             COALESCE($8, (SELECT id FROM {qualified_agents} WHERE alias = 'default' LIMIT 1)),
             COALESCE($9, 'default'), COALESCE($10::FLOAT8, 0.5::FLOAT8))
        ON CONFLICT (agent_id, key) DO UPDATE SET
            content = EXCLUDED.content,
            category = EXCLUDED.category,
            updated_at = EXCLUDED.updated_at,
            session_id = EXCLUDED.session_id,
            namespace = EXCLUDED.namespace,
            importance = EXCLUDED.importance
        "
    )
}

#[async_trait]
impl Memory for PostgresMemory {
    fn name(&self) -> &str {
        "postgres"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> Result<()> {
        // Apply `[memory] category_namespaces`: if the category maps to a
        // namespace, store under that namespace instead of the default.
        let category_key = Self::category_to_str(&category);
        let ns = self
            .category_namespaces
            .get(&category_key)
            .map(String::as_str);
        self.store_with_agent(key, content, category, session_id, ns, None, None)
            .await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let query = normalize_recent_recall_query(query).trim().to_string();
        let sid = session_id.map(str::to_string);
        let since_owned = since.map(str::to_string);
        let until_owned = until.map(str::to_string);

        // Empty-query fast path: the bare `*` / omitted-query contract is an
        // indexed recency read, not a full-text scan.
        if query.is_empty() {
            return run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
                let mut client = client.lock();
                let since_ref = since_owned.as_deref();
                let until_ref = until_owned.as_deref();
                let stmt = recent_recall_sql(
                    &qualified_table,
                    &qualified_agents,
                    since_ref.is_some(),
                    until_ref.is_some(),
                );

                #[allow(clippy::cast_possible_wrap)]
                let limit_i64 = limit as i64;

                let rows = match (since_ref, until_ref) {
                    (Some(s), Some(u)) => client.query(&stmt, &[&sid, &s, &u, &limit_i64])?,
                    (Some(s), None) => client.query(&stmt, &[&sid, &s, &limit_i64])?,
                    (None, Some(u)) => client.query(&stmt, &[&sid, &u, &limit_i64])?,
                    (None, None) => client.query(&stmt, &[&sid, &limit_i64])?,
                };
                rows.iter()
                    .map(Self::row_to_entry)
                    .collect::<Result<Vec<MemoryEntry>>>()
            })
            .await;
        }

        let client = self.client.get().clone();
        let attempt_timeout = self.recall_soft_timeout;
        let op_timeout = self.op_timeout;

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            let mut client = client.lock();
            let since_ref = since_owned.as_deref();
            let until_ref = until_owned.as_deref();

            let time_filter = recall_time_filter(since_ref.is_some(), until_ref.is_some(), 4);

            let stmt = format!(
                "
                SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8,
                       (
                         CASE WHEN to_tsvector('simple', m.key) @@ plainto_tsquery('simple', $1)
                           THEN ts_rank_cd(to_tsvector('simple', m.key), plainto_tsquery('simple', $1)) * 2.0
                           ELSE 0.0 END +
                         CASE WHEN to_tsvector('simple', m.content) @@ plainto_tsquery('simple', $1)
                           THEN ts_rank_cd(to_tsvector('simple', m.content), plainto_tsquery('simple', $1))
                           ELSE 0.0 END
                       ) AS score
                FROM {qualified_table} m
                LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
                WHERE ($2::TEXT IS NULL OR m.session_id = $2)
                  AND ($1 = '' OR to_tsvector('simple', m.key || ' ' || m.content) @@ plainto_tsquery('simple', $1))
                  {time_filter}
                ORDER BY score DESC, m.updated_at DESC
                LIMIT $3
                ",
            );

            #[allow(clippy::cast_possible_wrap)]
            let limit_i64 = limit as i64;

            // Bound THIS attempt server-side at the soft budget so a caller
            // that gives up leaves no orphan holding the shared connection.
            let entries =
                with_statement_timeout(&mut client, attempt_timeout, op_timeout, |client| {
                    let rows = match (since_ref, until_ref) {
                        (Some(s), Some(u)) => client.query(&stmt, &[&query, &sid, &limit_i64, &s, &u])?,
                        (Some(s), None) => client.query(&stmt, &[&query, &sid, &limit_i64, &s])?,
                        (None, Some(u)) => client.query(&stmt, &[&query, &sid, &limit_i64, &u])?,
                        (None, None) => client.query(&stmt, &[&query, &sid, &limit_i64])?,
                    };
                    rows.iter().map(Self::row_to_entry).collect::<Result<Vec<MemoryEntry>>>()
                })?;
            Ok(entries)
        })
        .await
    }

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let key = key.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<Option<MemoryEntry>> {
            let mut client = client.lock();
            let stmt = format!(
                "
                SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
                FROM {qualified_table} m
                LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
                WHERE m.key = $1
                LIMIT 1
                "
            );

            let row = client.query_opt(&stmt, &[&key])?;
            row.as_ref().map(Self::row_to_entry).transpose()
        })
        .await
    }

    async fn get_for_agent(&self, key: &str, agent_id: &str) -> Result<Option<MemoryEntry>> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let key = key.to_string();
        let agent_id = agent_id.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<Option<MemoryEntry>> {
            let mut client = client.lock();
            let stmt = format!(
                "
                SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
                FROM {qualified_table} m
                LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
                WHERE m.key = $1 AND m.agent_id = $2
                LIMIT 1
                "
            );

            let row = client.query_opt(&stmt, &[&key, &agent_id])?;
            row.as_ref().map(Self::row_to_entry).transpose()
        })
        .await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let category = category.map(Self::category_to_str);
        let sid = session_id.map(str::to_string);

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            let mut client = client.lock();
            let stmt = format!(
                "
                SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
                FROM {qualified_table} m
                LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
                WHERE ($1::TEXT IS NULL OR m.category = $1)
                  AND ($2::TEXT IS NULL OR m.session_id = $2)
                ORDER BY m.updated_at DESC
                "
            );

            let category_ref = category.as_deref();
            let session_ref = sid.as_deref();
            let rows = client.query(&stmt, &[&category_ref, &session_ref])?;
            rows.iter()
                .map(Self::row_to_entry)
                .collect::<Result<Vec<MemoryEntry>>>()
        })
        .await
    }

    async fn forget(&self, key: &str) -> Result<bool> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<bool> {
            let mut client = client.lock();
            let stmt = format!("DELETE FROM {qualified_table} WHERE key = $1");
            let deleted = client.execute(&stmt, &[&key])?;
            Ok(deleted > 0)
        })
        .await
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> Result<bool> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();
        let agent_id = agent_id.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<bool> {
            let mut client = client.lock();
            let stmt = format!("DELETE FROM {qualified_table} WHERE key = $1 AND agent_id = $2");
            let deleted = client.execute(&stmt, &[&key, &agent_id])?;
            Ok(deleted > 0)
        })
        .await
    }

    async fn purge_session_for_agent(&self, session_id: &str, agent_id: &str) -> Result<usize> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let session_id = session_id.to_string();
        let agent_id = agent_id.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            let mut client = client.lock();
            let stmt =
                format!("DELETE FROM {qualified_table} WHERE session_id = $1 AND agent_id = $2");
            let deleted = client.execute(&stmt, &[&session_id, &agent_id])?;
            usize::try_from(deleted).context("PostgreSQL returned an oversized delete count")
        })
        .await
    }

    async fn purge_agent(&self, agent_alias: &str) -> Result<usize> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = agent_alias.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            let mut client = client.lock();
            let stmt = format!(
                "DELETE FROM {qualified_table} WHERE agent_id = (SELECT id FROM {qualified_agents} WHERE alias = $1)"
            );
            let deleted = client.execute(&stmt, &[&alias])?;
            usize::try_from(deleted).context("PostgreSQL returned an oversized delete count")
        })
        .await
    }

    async fn export_agent(&self, agent_alias: &str) -> Result<Vec<MemoryEntry>> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = agent_alias.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            let mut client = client.lock();
            let stmt = format!(
                "
                SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
                FROM {qualified_table} m
                LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
                WHERE m.agent_id = (SELECT id FROM {qualified_agents} WHERE alias = $1)
                ORDER BY m.created_at ASC
                "
            );
            let rows = client.query(&stmt, &[&alias])?;
            rows.iter()
                .map(Self::row_to_entry)
                .collect::<Result<Vec<MemoryEntry>>>()
        })
        .await
    }

    async fn rename_agent(&self, from: &str, to: &str) -> Result<usize> {
        let client = self.client.get().clone();
        let qualified_agents = self.qualified_agents.clone();
        let qualified_table = self.qualified_table.clone();
        let from = from.to_string();
        let to = to.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            let mut client = client.lock();
            let mut tx = client.transaction()?;
            let to_rows: i64 = tx
                .query_one(
                    &format!(
                        "SELECT COUNT(*) FROM {qualified_table} WHERE agent_id = (SELECT id FROM {qualified_agents} WHERE alias = $1)"
                    ),
                    &[&to],
                )?
                .get(0);
            if to_rows > 0 {
                anyhow::bail!(
                    "cannot rename agent memory to `{to}`: an existing memory store under that alias has {to_rows} row(s); refusing to merge"
                );
            }
            tx.execute(
                &format!("DELETE FROM {qualified_agents} WHERE alias = $1"),
                &[&to],
            )?;
            let updated = tx.execute(
                &format!("UPDATE {qualified_agents} SET alias = $2 WHERE alias = $1"),
                &[&from, &to],
            )?;
            tx.commit()?;
            usize::try_from(updated).context("PostgreSQL returned an oversized update count")
        })
        .await
    }

    async fn count_agent(&self, agent_alias: &str) -> Result<usize> {
        let client = self.client.get().clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = agent_alias.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            let mut client = client.lock();
            // Mirror `rename_agent`: it moves the `agents` row (alias -> id), so
            // residue is the presence of that alias row (0 or 1), NOT the memory-
            // row count (which would miss an agents-row-only lag).
            let stmt = format!("SELECT COUNT(*) FROM {qualified_agents} WHERE alias = $1");
            let row = client.query_one(&stmt, &[&alias])?;
            let count: i64 = row.get(0);
            usize::try_from(count).context("PostgreSQL returned an oversized agent count")
        })
        .await
    }

    async fn count(&self) -> Result<usize> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            let mut client = client.lock();
            let stmt = format!("SELECT COUNT(*) FROM {qualified_table}");
            let count: i64 = client.query_one(&stmt, &[])?.get(0);
            let count =
                usize::try_from(count).context("PostgreSQL returned a negative memory count")?;
            Ok(count)
        })
        .await
    }

    async fn health_check(&self) -> bool {
        let client = self.client.get().clone();
        run_on_os_thread(self.op_timeout, move || {
            Ok(client.lock().simple_query("SELECT 1").is_ok())
        })
        .await
        .unwrap_or(false)
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
        agent_id: Option<&str>,
    ) -> Result<()> {
        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let key = key.to_string();
        let content = content.to_string();
        let category = Self::category_to_str(&category);
        let sid = session_id.map(str::to_string);
        let aid = agent_id.map(str::to_string);
        let ns = namespace
            .map(str::to_string)
            .unwrap_or_else(|| "default".to_string());
        let imp = importance.unwrap_or(0.5);

        run_on_os_thread(self.op_timeout, move || -> Result<()> {
            let now = Utc::now();
            let mut client = client.lock();
            let stmt = store_insert_sql(&qualified_table, &qualified_agents);

            let id = Uuid::new_v4().to_string();
            client.execute(
                &stmt,
                &[
                    &id, &key, &content, &category, &now, &now, &sid, &aid, &ns, &imp,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn recall_for_agents(
        &self,
        allowed_agent_ids: &[&str],
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        // Empty allowlist means "no agent filter": fall back to plain
        // recall. The wrapper always includes the bound agent's UUID,
        // so a non-empty allowlist is the live-runtime case.
        if allowed_agent_ids.is_empty() {
            return self.recall(query, limit, session_id, since, until).await;
        }

        let client = self.client.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let q = normalize_recent_recall_query(query).trim().to_string();
        let sid = session_id.map(str::to_string);
        let since_owned = since.map(str::to_string);
        let until_owned = until.map(str::to_string);
        let allowed: Vec<String> = allowed_agent_ids.iter().map(|s| (*s).to_string()).collect();

        // Empty-query fast path: indexed recency within the agent allowlist.
        if q.is_empty() {
            return run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
                let mut client = client.lock();
                let since_ref = since_owned.as_deref();
                let until_ref = until_owned.as_deref();
                let stmt = recent_recall_sql_for_agents(
                    &qualified_table,
                    &qualified_agents,
                    since_ref.is_some(),
                    until_ref.is_some(),
                );

                #[allow(clippy::cast_possible_wrap)]
                let limit_i64 = limit as i64;

                let rows = match (since_ref, until_ref) {
                    (Some(s), Some(u)) => client.query(&stmt, &[&allowed, &s, &u, &limit_i64])?,
                    (Some(s), None) => client.query(&stmt, &[&allowed, &s, &limit_i64])?,
                    (None, Some(u)) => client.query(&stmt, &[&allowed, &u, &limit_i64])?,
                    (None, None) => client.query(&stmt, &[&allowed, &limit_i64])?,
                };
                rows.iter()
                    .map(Self::row_to_entry)
                    .collect::<Result<Vec<MemoryEntry>>>()
            })
            .await;
        }

        let client = self.client.get().clone();
        let attempt_timeout = self.recall_soft_timeout;
        let op_timeout = self.op_timeout;

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            let mut client = client.lock();
            let since_ref = since_owned.as_deref();
            let until_ref = until_owned.as_deref();

            let time_filter = recall_time_filter(since_ref.is_some(), until_ref.is_some(), 5);

            let stmt = format!(
                "
                SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8,
                       (
                         CASE WHEN to_tsvector('simple', m.key) @@ plainto_tsquery('simple', $1)
                           THEN ts_rank_cd(to_tsvector('simple', m.key), plainto_tsquery('simple', $1)) * 2.0
                           ELSE 0.0 END +
                         CASE WHEN to_tsvector('simple', m.content) @@ plainto_tsquery('simple', $1)
                           THEN ts_rank_cd(to_tsvector('simple', m.content), plainto_tsquery('simple', $1))
                           ELSE 0.0 END
                       ) AS score
                FROM {qualified_table} m
                LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
                WHERE ($2::TEXT IS NULL OR m.session_id = $2)
                  AND ($1 = '' OR to_tsvector('simple', m.key || ' ' || m.content) @@ plainto_tsquery('simple', $1))
                  AND m.agent_id = ANY($4)
                  {time_filter}
                ORDER BY score DESC, m.updated_at DESC
                LIMIT $3
                ",
            );

            #[allow(clippy::cast_possible_wrap)]
            let limit_i64 = limit as i64;

            // Bound THIS attempt server-side at the soft budget so a caller
            // that gives up leaves no orphan holding the shared connection.
            let entries =
                with_statement_timeout(&mut client, attempt_timeout, op_timeout, |client| {
                    let rows = match (since_ref, until_ref) {
                        (Some(s), Some(u)) => {
                            client.query(&stmt, &[&q, &sid, &limit_i64, &allowed, &s, &u])?
                        }
                        (Some(s), None) => {
                            client.query(&stmt, &[&q, &sid, &limit_i64, &allowed, &s])?
                        }
                        (None, Some(u)) => {
                            client.query(&stmt, &[&q, &sid, &limit_i64, &allowed, &u])?
                        }
                        (None, None) => client.query(&stmt, &[&q, &sid, &limit_i64, &allowed])?,
                    };
                    rows.iter().map(Self::row_to_entry).collect::<Result<Vec<MemoryEntry>>>()
                })?;
            Ok(entries)
        })
        .await
    }

    async fn ensure_agent_uuid(&self, alias: &str) -> Result<String> {
        let client = self.client.get().clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = alias.to_string();
        run_on_os_thread(self.op_timeout, move || -> Result<String> {
            let mut client = client.lock();
            let candidate = Uuid::new_v4().to_string();
            client.execute(
                &format!(
                    "INSERT INTO {qualified_agents} (id, alias, created_at)
                     VALUES ($1, $2, NOW())
                     ON CONFLICT (alias) DO NOTHING"
                ),
                &[&candidate, &alias],
            )?;
            let row: String = client
                .query_one(
                    &format!("SELECT id FROM {qualified_agents} WHERE alias = $1 LIMIT 1"),
                    &[&alias],
                )?
                .get(0);
            Ok(row)
        })
        .await
    }
}

impl ::zeroclaw_api::attribution::Attributable for PostgresMemory {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Memory(::zeroclaw_api::attribution::MemoryKind::Postgres)
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn postgres_test_url() -> String {
        std::env::var("ZEROCLAW_TEST_POSTGRES_URL")
            .expect("set ZEROCLAW_TEST_POSTGRES_URL to run ignored PostgreSQL tests")
    }

    // Tuning cycle 3: the recent-entries (empty-query) path must be an
    // indexed recency read — no per-row tsvector computation — because it is
    // both the bare-recall contract path and the recall budget's fallback.
    #[test]
    fn recent_recall_sql_is_index_backed_and_vector_free() {
        let sql = recent_recall_sql("mem.memories", "mem.agents", true, false);
        assert!(
            !sql.contains("to_tsvector"),
            "the recent path must not compute tsvectors per row"
        );
        assert!(
            sql.contains("ORDER BY m.updated_at DESC"),
            "recency ordering rides the updated_at index"
        );
        assert!(sql.contains("LEFT JOIN mem.agents"), "agent alias join kept");
        assert!(sql.contains("$1::TEXT IS NULL OR m.session_id = $1"));
        assert!(sql.contains("m.created_at >= $2::TIMESTAMPTZ"));
        assert!(sql.contains("LIMIT $3"), "since bound → limit at $3, got: {sql}");
    }

    #[test]
    fn recent_recall_sql_for_agents_filters_by_allowlist_and_is_vector_free() {
        let sql = recent_recall_sql_for_agents("mem.memories", "mem.agents", true, true);
        assert!(!sql.contains("to_tsvector"));
        assert!(sql.contains("m.agent_id = ANY($1)"));
        assert!(sql.contains("m.created_at >= $2::TIMESTAMPTZ"));
        assert!(sql.contains("m.created_at <= $3::TIMESTAMPTZ"));
        assert!(sql.contains("LIMIT $4"), "both time bounds → limit at $4, got: {sql}");
    }

    #[test]
    fn recall_index_statements_carry_the_composite_key_content_fts_index() {
        let statements = PostgresMemory::recall_index_statements("mem.memories");
        let composite = statements
            .iter()
            .find(|(name, _)| *name == "idx_memories_key_content_fts")
            .expect("the composite index must be among the recall index statements")
            .1
            .clone();
        assert!(
            composite.contains("gin(to_tsvector('simple', key || ' ' || content))"),
            "the index expression must match the WHERE expression for the planner to use it, got: {composite}"
        );
    }

    /// The boot-DDL session guard must drop the statement timeout entirely:
    /// it is what exempts the one-time migration from the session
    /// `op_timeout` and any server/role default (canary 2026-10-28).
    #[test]
    fn boot_ddl_guard_exempts_statement_timeouts() {
        let guard = PostgresMemory::BOOT_DDL_SESSION_SQL;
        assert!(
            guard.contains("SET statement_timeout = 0"),
            "the boot-DDL guard must unbound statement_timeout, got: {guard:?}"
        );
        assert!(
            guard.contains("SET idle_in_transaction_session_timeout = 0"),
            "the boot-DDL guard must unbound idle_in_transaction_session_timeout, got: {guard:?}"
        );
    }

    /// The restore after the migration window must carry the operation
    /// timeout — the latency-bounded session normal operations (and the
    /// recall soft budget) rely on must NOT stay unbounded.
    #[test]
    fn bounded_session_restore_carries_the_operation_timeout() {
        let sql = PostgresMemory::bounded_session_sql(Duration::from_secs(30));
        assert!(
            sql.contains("SET statement_timeout = 30000"),
            "the restore must carry the op_timeout for statement_timeout, got: {sql:?}"
        );
        assert!(
            sql.contains("SET idle_in_transaction_session_timeout = 30000"),
            "the restore must carry the op_timeout for idle_in_transaction_session_timeout, got: {sql:?}"
        );
        assert!(
            !sql.contains("= 0"),
            "the restore must not leave the session unbounded, got: {sql:?}"
        );
    }

    /// Each recall index must be a standalone `IF NOT EXISTS` statement: its
    /// own implicit transaction, individually retryable on a later boot, and
    /// a failed build must not roll its siblings back.
    #[test]
    fn every_recall_index_statement_is_standalone_if_not_exists() {
        let statements = PostgresMemory::recall_index_statements("mem.memories");
        let mut names = statements.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        let unique = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            unique,
            "recall index names must be unique: {names:?}"
        );
        for (name, sql) in &statements {
            assert!(
                sql.contains(&format!("CREATE INDEX IF NOT EXISTS {name} ")),
                "each recall index must be a standalone IF NOT EXISTS statement, got: {sql}"
            );
        }
    }

    /// The degradation guard: a failed (e.g. timeout-cancelled) index build
    /// must be attempted, logged, and CONTINUED past — the remaining indexes
    /// are still attempted and the boot proceeds without the failed one.
    #[test]
    fn attempt_recall_indexes_continues_past_a_failed_index_build() {
        struct RecordingExecutor {
            fail_on: &'static str,
            attempted: Vec<String>,
        }

        impl DdlExecutor for RecordingExecutor {
            fn exec_batch(&mut self, sql: &str) -> Result<()> {
                self.attempted.push(sql.to_string());
                if sql.contains(self.fail_on) {
                    anyhow::bail!("canceling statement due to statement timeout");
                }
                Ok(())
            }
        }

        let mut executor = RecordingExecutor {
            fail_on: "idx_memories_key_content_fts",
            attempted: Vec::new(),
        };
        PostgresMemory::attempt_recall_indexes(&mut executor, "mem.memories");

        let expected: Vec<&str> = PostgresMemory::recall_index_statements("mem.memories")
            .iter()
            .map(|(name, _)| *name)
            .collect();
        let attempted_names: Vec<&str> = executor
            .attempted
            .iter()
            .map(|sql| {
                expected
                    .iter()
                    .copied()
                    .find(|name| sql.contains(name))
                    .expect("every attempted statement must be one of the recall indexes")
            })
            .collect();
        assert_eq!(
            attempted_names, expected,
            "every recall index must be attempted exactly once, in order"
        );

        let failed_pos = attempted_names
            .iter()
            .position(|n| *n == "idx_memories_key_content_fts")
            .expect("the failing index must have been attempted");
        let after = &attempted_names[failed_pos + 1..];
        assert!(
            after.contains(&"idx_memories_key_btree"),
            "the boot must continue past the failed build: remaining indexes still attempted, got: {after:?}"
        );
    }

    #[test]
    fn recall_time_filter_qualifies_alias_and_preserves_placeholder_order() {
        let cases = [
            (false, false, 4, ""),
            (true, false, 4, " AND m.created_at >= $4::TIMESTAMPTZ"),
            (false, true, 4, " AND m.created_at <= $4::TIMESTAMPTZ"),
            (
                true,
                true,
                4,
                " AND m.created_at >= $4::TIMESTAMPTZ AND m.created_at <= $5::TIMESTAMPTZ",
            ),
            (false, false, 5, ""),
            (true, false, 5, " AND m.created_at >= $5::TIMESTAMPTZ"),
            (false, true, 5, " AND m.created_at <= $5::TIMESTAMPTZ"),
            (
                true,
                true,
                5,
                " AND m.created_at >= $5::TIMESTAMPTZ AND m.created_at <= $6::TIMESTAMPTZ",
            ),
        ];

        for (since, until, first_placeholder, expected) in cases {
            assert_eq!(
                recall_time_filter(since, until, first_placeholder),
                expected
            );
        }
    }

    #[test]
    fn store_insert_sql_types_importance_as_float8() {
        let sql = store_insert_sql("memory.memories", "memory.agents");

        // Regression: the importance parameter ($10, 0-based bind index 9)
        // must be explicitly FLOAT8. Without the annotation, PostgreSQL
        // infers $10 as NUMERIC and the f64 bind fails with
        // "error serializing parameter 9" (f64 implements ToSql for FLOAT8
        // only). Verified against the live backend: untyped COALESCE($10,
        // 0.5) → numeric(1700); COALESCE($10::FLOAT8, 0.5::FLOAT8) →
        // float8(701).
        assert!(
            sql.contains("COALESCE($10::FLOAT8, 0.5::FLOAT8)"),
            "importance placeholder must be annotated ::FLOAT8, got: {sql}"
        );
        assert!(!sql.contains("COALESCE($10, 0.5)"));
        // Ten parameters bound by store_with_agent.
        for n in 1..=10 {
            assert!(sql.contains(&format!("${n}")), "missing ${n} in: {sql}");
        }
    }

    #[test]
    fn valid_identifiers_pass_validation() {
        assert!(validate_identifier("public", "schema").is_ok());
        assert!(validate_identifier("_memories_01", "table").is_ok());
    }

    #[test]
    fn invalid_identifiers_are_rejected() {
        assert!(validate_identifier("", "schema").is_err());
        assert!(validate_identifier("1bad", "schema").is_err());
        assert!(validate_identifier("bad-name", "table").is_err());
    }

    #[test]
    fn parse_category_maps_known_and_custom_values() {
        assert_eq!(PostgresMemory::parse_category("core"), MemoryCategory::Core);
        assert_eq!(
            PostgresMemory::parse_category("daily"),
            MemoryCategory::Daily
        );
        assert_eq!(
            PostgresMemory::parse_category("conversation"),
            MemoryCategory::Conversation
        );
        assert_eq!(
            PostgresMemory::parse_category("custom_notes"),
            MemoryCategory::Custom("custom_notes".into())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_on_thread_drops_value_on_plain_os_thread() {
        let (tx, rx) = oneshot::channel::<bool>();

        struct DropGuard(Option<oneshot::Sender<bool>>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                // true  → dropped on a plain OS thread (no active Tokio runtime) ✓
                // false → dropped on a Tokio runtime thread ✗
                let on_plain_thread = tokio::runtime::Handle::try_current().is_err();
                let _ = self.0.take().unwrap().send(on_plain_thread);
            }
        }

        // Drop DropOnThread from inside the Tokio runtime — this is the
        // scenario that caused the nested-runtime panic in production.
        drop(DropOnThread::new(DropGuard(Some(tx))));

        let on_plain_thread = rx.await.expect("DropGuard did not fire");
        assert!(
            on_plain_thread,
            "DropOnThread must run Drop on a plain OS thread, not a Tokio runtime thread"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_does_not_panic_inside_tokio_runtime() {
        let outcome = std::panic::catch_unwind(|| {
            PostgresMemory::new(
                "test",
                "postgres://zeroclaw:password@127.0.0.1:1/zeroclaw",
                "public",
                "memories",
                Some(1),
                None,
                None,
                None,
                None,
            )
        });

        assert!(outcome.is_ok(), "PostgresMemory::new should not panic");
        assert!(
            outcome.unwrap().is_err(),
            "PostgresMemory::new should return a connect error for an unreachable endpoint"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_with_pgvector_enabled_does_not_panic_inside_tokio_runtime() {
        let outcome = std::panic::catch_unwind(|| {
            PostgresMemory::new(
                "test",
                "postgres://zeroclaw:password@127.0.0.1:1/zeroclaw",
                "public",
                "memories",
                Some(1),
                None,
                Some(true),
                Some(4),
                None,
            )
        });

        assert!(
            outcome.is_ok(),
            "PostgresMemory::new should not panic with pgvector enabled"
        );
        assert!(
            outcome.unwrap().is_err(),
            "PostgresMemory::new should return a connect error for an unreachable endpoint"
        );
    }

    struct TestSchema {
        admin: Client,
        name: String,
    }

    impl TestSchema {
        fn create(database_url: &str, purpose: &str) -> Self {
            let name = format!("zc_mem_{purpose}_{}", std::process::id());
            let mut admin = Client::connect(database_url, NoTls).unwrap();
            admin
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}",
                    quote_identifier(&name)
                ))
                .unwrap();
            Self { admin, name }
        }
    }

    impl Drop for TestSchema {
        fn drop(&mut self) {
            let _ = self.admin.batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                quote_identifier(&self.name)
            ));
        }
    }

    /// Live regression for the nested-runtime panic in the pgvector-enabled
    /// construction path: `try_enable_pgvector` must run inside the
    /// `postgres-memory-init` OS thread, because the synchronous postgres
    /// client calls `block_on` internally and panics on a Tokio runtime
    /// thread. The pgvector extension does not need to be installed in the
    /// target database — an unavailable extension takes the graceful
    /// keyword-only fallback, while the pre-fix code panics before that
    /// fallback is reached.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_new_with_pgvector_enabled_from_tokio_runtime_does_not_panic() {
        let database_url = std::env::var("ZEROCLAW_TEST_POSTGRES_URL")
            .expect("set ZEROCLAW_TEST_POSTGRES_URL to run ignored PostgreSQL tests");
        let schema = TestSchema::create(&database_url, "vec");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let memory = runtime.block_on(async {
            PostgresMemory::new(
                "test",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                Some(true),
                Some(4),
                None,
            )
        });

        let memory =
            memory.expect("pgvector-enabled construction from a Tokio runtime should succeed");
        drop(memory);
        drop(runtime);
    }

    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_backend_round_trip_uses_isolated_schema() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "round_trip");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let memory = PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                Some(false),
                None,
                None,
            )
            .unwrap();

            memory
                .store(
                    "ci_key",
                    "PostgreSQL CI value",
                    MemoryCategory::Core,
                    Some("ci_session"),
                )
                .await
                .unwrap();
            let stored = memory.get("ci_key").await.unwrap().unwrap();
            assert_eq!(stored.content, "PostgreSQL CI value");
            assert_eq!(memory.count().await.unwrap(), 1);
            assert!(memory.health_check().await);
            assert!(memory.forget("ci_key").await.unwrap());
            assert_eq!(memory.count().await.unwrap(), 0);
        });
    }

    /// Seeds the boot-DDL regression fixture: the pre-V3 memories table shape
    /// with enough rows that the GIN tsvector index builds measurably exceed a
    /// 1 s `op_timeout` (calibrated 2026-10-28: ~3 s for the composite build).
    fn seed_oversized_memories(admin: &mut Client, schema_name: &str) {
        admin
            .batch_execute(&format!(
                "CREATE TABLE {schema}.memories (
                    id TEXT PRIMARY KEY,
                    key TEXT NOT NULL,
                    content TEXT NOT NULL,
                    category TEXT NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL,
                    session_id TEXT
                );
                INSERT INTO {schema}.memories (id, key, content, category, created_at, updated_at)
                SELECT 'seed_' || g, 'seed_key_' || g, repeat(md5(g::text || 'bootddl'), 32),
                       'daily', NOW(), NOW()
                FROM generate_series(1, 100000) g;",
                schema = quote_identifier(schema_name)
            ))
            .expect("seeding the oversized boot-DDL fixture must succeed");
    }

    /// Live regression for the 2026-10-28 canary boot crash-loop: the
    /// boot-time schema DDL (including the `idx_memories_key_content_fts` GIN
    /// build — measured 88 s on the 50 K-row canary store) ran under the
    /// session-level `op_timeout` `statement_timeout`; the cancelled build
    /// rolled back with the whole `batch_execute` implicit transaction, so
    /// every boot retry re-attempted it and the pod never came up. The
    /// contract: constructing the backend MUST succeed even when the
    /// recall-index builds exceed the configured `op_timeout` — boot DDL is a
    /// one-time migration, not a latency-bounded query.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_new_survives_recall_index_builds_slower_than_op_timeout() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "bootddl");
        seed_oversized_memories(&mut schema.admin, &schema.name);

        let memory = PostgresMemory::new(
            "test",
            &database_url,
            &schema.name,
            "memories",
            Some(5),
            Some(1), // op_timeout 1 s — far below the seeded index build time
            Some(false),
            None,
            None,
        )
        .expect("boot must survive a recall-index build slower than op_timeout");

        // The restored bounded session must still serve normal operations.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime.block_on(memory.health_check()),
            "the session restored after the boot DDL must serve operations"
        );
    }

    /// The timeout-exemption contract at the schema-init seam: a session with
    /// `statement_timeout = 100ms` (the server/role-default scenario) must
    /// still complete the boot schema-init — the DDL guard overrides the
    /// session bound for the migration window — and the composite recall
    /// index must actually persist (the incident's poison was the
    /// transactional rollback losing the index on every retry).
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn init_schema_completes_under_a_bounded_statement_timeout() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "bootddl100");
        seed_oversized_memories(&mut schema.admin, &schema.name);

        let mut client = Client::connect(&database_url, NoTls).unwrap();
        client.batch_execute("SET statement_timeout = 100").unwrap();

        let schema_ident = quote_identifier(&schema.name);
        let qualified_table = format!("{schema_ident}.{}", quote_identifier("memories"));
        PostgresMemory::init_schema(&mut client, &schema_ident, &qualified_table)
            .expect("schema-init must complete under a bounded session statement_timeout");

        let index_present: bool = client
            .query_one(
                "SELECT EXISTS (
                    SELECT 1 FROM pg_indexes
                    WHERE schemaname = $1 AND indexname = 'idx_memories_key_content_fts'
                )",
                &[&schema.name],
            )
            .unwrap()
            .get(0);
        assert!(
            index_present,
            "the composite recall index must persist after the schema-init"
        );
    }

    #[test]
    fn rewrites_only_values_that_change_under_sanitization() {
        let distinct = vec![
            "slack_C123_1.2_user one".to_string(),
            "already_sanitized".to_string(),
            "whatsapp_123@g.us_alice".to_string(),
            "abc-DEF_123".to_string(),
        ];

        let rewrites = PostgresMemory::compute_session_id_rewrites(&distinct);
        assert_eq!(rewrites.len(), 2, "only the two raw forms need rewriting");

        let by_old: std::collections::HashMap<_, _> = rewrites.into_iter().collect();
        assert_eq!(
            by_old.get("slack_C123_1.2_user one").map(String::as_str),
            Some("slack_C123_1_2_user_one")
        );
        assert_eq!(
            by_old.get("whatsapp_123@g.us_alice").map(String::as_str),
            Some("whatsapp_123_g_us_alice")
        );
    }

    #[test]
    fn no_rewrites_when_all_values_already_sanitized() {
        let distinct = vec![
            "slack_C123_1_2_user_one".to_string(),
            "abc-DEF_123".to_string(),
            "".to_string(),
        ];
        let rewrites = PostgresMemory::compute_session_id_rewrites(&distinct);
        assert!(
            rewrites.is_empty(),
            "no UPDATE should be issued when every value is already sanitized"
        );
    }

    #[test]
    fn rewrite_plan_is_idempotent_when_reapplied() {
        let raw = "slack_C123_1.2_user one";
        let sanitized = sanitize_session_key(raw);

        let first = PostgresMemory::compute_session_id_rewrites(&[raw.to_string()]);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1, sanitized);

        let second = PostgresMemory::compute_session_id_rewrites(&[sanitized]);
        assert!(
            second.is_empty(),
            "re-running the plan over the rewritten value yields no further rewrite"
        );
    }

    #[tokio::test]
    async fn op_thread_timeout_bounds_hung_operations() {
        let started = std::time::Instant::now();
        let result = run_on_os_thread(Duration::from_millis(50), move || {
            std::thread::sleep(Duration::from_millis(2_000));
            Ok(())
        })
        .await;
        assert!(
            result.is_err(),
            "a hung operation must resolve into an error"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1_000),
            "the await must unblock at the timeout, not wait out the operation: {:?}",
            started.elapsed()
        );
    }
}
