//! PostgreSQL-backed memory implementation.

use super::traits::{
    ExportFilter, Memory, MemoryCategory, MemoryEntry, normalize_recent_recall_query,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::{Condvar, Mutex};
use postgres::{Client, Error, NoTls, Row};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;
use zeroclaw_api::session_keys::sanitize_session_key;

/// Maximum allowed connect timeout (seconds) to avoid unreasonable waits.
const POSTGRES_CONNECT_TIMEOUT_CAP_SECS: u64 = 300;

/// Default connect timeout (seconds) when the config sets no
/// `connect_timeout_secs`. The deployed `[storage.postgres]` sets NEITHER
/// `connect_timeout_secs` NOR `op_timeout_secs`, so the shared connect home
/// carries its own bound instead of hanging a reconnect on a half-open TCP
/// connect.
const POSTGRES_CONNECT_TIMEOUT_DEFAULT_SECS: u64 = 5;

/// Default per-operation timeout (seconds) bounding each memory query.
const DEFAULT_OP_TIMEOUT_SECS: u64 = 30;

/// Maximum allowed per-operation timeout (seconds).
const POSTGRES_OP_TIMEOUT_CAP_SECS: u64 = 300;

/// Default pool size when `[storage.postgres] pool_size` is unset: four
/// connections serve concurrent memory operations without pinning the
/// server's connection slots. Unset means the default fires with NO debops
/// config change.
const POSTGRES_POOL_DEFAULT: u64 = 4;

/// Hard ceiling for the pool size: every zeroclaw backend shares the
/// primary's connection budget, so one backend never holds more than 16.
const POSTGRES_POOL_CAP: u64 = 16;

/// Resolve the configured pool size: `None` takes the default (4), 0 clamps
/// UP to 1 (a zero-sized pool would deadlock every operation), and values
/// above [`POSTGRES_POOL_CAP`] clamp down to the cap.
fn resolve_pool_size(pool_size: Option<u64>) -> usize {
    match pool_size {
        None => POSTGRES_POOL_DEFAULT as usize,
        Some(size) => size.clamp(1, POSTGRES_POOL_CAP) as usize,
    }
}

/// Drop `value` on a freshly spawned plain OS thread.
///
/// `postgres::Client::drop` calls `Runtime::block_on` internally, which
/// panics on a Tokio runtime thread — every Client drop in this module MUST
/// ride this helper. The one home for that law (extracted from
/// `DropOnThread`'s Drop body so mid-flight pool discards ride it too).
fn drop_on_thread<T: Send + 'static>(value: T) {
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
        drop_on_thread(value);
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

/// The pool's guarded state: parked connections and the count of
/// connections currently checked out. ONE mutex guards both; the condvar
/// pairs with it.
struct PoolState {
    idle: Vec<Client>,
    live: usize,
}

/// The failure classes of a pooled operation, decided by
/// [`classify_failure`].
enum FailureClass {
    /// The server answered a complete error response — the protocol is
    /// intact and the connection healthy. Release + propagate. NO retry: a
    /// statement-timeout cancel (the recall soft budget) must surface fast
    /// to the recall-budget fallback, and SQL errors are not transient.
    Sql,
    /// The connection is dead or untrustworthy: discard it, attempt ONE
    /// reconnect, retry the operation ONCE on the fresh connection.
    ConnectionLoss,
    /// Everything else — protocol desync without a closed marker
    /// (tokio-postgres 0.7.18's `Kind::UnexpectedMessage` has no public
    /// discriminator), client-side conversion errors, application errors.
    /// Discard + surface: post-desync state is untrustworthy, and an
    /// operation that failed mid-flight may leave the connection carrying
    /// state (e.g. an uncommitted transaction) that the next operation must
    /// not inherit.
    Unclassified,
}

/// Find the `postgres::Error` in an operation's failure chain: `?`-converted
/// errors downcast directly; `.context()`-wrapped ones surface in the chain
/// walk (anyhow's `chain()` starts with the error itself).
fn failure_pg_error(err: &anyhow::Error) -> Option<&Error> {
    err.chain().find_map(|cause| cause.downcast_ref::<Error>())
}

/// Classify an operation failure against the connection it ran on.
fn classify_failure(err: &anyhow::Error, client: &Client) -> FailureClass {
    let Some(pg) = failure_pg_error(err) else {
        return FailureClass::Unclassified;
    };
    // CONNECTION-LOSS FIRST: a FATAL server error (e.g. SQLSTATE 57P01,
    // `pg_terminate_backend` / a patroni shutdown) arrives as a perfectly
    // well-formed DbError, but the session behind it is DEAD — the server
    // sends the ErrorResponse and closes the socket. The closed-state
    // primary below catches exactly that (the connection task's terminal
    // poll closes the channel before the error reaches the caller). A
    // healthy ERROR-severity DbError (statement_timeout cancel, constraint
    // violation) leaves the client open and falls through to the SQL class.
    if is_connection_loss(pg, client) {
        return FailureClass::ConnectionLoss;
    }
    if pg.as_db_error().is_some() {
        return FailureClass::Sql;
    }
    FailureClass::Unclassified
}

/// The PRIMARY connection-loss signal: the closed disjunction.
/// `Error::is_closed()` is true for the synthetic `Error::closed()` a server
/// EOF surfaces (patroni restart, `pg_terminate_backend` — tokio-postgres
/// connection.rs:104); `Client::is_closed()` is set on ANY connection-task
/// death (the connection task closes its request channel on its terminal
/// poll, before the error reaches the caller).
fn connection_loss_disjunction(err_is_closed: bool, client_is_closed: bool) -> bool {
    err_is_closed || client_is_closed
}

/// Is the underlying connection dead (or untrustworthy) enough to warrant
/// discard + reconnect + one retry?
///
/// PRIMARY: the closed disjunction ([`connection_loss_disjunction`]).
/// BELT: the trait `Error::source()` io-downcast — an `Error::io()` carries
/// the underlying `io::Error` as its cause, and a read/write failure means
/// the socket state is unknown. `into_source()` is NOT used — it consumes
/// the error. The tokio-postgres `Kind` enum is PRIVATE in 0.7.18 (no
/// public `kind()`): NOTHING may depend on it; the unclassifiable middle is
/// handled by exclusion ([`FailureClass::Unclassified`]).
fn is_connection_loss(e: &Error, client: &Client) -> bool {
    connection_loss_disjunction(e.is_closed(), client.is_closed())
        || std::error::Error::source(e)
            .is_some_and(|source| source.downcast_ref::<std::io::Error>().is_some())
}

/// A bounded pool of PostgreSQL clients for ONE memory backend.
///
/// Replaces the single `Mutex<Client>`: a dead connection (server restart,
/// `pg_terminate_backend`, network loss) is DISCARDED and reconnected on the
/// next failed operation instead of poisoning every later operation — the
/// 2026-11-17/18 fleet-death class (tokio-postgres has no auto-reconnect;
/// every webhook turn failed instantly until a manual pod roll).
///
/// Behind an [`Arc`], wrapped in the existing `DropOnThread` (the pool's
/// idle `Client`s drop on a plain OS thread via [`drop_on_thread`]).
struct ConnectionPool {
    state: Mutex<PoolState>,
    /// Wakes acquire-waiters on a release or a discard (a freed slot).
    signal: Condvar,
    /// The ceiling on owned connections (idle + checked out). The RESOLVED
    /// pool size — a boot that degraded below it keeps the ceiling so the
    /// pool can expand at runtime when connects succeed again.
    max: usize,
    db_url: String,
    connect_timeout_secs: Option<u64>,
    op_timeout: Duration,
}

impl ConnectionPool {
    fn new(
        primary: Client,
        extras: Vec<Client>,
        max: usize,
        db_url: String,
        connect_timeout_secs: Option<u64>,
        op_timeout: Duration,
    ) -> Self {
        let mut idle = Vec::with_capacity(1 + extras.len());
        idle.push(primary);
        idle.extend(extras);
        Self {
            state: Mutex::new(PoolState { idle, live: 0 }),
            signal: Condvar::new(),
            max,
            db_url,
            connect_timeout_secs,
            op_timeout,
        }
    }

    /// The shared connect-input home (boot + reconnect): parse the URL,
    /// bound the connect timeout (the config value or the ~5s default,
    /// always capped), and set the TCP keepalives — idle 15s, retries 3,
    /// and INTERVAL 15s. The interval was unset before (OS default ~75s
    /// probes): the end-to-end dead-connection detection window was ~240s;
    /// the explicit interval bounds it near ~60s.
    fn connect_config(db_url: &str, connect_timeout_secs: Option<u64>) -> Result<postgres::Config> {
        let mut config: postgres::Config = db_url
            .parse()
            .context("invalid PostgreSQL connection URL")?;
        let bounded = connect_timeout_secs
            .unwrap_or(POSTGRES_CONNECT_TIMEOUT_DEFAULT_SECS)
            .min(POSTGRES_CONNECT_TIMEOUT_CAP_SECS);
        config.connect_timeout(Duration::from_secs(bounded));
        config.keepalives(true);
        config.keepalives_idle(Duration::from_secs(15));
        config.keepalives_retries(3);
        config.keepalives_interval(Duration::from_secs(15));
        Ok(config)
    }

    /// The reconnect factory (and the boot's extra connects): ONE fresh
    /// connection carrying the bounded session. NO boot DDL, NO pgvector
    /// setup — a wiped-DB scenario surfaces an honest SQL error and the
    /// next boot rebuilds the schema.
    fn connect_one(
        db_url: &str,
        connect_timeout_secs: Option<u64>,
        op_timeout: Duration,
    ) -> Result<Client> {
        let mut client = Self::connect_config(db_url, connect_timeout_secs)?
            .connect(NoTls)
            .context("failed to connect to PostgreSQL memory backend")?;
        client
            .batch_execute(&PostgresMemory::bounded_session_sql(op_timeout))
            .context("failed to apply PostgreSQL session timeouts")?;
        Ok(client)
    }

    /// Check a connection out: pop the idle set; if empty and the pool can
    /// still grow, connect one now; otherwise wait on the condvar for a
    /// release or a discard. The slot is RESERVED before the lock is
    /// dropped for the connect, so concurrent expansions cannot exceed
    /// `max`; a failed expansion connect frees the reservation and falls
    /// back to waiting (a degraded pool keeps SERVING instead of failing
    /// the overflow outright).
    fn acquire(self: Arc<Self>) -> Result<PooledConn> {
        let mut state = self.state.lock();
        loop {
            if let Some(client) = state.idle.pop() {
                state.live += 1;
                // Drop the state lock BEFORE moving the pool Arc into the
                // guard: the client is already out of the idle set and its
                // slot accounted, so the unlocked window is race-free.
                drop(state);
                return Ok(PooledConn {
                    client: Some(client),
                    pool: self,
                });
            }
            if state.live < self.max {
                // Reserve the slot BEFORE releasing the lock for the
                // connect: concurrent expansions stay inside `max`, and a
                // ~5s connect never blocks a release behind the state lock.
                state.live += 1;
                drop(state);
                match Self::connect_one(&self.db_url, self.connect_timeout_secs, self.op_timeout) {
                    Ok(client) => {
                        return Ok(PooledConn {
                            client: Some(client),
                            pool: self,
                        });
                    }
                    Err(err) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "max": self.max,
                                "error": err.to_string(),
                            })),
                            "PostgreSQL pool expansion connect failed; waiting for a released \
                             connection instead of failing the operation"
                        );
                        {
                            let mut reservation = self.state.lock();
                            reservation.live -= 1;
                        }
                        self.signal.notify_one();
                        state = self.state.lock();
                    }
                }
            }
            // No idle connection and no expansion slot: park until a release
            // or a discard frees one (each notifies).
            self.signal.wait(&mut state);
        }
    }

    /// Return a healthy connection to the idle set.
    fn release_conn(&self, client: Client) {
        let mut state = self.state.lock();
        state.live -= 1;
        state.idle.push(client);
        drop(state);
        self.signal.notify_one();
    }

    /// Discard a dead (or untrusted) connection: the client drops on a
    /// plain OS thread ([`drop_on_thread`] — the drop calls
    /// `Runtime::block_on` internally), and the freed slot wakes waiters.
    fn discard_conn(&self, client: Client) {
        drop_on_thread(client);
        let mut state = self.state.lock();
        state.live -= 1;
        drop(state);
        self.signal.notify_one();
    }

    /// The pool's connection census (idle + checked out) — test-only
    /// observability (the pool tests).
    #[cfg(test)]
    fn owned_connections(&self) -> usize {
        let state = self.state.lock();
        state.idle.len() + state.live
    }

    /// THE one pooled-operation seam: check a connection out, run `op`,
    /// classify the outcome.
    ///
    /// - `Ok` → the connection returns to the idle set.
    /// - Connection loss ([`is_connection_loss`]) → the dead connection is
    ///   DISCARDED (the drop rides [`drop_on_thread`]), ONE reconnect is
    ///   attempted (bounded by the ~5s default connect timeout), and the
    ///   operation runs ONCE on the fresh connection; any further failure
    ///   surfaces. Disclosed duplicate-effect edge: a retried write may
    ///   re-execute (the upsert path is idempotent by key; an ambiguous
    ///   retry's `created_at` shift is benign — `updated_at` is the
    ///   ordering key).
    /// - SQL-class ([`FailureClass::Sql`]) → release + propagate, no retry.
    /// - Anything else ([`FailureClass::Unclassified`]) → the connection is
    ///   DISCARDED and the error surfaces: post-desync state is
    ///   untrustworthy, and a failed operation may have left uncommitted
    ///   transaction state the next operation must not inherit.
    /// - A panic unwinds through the [`PooledConn`] Drop, which discards.
    ///
    /// Disclosed aggregate: `run_on_os_thread` DETACHES the operation
    /// thread, so N concurrent timed-out operations park up to
    /// min(N, max) pool slots until the server-side statement timeout (or
    /// the TCP keepalive) kills the query. The bounded condvar wait on
    /// acquire is the hardening if evidence demands it.
    fn with<T>(self: Arc<Self>, op: impl Fn(&mut Client) -> Result<T>) -> Result<T> {
        let mut guard = self.clone().acquire()?;
        match op(&mut guard) {
            Ok(value) => {
                guard.release();
                Ok(value)
            }
            Err(err) => {
                let class = classify_failure(&err, &guard);
                match class {
                    FailureClass::Sql => {
                        guard.release();
                        Err(err)
                    }
                    FailureClass::Unclassified => {
                        guard.discard();
                        Err(err)
                    }
                    FailureClass::ConnectionLoss => {
                        guard.discard();
                        match Self::connect_one(
                            &self.db_url,
                            self.connect_timeout_secs,
                            self.op_timeout,
                        ) {
                            Ok(client) => {
                                {
                                    let mut state = self.state.lock();
                                    state.live += 1;
                                }
                                let mut fresh = PooledConn {
                                    client: Some(client),
                                    pool: self,
                                };
                                match op(&mut fresh) {
                                    Ok(value) => {
                                        fresh.release();
                                        Ok(value)
                                    }
                                    Err(retry_err) => {
                                        let retry_class = classify_failure(&retry_err, &fresh);
                                        match retry_class {
                                            FailureClass::Sql => fresh.release(),
                                            _ => fresh.discard(),
                                        }
                                        Err(retry_err)
                                    }
                                }
                            }
                            Err(connect_err) => {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Fail
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "original": err.to_string(),
                                        })
                                    ),
                                    "PostgreSQL pool reconnect after connection loss failed; \
                                     the next operation will retry the reconnect"
                                );
                                Err(connect_err.context(
                                    "postgres memory pool reconnect after connection loss failed",
                                ))
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Drop for ConnectionPool {
    fn drop(&mut self) {
        // The pool outlives its operations (every op thread holds a pool
        // Arc), so `&mut self` here implies no connection is checked out.
        // The idle clients drop on a plain OS thread — never on a Tokio
        // runtime thread.
        let state = self.state.get_mut();
        let idle = std::mem::take(&mut state.idle);
        if !idle.is_empty() {
            drop_on_thread(idle);
        }
    }
}

/// RAII guard for one checked-out pool connection.
///
/// Drop = DISCARD (never an idle return): a guard that reaches Drop outside
/// the explicit release path sits on a failure or unwind path where the
/// connection's state is untrusted — returning it to idle would launder the
/// poison into the next operation.
struct PooledConn {
    client: Option<Client>,
    pool: Arc<ConnectionPool>,
}

impl PooledConn {
    /// Release a healthy connection back to the idle set.
    fn release(mut self) {
        if let Some(client) = self.client.take() {
            self.pool.release_conn(client);
        }
    }

    /// Discard the connection (dead or untrusted): rides the pool's
    /// discard path.
    fn discard(self) {
        // `client` stays Some — the Drop impl runs the discard.
        drop(self);
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            self.pool.discard_conn(client);
        }
    }
}

impl Deref for PooledConn {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.client.as_ref().expect("PooledConn already released")
    }
}

impl DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("PooledConn already released")
    }
}

pub struct PostgresMemory {
    alias: String,
    pool: DropOnThread<Arc<ConnectionPool>>,
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
    /// by the server, freeing the pooled connection for the recall budget's
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
        pool_size: Option<u64>,
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

        let (pool, pgvector_ok) = Self::initialize_client(
            db_url.to_string(),
            connect_timeout_secs,
            op_timeout,
            schema_ident.clone(),
            qualified_table.clone(),
            pgvector,
            resolve_pool_size(pool_size),
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
            pool: DropOnThread::new(pool),
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
        pool_size: usize,
    ) -> Result<(Arc<ConnectionPool>, bool)> {
        let init_handle = std::thread::Builder::new()
            .name("postgres-memory-init".to_string())
            .spawn(move || -> Result<(Arc<ConnectionPool>, bool)> {
                // Connection #1: the shared connect home + the FULL boot
                // migration path (schema DDL, V3 migration, pgvector).
                let mut client = ConnectionPool::connect_config(&db_url, connect_timeout_secs)?
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

                // The remaining pool_size - 1 connections connect
                // SEQUENTIALLY in this SAME init thread — boot stays
                // single-threaded (the v0.9.0 startup-deadlock surface is
                // not widened). A failed extra connection is a LOUD WARN and
                // CONTINUES: a degraded pool still serves operations, and
                // the pool keeps its ceiling so it can expand at runtime
                // when connects succeed again.
                let mut extras = Vec::new();
                for slot in 1..pool_size {
                    match ConnectionPool::connect_one(&db_url, connect_timeout_secs, op_timeout) {
                        Ok(client) => extras.push(client),
                        Err(err) => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Fail
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({
                                    "slot": slot,
                                    "pool_size": pool_size,
                                    "error": err.to_string(),
                                })),
                                "PostgreSQL pool connect failed at boot; continuing with a \
                                 smaller pool — the degraded pool still serves operations"
                            );
                        }
                    }
                }

                let pool = ConnectionPool::new(
                    client,
                    extras,
                    pool_size,
                    db_url,
                    connect_timeout_secs,
                    op_timeout,
                );
                Ok((Arc::new(pool), pgvector_ok))
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

/// Ranking input bound for the content arm of the recall score expression.
///
/// The ranked arm re-parses `to_tsvector(m.content)` for every candidate row
/// the FTS filter produced. On a large store with blob-sized entries (SOP run
/// transcripts, tool-output dumps with >2047-char tokens) that parse dominates
/// the whole query: measured 2026-10-28 on the prod gateway leader, the same
/// 270-candidate recall took 8.07s unbounded vs 0.22s with a 4096-char prefix.
/// The unbounded shape burned the 2s recall soft budget on every unscoped
/// recall (`recall_budget` degradation rows in prod traces) and fell back to
/// recent entries — the ranked results were computed and then thrown away.
///
/// The FTS FILTER stays full-content, so a document that matches only deep in
/// its body is still recalled — it ranks by its key score / recency instead of
/// a deep-position content rank. Keys stay unbounded (they are short).
const RECALL_RANK_CONTENT_PREFIX_CHARS: usize = 4096;

/// Which text-search parser a ranked-recall shape binds.
///
/// `Strict` is the historical conjunction: every token must match
/// (`plainto_tsquery` cannot express OR at all). `Relaxed` is the OR fallback,
/// used when the strict shape returns nothing for a multi-token query:
/// `websearch_to_tsquery` honours the caller-built `token or token` string and
/// can never raise a syntax error on user input — which is why the fallback
/// binds a string we build rather than `to_tsquery` syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecallTsqueryParse {
    Strict,
    Relaxed,
}

impl RecallTsqueryParse {
    fn as_sql(self) -> &'static str {
        match self {
            Self::Strict => "plainto_tsquery",
            Self::Relaxed => "websearch_to_tsquery",
        }
    }
}

/// Build the OR-fallback input from an already-normalized query.
///
/// Tokens are the alphanumeric/underscore runs (punctuation contributes no
/// lexeme, so punctuation-only fragments are dropped — an OR over them is
/// still empty). Fewer than two tokens means there is nothing to relax: a
/// one-token OR IS the same conjunction, so no fallback is built.
fn tsquery_or_fallback_input(query: &str) -> Option<String> {
    // Whitespace-split, then drop tokens that carry no lexeme at all. The
    // punctuation AROUND a token is kept: `websearch_to_tsquery` ignores it,
    // and stripping it would rewrite the caller's terms.
    let tokens: Vec<&str> = query
        .split_whitespace()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .collect();
    if tokens.len() < 2 {
        None
    } else {
        Some(tokens.join(" or "))
    }
}

/// The recall score expression shared by `recall` and `recall_for_agents`.
/// `$1` is the plainto_tsquery parameter; caller supplies the FROM alias
/// (`m.`), which both call sites already use.
///
/// Built with real newlines, never `\`-newline continuations: a Rust string
/// continuation strips the newline AND the next line's leading whitespace,
/// which fused `* 2.0` + `ELSE` into the parse-fatal `2.0ELSE` token
/// (server-proven 2026-11-18: every non-empty-query ranked recall died at
/// parse with "trailing junk after numeric literal"). Newlines are valid SQL
/// whitespace; the leading indentation below is too.
fn recall_rank_expression(parse: RecallTsqueryParse) -> String {
    let tsq = parse.as_sql();
    format!(
        "LEAST(
         CASE WHEN to_tsvector('simple', m.key) @@ {tsq}('simple', $1)
           THEN ts_rank_cd(to_tsvector('simple', m.key), {tsq}('simple', $1), 32) * 2.0
           ELSE 0.0 END +
         CASE WHEN to_tsvector('simple', left(m.content, {prefix})) @@ {tsq}('simple', $1)
           THEN ts_rank_cd(to_tsvector('simple', left(m.content, {prefix})), {tsq}('simple', $1), 32)
           ELSE 0.0 END, 1.0)",
        prefix = RECALL_RANK_CONTENT_PREFIX_CHARS,
    )
}

/// Parse an RFC 3339 recall time bound into a `TIMESTAMPTZ` instant. The
/// bound is VALIDATED and compared as an instant, never as a string: a text
/// bind cannot serialize into the server-inferred timestamptz parameter —
/// the server assigns an untyped `$n::TIMESTAMPTZ` parameter the cast
/// target's type, and postgres-types 0.2.14's `&str::accepts` covers only
/// the text-ish types — and an unparseable bound fails LOUDLY here with the
/// bound named, never silently dropping the window (a bounded recall that
/// degrades into an unbounded one is the silent-recall hazard class; the
/// same shape proven live on the export path, 2026-11-19). The bound is
/// VALIDATED and compared as an instant, never as a string — the RFC 3339
/// string comparison mis-ordered mixed-offset spellings
/// (`2026-01-01T01:00:00+02:00` sorts after `2026-01-01T00:00:00Z` though
/// it is an EARLIER instant); a malformed bound fails the read loudly (the
/// old string compare silently produced an empty export).
/// (Rebase note: the RECALL-TIME-BOUND program authored an identical
/// helper for the recall binds; the two collapsed to this single home.)
fn parse_rfc3339_bound(bound: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(bound)
        .map(|dt| dt.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC 3339 time bound: {bound:?}"))
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

/// The session-scope predicate shared by every recall path — the
/// durable-global carve-out, the sqlite contract ported to postgres
/// (fusion verdict 2026-11-18, 3/3 seats; sqlite held it since 2026-07-09
/// in `vector_search`/FTS and the hybrid stage). The caller's session
/// binds at `session_placeholder`; rows from that session pass, and
/// session-NULL `core`/`daily` rows — durable global knowledge, the set
/// from [`crate::durable_global_category_names`] bound as a text array at
/// `categories_placeholder` — are admitted from any session. Other
/// sessions' rows (INCLUDING their session-bound `core` rows) and
/// session-NULL rows of every other category stay excluded.
///
/// The `$N::TEXT IS NULL` escape keeps the session-less call shape
/// (`session_id = None`) admitting everything, exactly as the strict
/// predicate it replaces did — with no session asked for, the carve-out
/// arm is subsumed. `session_id` is a conversation-scope axis, never a
/// visibility gate.
///
/// Single line by construction — no `\`-newline continuations (the
/// RECALL-RANK-WS law: Rust strips the continuation plus the next line's
/// leading whitespace and silently fuses SQL tokens).
fn recall_session_scope_predicate(
    session_placeholder: usize,
    categories_placeholder: usize,
) -> String {
    format!(
        "(${session}::TEXT IS NULL OR m.session_id = ${session} OR (m.session_id IS NULL AND m.category = ANY(${categories})))",
        session = session_placeholder,
        categories = categories_placeholder,
    )
}

/// The recent-entries recall: indexed recency (`idx_memories_updated_at`),
/// NO full-text scoring. The empty-query contract (bare `*` / omitted query
/// returns recent entries) must not pay the per-row tsvector cost of the
/// ranked query — on the measured 50K-row store the ranked shape seq-scanned
/// every row even for a bare recall because the constant-zero score
/// expressions still computed `to_tsvector` per row. This is also the
/// degraded shape the recall budget's fallback lands on.
///
/// Carries the durable-global carve-out (see
/// [`recall_session_scope_predicate`]): the session-scoped fallback still
/// surfaces session-NULL `core`/`daily` knowledge.
///
/// Placeholders: `$1` session_id, `$2..` the enabled time bounds, then the
/// carve-out category array, LIMIT last.
fn recent_recall_sql(
    qualified_table: &str,
    qualified_agents: &str,
    since: bool,
    until: bool,
) -> String {
    let time_filter = recall_time_filter(since, until, 2);
    let categories_placeholder = 2 + usize::from(since) + usize::from(until);
    let limit_placeholder = categories_placeholder + 1;
    let session_filter = recall_session_scope_predicate(1, categories_placeholder);
    format!(
        "
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
        FROM {qualified_table} m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
        WHERE {session_filter}
          {time_filter}
        ORDER BY m.updated_at DESC
        LIMIT ${limit_placeholder}
        "
    )
}

/// The agent-scoped variant of [`recent_recall_sql`]. Carries the same
/// durable-global carve-out — this is the recall budget's fallback shape for
/// agent-scoped recalls, and the pre-fix shape DROPPED the session filter
/// entirely here (a session-scoped recall that tripped the budget fell back
/// to ALL sessions' rows — the budget-fallback bleed the fusion report
/// flagged). Placeholders: `$1` the agent allowlist, `$2` session_id,
/// `$3..` the enabled time bounds, then the carve-out category array,
/// LIMIT last.
fn recent_recall_sql_for_agents(
    qualified_table: &str,
    qualified_agents: &str,
    since: bool,
    until: bool,
) -> String {
    let time_filter = recall_time_filter(since, until, 3);
    let categories_placeholder = 3 + usize::from(since) + usize::from(until);
    let limit_placeholder = categories_placeholder + 1;
    let session_filter = recall_session_scope_predicate(2, categories_placeholder);
    format!(
        "
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
        FROM {qualified_table} m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
        WHERE m.agent_id = ANY($1)
          AND {session_filter}
          {time_filter}
        ORDER BY m.updated_at DESC
        LIMIT ${limit_placeholder}
        "
    )
}

/// The ranked-recall candidate cap: the candidate stage never candidates
/// fewer than [`crate::RECALL_CANDIDATE_FLOOR`] rows (a tiny result-limit
/// ask still ranks over a useful window) nor more than
/// [`crate::POSTGRES_RECALL_CANDIDATE_CAP`] rows (ranked cost is
/// O(candidates) — measured 6,093 ms at 10,805 unbounded candidates on the
/// prod primary, 2026-11-18).
fn recall_candidate_cap(limit: usize) -> usize {
    limit
        .max(crate::RECALL_CANDIDATE_FLOOR)
        .min(crate::POSTGRES_RECALL_CANDIDATE_CAP)
}

/// Normalize a user query before it is bound into `plainto_tsquery`.
///
/// The `simple` text-search parser emits BOTH a compound lexeme and its parts
/// for a hyphenated run, so `plainto_tsquery('simple', 'bare-json')` parses to
/// `'bare-json' & 'bare' & 'json'`. A document whose key carries the term only
/// inside a LONGER hyphen chain (`deploy-executor-step-output-bare-json`)
/// produces that longer compound and never a standalone `'bare-json'`, so the
/// conjunction is unsatisfiable and the query returns nothing — silently
/// (prod evidence 2026-11-20: whole key families unfindable, which reads as
/// "too fresh" or "absent" to the caller).
///
/// Splitting on hyphens makes the query the conjunction of its words, which
/// matches the key and the body regardless of how the document is hyphenated.
fn normalize_tsquery_input(query: &str) -> String {
    query.replace('-', " ")
}

/// The ranked-recall shape shared by the unscoped and agent-scoped paths: a
/// `MATERIALIZED` candidate stage bounds the ranked cost to
/// O(candidate_cap) before the rank expression ever runs (measured
/// 2026-11-18 on the idle prod primary at 86,110 rows: 'circuit breaker' →
/// 10,805 candidates / 6,093 ms; 'drive_sync' → 6,418 candidates / 6,210 ms
/// — the per-candidate tsvector recheck makes ranked cost O(candidates)).
///
/// The stage carries every candidate-selection predicate — the full-content
/// FTS match (`to_tsvector('simple', m.key || ' ' || m.content)`, the
/// deep-match contract: a term match beyond the 4096-char rank prefix still
/// surfaces the row, ranking by its key arm instead), the session scoping
/// with the durable-global carve-out
/// ([`recall_session_scope_predicate`] — session-NULL `core`/`daily` rows
/// are recallable from any session), the agent-scoped allowlist when
/// present, and the enabled time bounds — ordered by recency
/// (`m.updated_at DESC`) and closed by the cap LIMIT. The rank expression,
/// the score ORDER BY and the result LIMIT follow the candidate join.
///
/// Bind layouts (the cap slot binds before the time bounds; the carve-out
/// category array binds after them, LIMIT stays the outer `$3`):
/// - unscoped: `$1` query / `$2` session / `$3` result limit / `$4` cap /
///   `$5..` time / categories last;
/// - agent-scoped: `$1` query / `$2` session / `$3` result limit / `$4`
///   allowlist / `$5` cap / `$6..` time / categories last.
///
/// `$1` is the query in BOTH the stage filter and the rank expression
/// ([`recall_rank_expression`] hardcodes it) — the layouts keep that
/// invariant. The outer query aliases the stage as `m` so the rank
/// expression's `m.` references hold.
fn ranked_recall_sql(
    qualified_table: &str,
    qualified_agents: &str,
    agent_scoped: bool,
    since: bool,
    until: bool,
    parse: RecallTsqueryParse,
) -> String {
    let cap_placeholder = if agent_scoped { 5 } else { 4 };
    let first_time_placeholder = cap_placeholder + 1;
    let time_filter = recall_time_filter(since, until, first_time_placeholder);
    let categories_placeholder = first_time_placeholder + usize::from(since) + usize::from(until);
    let session_filter = recall_session_scope_predicate(2, categories_placeholder);
    let allowlist_filter = if agent_scoped {
        "\n          AND m.agent_id = ANY($4)"
    } else {
        ""
    };
    format!(
        "
        WITH candidates AS MATERIALIZED (
            SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id,
                   m.agent_id, m.namespace, m.importance, m.updated_at
            FROM {qualified_table} m
            WHERE ($1 = '' OR to_tsvector('simple', m.key || ' ' || m.content) @@ {tsq}('simple', $1))
              AND {session_filter}{allowlist_filter}
              {time_filter}
            ORDER BY m.updated_at DESC
            LIMIT ${cap_placeholder}
        )
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8,
               ({rank})
               AS score
        FROM candidates m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
        ORDER BY score DESC, m.updated_at DESC
        LIMIT $3
        ",
        rank = recall_rank_expression(parse),
        tsq = parse.as_sql(),
    )
}

/// The prefix enumeration: index-assisted, LIMITed in storage.
///
/// `key LIKE $1 || '%'` is served by the boot-time
/// `idx_memories_key_btree` index (`text_pattern_ops` — the operator class
/// that makes btree serve `LIKE 'prefix%'` under the default C-less
/// collation), so the enumeration is an index range scan capped by LIMIT
/// instead of the no-LIMIT full-table scan `list` runs (measured 15.2s on
/// the prod leader's 50K-row store, canary v7). Placeholders: `$1` the
/// LIKE-escaped prefix, `$2` session_id, `$3` the LIMIT.
fn list_prefixed_sql(qualified_table: &str, qualified_agents: &str) -> String {
    format!(
        "
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
        FROM {qualified_table} m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
        WHERE m.key LIKE ($1 || '%')
          AND ($2::TEXT IS NULL OR m.session_id = $2)
        ORDER BY m.updated_at DESC
        LIMIT $3
        "
    )
}

/// The shared listing projection behind [`PostgresMemory::list`] and
/// [`PostgresMemory::list_limited`]: the full row projection including
/// `content` (the listing pane materializes previews from it), the agent
/// alias join, the `$1`/`$2` category/session invariants, most-recent-first
/// ordering. `bounded` appends `LIMIT $3` — the ONLY difference between
/// the two shapes; `false` is the unbounded export-contract read and must
/// carry no LIMIT at all.
fn memories_list_sql(qualified_table: &str, qualified_agents: &str, bounded: bool) -> String {
    let limit_clause = if bounded { " LIMIT $3" } else { "" };
    format!(
        "
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
        FROM {qualified_table} m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id
        WHERE ($1::TEXT IS NULL OR m.category = $1)
          AND ($2::TEXT IS NULL OR m.session_id = $2)
        ORDER BY m.updated_at DESC{limit_clause}
        "
    )
}

/// The backend-side listing cap: the RPC/gateway layers clamp before
/// calling, and this repeats the ceiling so a DIRECT caller of
/// `list_limited` cannot push the read past the listing max.
fn list_limited_cap(limit: usize) -> usize {
    limit.min(zeroclaw_api::memory_traits::MEMORY_LIST_MAX_LIMIT)
}

/// Parse an RFC 3339 export bound into a `TIMESTAMPTZ` instant. The bound
/// The filtered, windowed export projection behind
/// [`PostgresMemory::list_limited_filtered`]: the shared listing
/// projection with the export filter predicates pushed INTO the WHERE
/// clause and the paging window `LIMIT $n OFFSET $n` appended.
///
/// Every predicate present in the filter joins the WHERE with the next
/// sequential placeholder; predicates for ABSENT filter fields are OMITTED,
/// not `IS NULL`-guarded (the `recall_time_filter` precedent): each hot
/// shape plans against its own specialized statement, keeping the
/// `created_at` range and the `updated_at` index walk available to the
/// planner instead of forcing a generic NULL-guarded plan.
///
/// `since`/`until` bind as `::TIMESTAMPTZ` — the RFC 3339 string
/// comparison the export tool's old post-filter performed mis-ordered
/// mixed-offset spellings (`2026-01-01T01:00:00+02:00` sorts after
/// `2026-01-01T00:00:00Z` though it is an EARLIER instant); the timestamptz
/// bind compares instants. `ORDER BY m.updated_at DESC, m.id DESC` is TOTAL
/// (id is the primary key), so the offset windows are repeatable: paging
/// cannot skip or duplicate rows that share an `updated_at`.
fn memories_list_filtered_sql(
    qualified_table: &str,
    qualified_agents: &str,
    has_namespace: bool,
    has_category: bool,
    has_session: bool,
    has_since: bool,
    has_until: bool,
) -> String {
    let mut predicates: Vec<String> = Vec::new();
    let mut placeholder = 1usize;
    if has_namespace {
        predicates.push(format!("m.namespace = ${placeholder}"));
        placeholder += 1;
    }
    if has_category {
        predicates.push(format!("m.category = ${placeholder}"));
        placeholder += 1;
    }
    if has_session {
        predicates.push(format!("m.session_id = ${placeholder}"));
        placeholder += 1;
    }
    if has_since {
        predicates.push(format!("m.created_at >= ${placeholder}::TIMESTAMPTZ"));
        placeholder += 1;
    }
    if has_until {
        predicates.push(format!("m.created_at <= ${placeholder}::TIMESTAMPTZ"));
        placeholder += 1;
    }
    let limit_placeholder = placeholder;
    let offset_placeholder = placeholder + 1;
    let where_clause = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    format!(
        "
        SELECT m.id, m.key, m.content, m.category, m.created_at, m.session_id, a.alias AS agent_alias, m.agent_id, m.namespace, m.importance::FLOAT8
        FROM {qualified_table} m
        LEFT JOIN {qualified_agents} a ON a.id = m.agent_id{where_clause}
        ORDER BY m.updated_at DESC, m.id DESC
        LIMIT ${limit_placeholder} OFFSET ${offset_placeholder}
        "
    )
}

/// Escape a caller-supplied prefix for the LIKE predicate: `%` and `_` are
/// LIKE wildcards and `\` is the LIKE escape character, so the tool's
/// literal `starts_with` contract requires escaping all three before the
/// `'%'` suffix is appended.
fn escape_like_prefix(prefix: &str) -> String {
    prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Bound ONE ranked recall attempt: sets `statement_timeout` to the soft
/// recall budget, runs `run`, then restores the session default
/// (`op_timeout`). The whole SET → query → restore sequence runs inside ONE
/// EXCLUSIVE per-connection checkout — the pooled connection is held for
/// the entire sequence, so it cannot interleave with another operation (a
/// strictly stronger isolation than the former shared-mutex critical
/// section). Best-effort restore: a failed restore only leaves the
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
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let query = normalize_tsquery_input(normalize_recent_recall_query(query)).trim().to_string();
        let sid = session_id.map(str::to_string);
        // The time bounds parse to instants CLIENT-side and bind as
        // timestamptz: a text bind cannot serialize into the
        // server-inferred timestamptz parameter (postgres-types 0.2.14's
        // `&str::accepts` excludes TIMESTAMPTZ), and an unparseable bound
        // fails LOUDLY here instead of silently recalling unbounded
        // ([`parse_rfc3339_bound`]).
        let since = since.map(parse_rfc3339_bound).transpose()?;
        let until = until.map(parse_rfc3339_bound).transpose()?;

        // Empty-query fast path: the bare `*` / omitted-query contract is an
        // indexed recency read, not a full-text scan.
        if query.is_empty() {
            let categories: Vec<&'static str> = crate::durable_global_category_names().to_vec();
            return run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
                pool.with(|client| {
                    let since_ref = since.as_ref();
                    let until_ref = until.as_ref();
                    let stmt = recent_recall_sql(
                        &qualified_table,
                        &qualified_agents,
                        since_ref.is_some(),
                        until_ref.is_some(),
                    );

                    #[allow(clippy::cast_possible_wrap)]
                    let limit_i64 = limit as i64;

                    let rows = match (since_ref, until_ref) {
                        (Some(s), Some(u)) => {
                            client.query(&stmt, &[&sid, &s, &u, &categories, &limit_i64])?
                        }
                        (Some(s), None) => {
                            client.query(&stmt, &[&sid, &s, &categories, &limit_i64])?
                        }
                        (None, Some(u)) => {
                            client.query(&stmt, &[&sid, &u, &categories, &limit_i64])?
                        }
                        (None, None) => client.query(&stmt, &[&sid, &categories, &limit_i64])?,
                    };
                    rows.iter()
                        .map(Self::row_to_entry)
                        .collect::<Result<Vec<MemoryEntry>>>()
                })
            })
            .await;
        }

        let pool = self.pool.get().clone();
        let attempt_timeout = self.recall_soft_timeout;
        let op_timeout = self.op_timeout;
        let cap = recall_candidate_cap(limit);
        let categories: Vec<&'static str> = crate::durable_global_category_names().to_vec();

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            pool.with(|client| {
                let since_ref = since.as_ref();
                let until_ref = until.as_ref();

                let stmt = ranked_recall_sql(
                    &qualified_table,
                    &qualified_agents,
                    false,
                    since_ref.is_some(),
                    until_ref.is_some(),
                    RecallTsqueryParse::Strict,
                );

                #[allow(clippy::cast_possible_wrap)]
                let limit_i64 = limit as i64;
                #[allow(clippy::cast_possible_wrap)]
                let cap_i64 = cap as i64;

                // Bound THIS attempt server-side at the soft budget so a caller
                // that gives up leaves no orphan holding the pooled connection.
                with_statement_timeout(client, attempt_timeout, op_timeout, |client| {
                    let rows = match (since_ref, until_ref) {
                        (Some(s), Some(u)) => client.query(
                            &stmt,
                            &[&query, &sid, &limit_i64, &cap_i64, &s, &u, &categories],
                        )?,
                        (Some(s), None) => client.query(
                            &stmt,
                            &[&query, &sid, &limit_i64, &cap_i64, &s, &categories],
                        )?,
                        (None, Some(u)) => client.query(
                            &stmt,
                            &[&query, &sid, &limit_i64, &cap_i64, &u, &categories],
                        )?,
                        (None, None) => client
                            .query(&stmt, &[&query, &sid, &limit_i64, &cap_i64, &categories])?,
                    };
                    let entries = rows
                        .iter()
                        .map(Self::row_to_entry)
                        .collect::<Result<Vec<MemoryEntry>>>()?;
                    if !entries.is_empty() {
                        return Ok(entries);
                    }
                    // OR relaxation: a multi-token query whose conjunction
                    // matched nothing is retried as a disjunction, so one
                    // absent token no longer collapses the whole search to a
                    // silent empty (which callers read as "no such memory").
                    // A single-token query has nothing to relax and returns
                    // the empty result unchanged.
                    let Some(or_query) = tsquery_or_fallback_input(&query) else {
                        return Ok(entries);
                    };
                    let relaxed_stmt = ranked_recall_sql(
                        &qualified_table,
                        &qualified_agents,
                        false,
                        since_ref.is_some(),
                        until_ref.is_some(),
                        RecallTsqueryParse::Relaxed,
                    );
                    let relaxed_rows = match (since_ref, until_ref) {
                        (Some(s), Some(u)) => client.query(
                            &relaxed_stmt,
                            &[&or_query, &sid, &limit_i64, &cap_i64, &s, &u, &categories],
                        )?,
                        (Some(s), None) => client.query(
                            &relaxed_stmt,
                            &[&or_query, &sid, &limit_i64, &cap_i64, &s, &categories],
                        )?,
                        (None, Some(u)) => client.query(
                            &relaxed_stmt,
                            &[&or_query, &sid, &limit_i64, &cap_i64, &u, &categories],
                        )?,
                        (None, None) => client.query(
                            &relaxed_stmt,
                            &[&or_query, &sid, &limit_i64, &cap_i64, &categories],
                        )?,
                    };
                    relaxed_rows
                        .iter()
                        .map(Self::row_to_entry)
                        .collect::<Result<Vec<MemoryEntry>>>()
                })
            })
        })
        .await
    }

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let key = key.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<Option<MemoryEntry>> {
            pool.with(|client| {
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
        })
        .await
    }

    async fn get_for_agent(&self, key: &str, agent_id: &str) -> Result<Option<MemoryEntry>> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let key = key.to_string();
        let agent_id = agent_id.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<Option<MemoryEntry>> {
            pool.with(|client| {
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
        })
        .await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let category = category.map(Self::category_to_str);
        let sid = session_id.map(str::to_string);

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            pool.with(|client| {
                // UNBOUNDED by contract: the GDPR export rides this read; the
                // bounded listing is `list_limited`. The shared projection with
                // `bounded = false` carries no LIMIT clause at all.
                let stmt = memories_list_sql(&qualified_table, &qualified_agents, false);

                let category_ref = category.as_deref();
                let session_ref = sid.as_deref();
                let rows = client.query(&stmt, &[&category_ref, &session_ref])?;
                rows.iter()
                    .map(Self::row_to_entry)
                    .collect::<Result<Vec<MemoryEntry>>>()
            })
        })
        .await
    }

    async fn list_limited(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        // The backend repeats the RPC/gateway-layer clamp: a direct caller
        // cannot push the listing read past the listing max.
        let limit = list_limited_cap(limit);
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let category = category.map(Self::category_to_str);
        let sid = session_id.map(str::to_string);
        #[allow(clippy::cast_possible_wrap)]
        let limit_i64 = limit as i64;

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            pool.with(|client| {
                // The bound is IN STORAGE: `LIMIT $3` rides the
                // idx_memories_updated_at backward scan, so a large store never
                // materializes the unbounded projection to truncate it.
                let stmt = memories_list_sql(&qualified_table, &qualified_agents, true);

                let category_ref = category.as_deref();
                let session_ref = sid.as_deref();
                let rows = client.query(&stmt, &[&category_ref, &session_ref, &limit_i64])?;
                rows.iter()
                    .map(Self::row_to_entry)
                    .collect::<Result<Vec<MemoryEntry>>>()
            })
        })
        .await
    }

    async fn list_limited_filtered(
        &self,
        filter: &ExportFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let namespace = filter.namespace.clone();
        let category = filter.category.as_ref().map(Self::category_to_str);
        let session_id = filter.session_id.clone();
        // The time bounds parse to instants CLIENT-side and bind as
        // timestamptz: a text bind cannot serialize into the
        // server-inferred timestamptz parameter (and an unparseable bound
        // fails loudly instead of filtering everything out).
        let since = filter
            .since
            .as_deref()
            .map(parse_rfc3339_bound)
            .transpose()?;
        let until = filter
            .until
            .as_deref()
            .map(parse_rfc3339_bound)
            .transpose()?;

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            pool.with(|client| {
                let namespace_ref = namespace.as_deref();
                let category_ref = category.as_deref();
                let session_ref = session_id.as_deref();

                // Every export predicate rides storage: the filter fields bind
                // WHERE-side (the time bounds as TIMESTAMPTZ instants), the
                // paging window binds LIMIT/OFFSET — a page never materializes
                // the whole store.
                let stmt = memories_list_filtered_sql(
                    &qualified_table,
                    &qualified_agents,
                    namespace_ref.is_some(),
                    category_ref.is_some(),
                    session_ref.is_some(),
                    since.is_some(),
                    until.is_some(),
                );

                #[allow(clippy::cast_possible_wrap)]
                let limit_i64 = limit as i64;
                #[allow(clippy::cast_possible_wrap)]
                let offset_i64 = offset as i64;

                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = Vec::new();
                if namespace_ref.is_some() {
                    params.push(&namespace_ref);
                }
                if category_ref.is_some() {
                    params.push(&category_ref);
                }
                if session_ref.is_some() {
                    params.push(&session_ref);
                }
                if since.is_some() {
                    params.push(&since);
                }
                if until.is_some() {
                    params.push(&until);
                }
                params.push(&limit_i64);
                params.push(&offset_i64);

                let rows = client.query(&stmt, &params)?;
                rows.iter()
                    .map(Self::row_to_entry)
                    .collect::<Result<Vec<MemoryEntry>>>()
            })
        })
        .await
    }

    async fn list_prefixed(
        &self,
        prefix: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        if prefix.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let prefix = escape_like_prefix(prefix);
        let sid = session_id.map(str::to_string);
        let attempt_timeout = self.recall_soft_timeout;
        let op_timeout = self.op_timeout;

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            pool.with(|client| {
                let stmt = list_prefixed_sql(&qualified_table, &qualified_agents);

                #[allow(clippy::cast_possible_wrap)]
                let limit_i64 = limit as i64;
                let session_ref = sid.as_deref();

                // Bound THIS attempt server-side at the soft recall budget — the
                // same per-attempt cancel shape the ranked recall gets — so an
                // over-budget enumeration is cancelled by the server and frees
                // the pooled connection instead of holding it until `op_timeout`.
                with_statement_timeout(client, attempt_timeout, op_timeout, |client| {
                    let rows = client.query(&stmt, &[&prefix, &session_ref, &limit_i64])?;
                    rows.iter()
                        .map(Self::row_to_entry)
                        .collect::<Result<Vec<MemoryEntry>>>()
                })
            })
        })
        .await
    }

    async fn forget(&self, key: &str) -> Result<bool> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<bool> {
            pool.with(|client| {
                let stmt = format!("DELETE FROM {qualified_table} WHERE key = $1");
                let deleted = client.execute(&stmt, &[&key])?;
                Ok(deleted > 0)
            })
        })
        .await
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> Result<bool> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();
        let agent_id = agent_id.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<bool> {
            pool.with(|client| {
                let stmt =
                    format!("DELETE FROM {qualified_table} WHERE key = $1 AND agent_id = $2");
                let deleted = client.execute(&stmt, &[&key, &agent_id])?;
                Ok(deleted > 0)
            })
        })
        .await
    }

    async fn purge_session_for_agent(&self, session_id: &str, agent_id: &str) -> Result<usize> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let session_id = session_id.to_string();
        let agent_id = agent_id.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            pool.with(|client| {
                let stmt = format!(
                    "DELETE FROM {qualified_table} WHERE session_id = $1 AND agent_id = $2"
                );
                let deleted = client.execute(&stmt, &[&session_id, &agent_id])?;
                usize::try_from(deleted).context("PostgreSQL returned an oversized delete count")
            })
        })
        .await
    }

    async fn purge_agent(&self, agent_alias: &str) -> Result<usize> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = agent_alias.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            pool.with(|client| {
                let stmt = format!(
                    "DELETE FROM {qualified_table} WHERE agent_id = (SELECT id FROM {qualified_agents} WHERE alias = $1)"
                );
                let deleted = client.execute(&stmt, &[&alias])?;
                usize::try_from(deleted).context("PostgreSQL returned an oversized delete count")
            })
        })
        .await
    }

    async fn export_agent(&self, agent_alias: &str) -> Result<Vec<MemoryEntry>> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = agent_alias.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            pool.with(|client| {
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
        })
        .await
    }

    async fn rename_agent(&self, from: &str, to: &str) -> Result<usize> {
        let pool = self.pool.get().clone();
        let qualified_agents = self.qualified_agents.clone();
        let qualified_table = self.qualified_table.clone();
        let from = from.to_string();
        let to = to.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            pool.with(|client| {
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
        })
        .await
    }

    async fn count_agent(&self, agent_alias: &str) -> Result<usize> {
        let pool = self.pool.get().clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = agent_alias.to_string();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            pool.with(|client| {
                // Mirror `rename_agent`: it moves the `agents` row (alias -> id), so
                // residue is the presence of that alias row (0 or 1), NOT the memory-
                // row count (which would miss an agents-row-only lag).
                let stmt = format!("SELECT COUNT(*) FROM {qualified_agents} WHERE alias = $1");
                let row = client.query_one(&stmt, &[&alias])?;
                let count: i64 = row.get(0);
                usize::try_from(count).context("PostgreSQL returned an oversized agent count")
            })
        })
        .await
    }

    async fn count(&self) -> Result<usize> {
        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();

        run_on_os_thread(self.op_timeout, move || -> Result<usize> {
            pool.with(|client| {
                let stmt = format!("SELECT COUNT(*) FROM {qualified_table}");
                let count: i64 = client.query_one(&stmt, &[])?.get(0);
                let count = usize::try_from(count)
                    .context("PostgreSQL returned a negative memory count")?;
                Ok(count)
            })
        })
        .await
    }

    async fn health_check(&self) -> bool {
        let pool = self.pool.get().clone();
        run_on_os_thread(self.op_timeout, move || {
            // The error PROPAGATES out of the closure so the pool classifies
            // it: a dead connection is discarded, ONE reconnect is attempted,
            // and the probe runs once on the fresh connection. DECIDED
            // semantics — the healing pool reports TRUE (heal in place): a
            // false-while-healing would turn every patroni restart into a
            // readiness CrashLoop, recreating the manual-roll regime. False
            // only when the reconnect also fails (or the probe thread times
            // out). The observable claim is the NEXT MEMORY OP succeeds, not
            // the first webhook turn.
            pool.with(|client| {
                client
                    .simple_query("SELECT 1")
                    .map(|_| true)
                    .map_err(Into::into)
            })
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
        let pool = self.pool.get().clone();
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
            pool.with(|client| {
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

        let pool = self.pool.get().clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_agents = self.qualified_agents.clone();
        let q = normalize_tsquery_input(normalize_recent_recall_query(query)).trim().to_string();
        let sid = session_id.map(str::to_string);
        // The time bounds parse to instants CLIENT-side and bind as
        // timestamptz: a text bind cannot serialize into the
        // server-inferred timestamptz parameter (postgres-types 0.2.14's
        // `&str::accepts` excludes TIMESTAMPTZ), and an unparseable bound
        // fails LOUDLY here instead of silently recalling unbounded
        // ([`parse_rfc3339_bound`]). Below the empty-allowlist early
        // return, which delegates with the unparsed bounds for `recall` to
        // parse at its own seam.
        let since = since.map(parse_rfc3339_bound).transpose()?;
        let until = until.map(parse_rfc3339_bound).transpose()?;
        let allowed: Vec<String> = allowed_agent_ids.iter().map(|s| (*s).to_string()).collect();

        // Empty-query fast path: indexed recency within the agent allowlist.
        // Carries the caller's session + the durable-global carve-out — the
        // budget-fallback bleed fix (the pre-fix shape dropped the session
        // filter entirely on this path).
        if q.is_empty() {
            let categories: Vec<&'static str> = crate::durable_global_category_names().to_vec();
            return run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
                pool.with(|client| {
                    let since_ref = since.as_ref();
                    let until_ref = until.as_ref();
                    let stmt = recent_recall_sql_for_agents(
                        &qualified_table,
                        &qualified_agents,
                        since_ref.is_some(),
                        until_ref.is_some(),
                    );

                    #[allow(clippy::cast_possible_wrap)]
                    let limit_i64 = limit as i64;

                    let rows =
                        match (since_ref, until_ref) {
                            (Some(s), Some(u)) => client
                                .query(&stmt, &[&allowed, &sid, &s, &u, &categories, &limit_i64])?,
                            (Some(s), None) => client
                                .query(&stmt, &[&allowed, &sid, &s, &categories, &limit_i64])?,
                            (None, Some(u)) => client
                                .query(&stmt, &[&allowed, &sid, &u, &categories, &limit_i64])?,
                            (None, None) => {
                                client.query(&stmt, &[&allowed, &sid, &categories, &limit_i64])?
                            }
                        };
                    rows.iter()
                        .map(Self::row_to_entry)
                        .collect::<Result<Vec<MemoryEntry>>>()
                })
            })
            .await;
        }

        let pool = self.pool.get().clone();
        let attempt_timeout = self.recall_soft_timeout;
        let op_timeout = self.op_timeout;
        let cap = recall_candidate_cap(limit);
        let categories: Vec<&'static str> = crate::durable_global_category_names().to_vec();

        run_on_os_thread(self.op_timeout, move || -> Result<Vec<MemoryEntry>> {
            pool.with(|client| {
                let since_ref = since.as_ref();
                let until_ref = until.as_ref();

                let stmt = ranked_recall_sql(
                    &qualified_table,
                    &qualified_agents,
                    true,
                    since_ref.is_some(),
                    until_ref.is_some(),
                    RecallTsqueryParse::Strict,
                );

                #[allow(clippy::cast_possible_wrap)]
                let limit_i64 = limit as i64;
                #[allow(clippy::cast_possible_wrap)]
                let cap_i64 = cap as i64;

                // Bound THIS attempt server-side at the soft budget so a caller
                // that gives up leaves no orphan holding the pooled connection.
                with_statement_timeout(client, attempt_timeout, op_timeout, |client| {
                    let rows = match (since_ref, until_ref) {
                        (Some(s), Some(u)) => client.query(
                            &stmt,
                            &[
                                &q,
                                &sid,
                                &limit_i64,
                                &allowed,
                                &cap_i64,
                                &s,
                                &u,
                                &categories,
                            ],
                        )?,
                        (Some(s), None) => client.query(
                            &stmt,
                            &[&q, &sid, &limit_i64, &allowed, &cap_i64, &s, &categories],
                        )?,
                        (None, Some(u)) => client.query(
                            &stmt,
                            &[&q, &sid, &limit_i64, &allowed, &cap_i64, &u, &categories],
                        )?,
                        (None, None) => client.query(
                            &stmt,
                            &[&q, &sid, &limit_i64, &allowed, &cap_i64, &categories],
                        )?,
                    };
                    let entries = rows
                        .iter()
                        .map(Self::row_to_entry)
                        .collect::<Result<Vec<MemoryEntry>>>()?;
                    if !entries.is_empty() {
                        return Ok(entries);
                    }
                    // OR relaxation on the AGENT-SCOPED path too — this is the
                    // path an agent's own recall takes, so wiring only the
                    // unscoped shape leaves the fallback inert exactly where
                    // agents exhaust their reads (2026-11-20 live finding: the
                    // unscoped shape relaxed, agent-scoped did not).
                    let Some(or_query) = tsquery_or_fallback_input(&q) else {
                        return Ok(entries);
                    };
                    let relaxed_stmt = ranked_recall_sql(
                        &qualified_table,
                        &qualified_agents,
                        true,
                        since_ref.is_some(),
                        until_ref.is_some(),
                        RecallTsqueryParse::Relaxed,
                    );
                    let relaxed_rows = match (since_ref, until_ref) {
                        (Some(s), Some(u)) => client.query(
                            &relaxed_stmt,
                            &[
                                &or_query, &sid, &limit_i64, &allowed, &cap_i64, &s, &u, &categories,
                            ],
                        )?,
                        (Some(s), None) => client.query(
                            &relaxed_stmt,
                            &[&or_query, &sid, &limit_i64, &allowed, &cap_i64, &s, &categories],
                        )?,
                        (None, Some(u)) => client.query(
                            &relaxed_stmt,
                            &[&or_query, &sid, &limit_i64, &allowed, &cap_i64, &u, &categories],
                        )?,
                        (None, None) => client.query(
                            &relaxed_stmt,
                            &[&or_query, &sid, &limit_i64, &allowed, &cap_i64, &categories],
                        )?,
                    };
                    relaxed_rows
                        .iter()
                        .map(Self::row_to_entry)
                        .collect::<Result<Vec<MemoryEntry>>>()
                })
            })
        })
        .await
    }

    async fn ensure_agent_uuid(&self, alias: &str) -> Result<String> {
        let pool = self.pool.get().clone();
        let qualified_agents = self.qualified_agents.clone();
        let alias = alias.to_string();
        run_on_os_thread(self.op_timeout, move || -> Result<String> {
            pool.with(|client| {
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
        assert!(
            sql.contains("LEFT JOIN mem.agents"),
            "agent alias join kept"
        );
        assert!(
            sql.contains(
                "($1::TEXT IS NULL OR m.session_id = $1 OR (m.session_id IS NULL AND m.category = ANY($3)))",
            ),
            "the session predicate carries the durable-global carve-out (session-NULL core/daily admitted), got: {sql}"
        );
        assert!(sql.contains("m.created_at >= $2::TIMESTAMPTZ"));
        assert!(
            sql.contains("LIMIT $4"),
            "since bound → categories at $3, limit at $4, got: {sql}"
        );
    }

    #[test]
    fn recent_recall_sql_for_agents_filters_by_allowlist_and_is_vector_free() {
        let sql = recent_recall_sql_for_agents("mem.memories", "mem.agents", true, true);
        assert!(!sql.contains("to_tsvector"));
        assert!(sql.contains("m.agent_id = ANY($1)"));
        assert!(
            sql.contains("(m.session_id IS NULL AND m.category = ANY($5))"),
            "the agent-scoped recency path carries the durable-global carve-out with the categories array at $5, got: {sql}"
        );
        assert!(sql.contains("m.created_at >= $3::TIMESTAMPTZ"));
        assert!(sql.contains("m.created_at <= $4::TIMESTAMPTZ"));
        assert!(
            sql.contains("LIMIT $6"),
            "both time bounds → limit at $6, got: {sql}"
        );
    }

    /// The budget-fallback bleed pin (the fusion verdict 2026-11-18): the
    /// agent-scoped recency path is the recall budget's fallback shape —
    /// `BudgetedMemory::recall_for_agents` degrades to
    /// `recall_for_agents(.., "", ..)` carrying the caller's session — so it
    /// MUST carry the same session-scope predicate as every other recall
    /// path: the caller's session plus the durable-global carve-out
    /// (session-NULL `core`/`daily`). The pre-fix shape dropped the session
    /// filter entirely — a session-scoped recall that tripped the budget
    /// fell back to ALL sessions' rows (the bleed).
    #[test]
    fn recent_recall_for_agents_carries_the_durable_global_predicate() {
        let sql = recent_recall_sql_for_agents("mem.memories", "mem.agents", true, true);
        assert!(
            sql.contains("m.agent_id = ANY($1)"),
            "the allowlist stays at $1, got: {sql}"
        );
        assert!(
            sql.contains("m.session_id = $2"),
            "the budget fallback must keep the caller's session scope, got: {sql}"
        );
        assert!(
            sql.contains("(m.session_id IS NULL AND m.category = ANY("),
            "the durable-global carve-out must ride the agent-scoped recency path, got: {sql}"
        );
        assert!(
            sql.contains("m.created_at >= $3::TIMESTAMPTZ")
                && sql.contains("m.created_at <= $4::TIMESTAMPTZ"),
            "the session bind shifts the time bounds to $3/$4, got: {sql}"
        );
        assert!(
            sql.contains("LIMIT $6"),
            "allowlist + session + both bounds → limit at $6, got: {sql}"
        );
    }

    // Tuning cycle 5: the prefix enumeration must be an index-assisted,
    // LIMITed query — the no-LIMIT full-table scan it used to ride through
    // `list` measured 15.2s on the prod leader (canary v7).
    #[test]
    fn list_prefixed_sql_is_prefix_pushed_and_limited() {
        let sql = list_prefixed_sql("mem.memories", "mem.agents");
        assert!(
            sql.contains("m.key LIKE ($1 || '%')"),
            "the prefix predicate must be pushed into storage, got: {sql}"
        );
        assert!(
            sql.contains("ORDER BY m.updated_at DESC"),
            "most-recent-first ordering matches the enumeration contract"
        );
        assert!(
            sql.contains("LIMIT $3"),
            "the LIMIT must be a bound parameter pushed into storage, got: {sql}"
        );
        assert!(
            sql.contains("$2::TEXT IS NULL OR m.session_id = $2"),
            "session scoping mirrors list, got: {sql}"
        );
        assert!(
            sql.contains("LEFT JOIN mem.agents"),
            "agent alias join kept"
        );
    }

    // V3 LIST-BOUND: the listing surface gets a bounded read while the
    // plain `list` read stays unbounded (the GDPR export contract rides it).

    /// THE UNBOUNDED-EXPORT PIN — the regression guard this program exists
    /// to protect: the plain `list` read carries NO LIMIT. The listing bound
    /// (`list_limited`) must never leak into it, and the extraction of the
    /// shared listing projection must not disturb the export path's shape.
    #[test]
    fn memory_list_trait_list_stays_unbounded() {
        let sql = memories_list_sql("mem.memories", "mem.agents", false);
        assert!(
            !sql.contains("LIMIT"),
            "Memory::list stays unbounded — the export contract rides it, got: {sql}"
        );
        assert!(
            sql.contains("ORDER BY m.updated_at DESC"),
            "recency ordering rides the updated_at index"
        );
        assert!(
            sql.contains("LEFT JOIN mem.agents"),
            "agent alias join kept"
        );
        assert!(
            sql.contains("($1::TEXT IS NULL OR m.category = $1)"),
            "the category placeholder invariant matches list, got: {sql}"
        );
        assert!(
            sql.contains("($2::TEXT IS NULL OR m.session_id = $2)"),
            "the session placeholder invariant matches list, got: {sql}"
        );
    }

    /// The listing read pushes the bound into storage: `LIMIT $3` binds the
    /// clamped cap (the backend repeats the RPC-layer clamp so a direct
    /// caller cannot exceed the listing max), every other shape identical
    /// to `list`.
    #[test]
    fn list_limited_sql_pushes_the_limit_into_storage() {
        assert_eq!(
            list_limited_cap(50),
            50,
            "a limit within the max passes through"
        );
        assert_eq!(
            list_limited_cap(100_000),
            zeroclaw_api::memory_traits::MEMORY_LIST_MAX_LIMIT,
            "a direct caller cannot exceed the listing max"
        );
        let sql = memories_list_sql("mem.memories", "mem.agents", true);
        assert!(
            sql.contains("LIMIT $3"),
            "the LIMIT binds the clamped cap as $3, got: {sql}"
        );
        assert!(
            sql.contains("ORDER BY m.updated_at DESC"),
            "recency ordering rides the updated_at index"
        );
        assert!(
            sql.contains("LEFT JOIN mem.agents"),
            "agent alias join kept"
        );
        assert!(
            sql.contains("($1::TEXT IS NULL OR m.category = $1)"),
            "the category placeholder invariant matches list, got: {sql}"
        );
        assert!(
            sql.contains("($2::TEXT IS NULL OR m.session_id = $2)"),
            "the session placeholder invariant matches list, got: {sql}"
        );
    }

    /// Live: the listing read respects the LIMIT in storage (newest-first)
    /// and the plan serves it from `idx_memories_updated_at` (the backward
    /// scan) — the same live-test shape as the prefix-bound pins.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_list_limited_respects_the_limit_and_serves_the_updated_at_index() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "listbound");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        // Constructs the table AND the recall indexes (boot DDL), including
        // idx_memories_updated_at. Production shape: the pgvector column
        // setup adds the namespace/importance columns the SELECT reads.
        runtime.block_on(async {
            PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();
        });
        drop(runtime);

        let qualified_table = format!(
            "{}.{}",
            quote_identifier(&schema.name),
            quote_identifier("memories")
        );
        let qualified_agents = format!(
            "{}.{}",
            quote_identifier(&schema.name),
            quote_identifier("agents")
        );
        // 25 target rows staggered one second apart (g=1 is newest) plus
        // 20K noise rows all older than every target: the newest-10 window
        // is exactly the targets, and the planner cannot serve LIMIT 10
        // from a seq scan cheaper than the updated_at index walk.
        schema
            .admin
            .batch_execute(&format!(
                "INSERT INTO {qualified_table} (id, key, content, category, created_at, updated_at, agent_id)
                 SELECT 'tgt_' || g, 'listbound:' || g, 'target ' || g, 'daily',
                        NOW() - make_interval(secs => g), NOW() - make_interval(secs => g),
                        (SELECT id FROM {qualified_agents} WHERE alias = 'default' LIMIT 1)
                 FROM generate_series(1, 25) g;
                 INSERT INTO {qualified_table} (id, key, content, category, created_at, updated_at, agent_id)
                 SELECT 'noise_' || g, 'noise:' || g, repeat(md5(g::text), 16), 'daily',
                        NOW() - make_interval(secs => 100000 + g), NOW() - make_interval(secs => 100000 + g),
                        (SELECT id FROM {qualified_agents} WHERE alias = 'default' LIMIT 1)
                 FROM generate_series(1, 20000) g;
                 ANALYZE {qualified_table};",
            ))
            .expect("seeding the listing-bound fixture must succeed");

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
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            let page = memory
                .list_limited(None, None, 10)
                .await
                .expect("the bounded listing read");
            assert_eq!(
                page.len(),
                10,
                "the LIMIT must be respected in storage, not truncated client-side"
            );
            let keys: Vec<String> = page.iter().map(|e| e.key.clone()).collect();
            let expected: Vec<String> = (1..=10).map(|g| format!("listbound:{g}")).collect();
            assert_eq!(
                keys, expected,
                "newest-first: the newest `limit` rows in updated_at DESC order"
            );
        });
        drop(runtime);

        let sql = memories_list_sql(&qualified_table, &qualified_agents, true);
        let explain = schema
            .admin
            .query(
                &format!("EXPLAIN {sql}"),
                &[&None::<String>, &None::<String>, &10i64],
            )
            .expect("EXPLAIN on the listing query must succeed");
        let plan: String = explain
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            plan.contains("idx_memories_updated_at"),
            "the listing read must be served by the updated_at index, got plan:\n{plan}"
        );
    }

    /// The filtered export window's SQL carries every export predicate in
    /// storage: the namespace/category/session equality guards, the
    /// since/until bounds bound as `::TIMESTAMPTZ` (never the RFC 3339
    /// string comparison the old export post-filter performed), and the
    /// `LIMIT/OFFSET` paging window. Predicates for absent filter fields
    /// are OMITTED (the `recall_time_filter` precedent) so each hot shape
    /// plans against its own specialized statement instead of a
    /// NULL-guarded generic plan.
    #[test]
    fn list_limited_filtered_sql_carries_the_predicates_and_limit() {
        let sql = memories_list_filtered_sql("m", "a", true, true, true, true, true);
        assert!(
            sql.contains("m.namespace = $1"),
            "namespace rides storage: {sql}"
        );
        assert!(sql.contains("m.category = $2"));
        assert!(sql.contains("m.session_id = $3"));
        assert!(
            sql.contains("m.created_at >= $4::TIMESTAMPTZ"),
            "since binds as timestamptz, not a string comparison: {sql}"
        );
        assert!(
            sql.contains("m.created_at <= $5::TIMESTAMPTZ"),
            "until binds as timestamptz, not a string comparison: {sql}"
        );
        assert!(
            sql.contains("LIMIT $6 OFFSET $7"),
            "the paging window is in storage: {sql}"
        );
        assert!(
            sql.contains("ORDER BY m.updated_at DESC, m.id DESC"),
            "the order is total (id tie-break) so offset windows cannot skip or duplicate rows: {sql}"
        );

        // Absent filter fields are omitted, not NULL-guarded: the bare
        // shape has no WHERE clause at all and the window placeholders
        // renumber to the front.
        let bare = memories_list_filtered_sql("m", "a", false, false, false, false, false);
        assert!(!bare.contains(" WHERE "), "no filter, no WHERE: {bare}");
        assert!(bare.contains("LIMIT $1 OFFSET $2"));

        // The purge SOP's shape — since+until only — keeps the timestamptz
        // binds at the leading placeholders with the window behind them.
        let window = memories_list_filtered_sql("m", "a", false, false, false, true, true);
        assert!(window.contains("m.created_at >= $1::TIMESTAMPTZ"));
        assert!(window.contains("m.created_at <= $2::TIMESTAMPTZ"));
        assert!(window.contains("LIMIT $3 OFFSET $4"));
    }

    /// G3.3 — the full-set contract: the paged, predicate-pushed export's
    /// key set is BYTE-IDENTICAL to the unbounded `list`'s key set filtered
    /// the same way. Seed 2500 in-window rows plus 300 deep-history rows
    /// (~400 days old, outside the window): the export pages the 2500 as
    /// 2000 + 500 + the empty terminator page, and no deep row leaks in.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_export_paged_key_set_byte_identical_to_unbounded_list() {
        use zeroclaw_api::memory_traits::{ExportFilter, MEMORY_LIST_MAX_LIMIT};

        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "expbound");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();
        });
        drop(runtime);

        let qualified_table = format!(
            "{}.{}",
            quote_identifier(&schema.name),
            quote_identifier("memories")
        );
        let qualified_agents = format!(
            "{}.{}",
            quote_identifier(&schema.name),
            quote_identifier("agents")
        );
        schema
            .admin
            .batch_execute(&format!(
                "INSERT INTO {qualified_table} (id, key, content, category, created_at, updated_at, agent_id)
                 SELECT 'recent_' || g, 'expbound:recent:' || g, 'recent ' || g, 'daily',
                        NOW() - make_interval(secs => g), NOW() - make_interval(secs => g),
                        (SELECT id FROM {qualified_agents} WHERE alias = 'default' LIMIT 1)
                 FROM generate_series(1, 2500) g;
                 INSERT INTO {qualified_table} (id, key, content, category, created_at, updated_at, agent_id)
                 SELECT 'deep_' || g, 'expbound:deep:' || g, 'deep ' || g, 'daily',
                        NOW() - INTERVAL '400 days' - make_interval(secs => g),
                        NOW() - INTERVAL '400 days' - make_interval(secs => g),
                        (SELECT id FROM {qualified_agents} WHERE alias = 'default' LIMIT 1)
                 FROM generate_series(1, 300) g;
                 ANALYZE {qualified_table};",
            ))
            .expect("seeding the export-paging fixture must succeed");

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
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            // The purge SOP's shape: a since+until window. The tool's paging
            // loop, exercised against the real backend.
            let filter = ExportFilter {
                since: Some("2026-01-01T00:00:00Z".into()),
                until: Some("2027-01-01T00:00:00Z".into()),
                ..ExportFilter::default()
            };
            let mut paged_keys: Vec<String> = Vec::new();
            let mut pages = 0usize;
            let mut offset = 0usize;
            loop {
                let page = memory
                    .list_limited_filtered(&filter, offset, MEMORY_LIST_MAX_LIMIT)
                    .await
                    .expect("the filtered export page");
                pages += 1;
                let empty = page.is_empty();
                paged_keys.extend(page.iter().map(|e| e.key.clone()));
                if empty {
                    break;
                }
                offset += MEMORY_LIST_MAX_LIMIT;
            }

            assert_eq!(
                pages, 3,
                "2500 in-window rows page as 2000 + 500 + the empty terminator page"
            );
            assert_eq!(
                paged_keys.len(),
                2500,
                "no duplicates across page boundaries: every row exactly once"
            );

            // The G3.3 oracle: the UNBOUNDED list, filtered the same way —
            // the old export's semantics (lexicographic on the entry
            // timestamps; row_to_entry emits uniform UTC +00:00 spellings
            // and no seeded row sits near a boundary instant, so the string
            // oracle and the timestamptz binds partition identically).
            let all = memory.list(None, None).await.unwrap();
            let mut expected: Vec<String> = all
                .iter()
                .filter(|e| {
                    let t = e.timestamp.as_str();
                    t >= "2026-01-01T00:00:00Z" && t <= "2027-01-01T00:00:00Z"
                })
                .map(|e| e.key.clone())
                .collect();
            assert_eq!(
                expected.len(),
                2500,
                "the oracle's own window must select exactly the 2500 recent rows"
            );
            expected.sort();
            let mut paged_sorted = paged_keys;
            paged_sorted.sort();
            assert_eq!(
                paged_sorted, expected,
                "G3.3: the paged filtered export's key set is byte-identical to \
                 the unbounded list's key set filtered the same way"
            );
        });
        drop(runtime);

        let _ = &mut schema; // keep the Drop guard alive until the assertions ran
    }

    #[test]
    fn escape_like_prefix_neutralizes_like_wildcards() {
        assert_eq!(escape_like_prefix("ws:"), "ws:");
        assert_eq!(escape_like_prefix("100%"), "100\\%");
        assert_eq!(escape_like_prefix("under_score"), "under\\_score");
        assert_eq!(escape_like_prefix(r"back\slash"), r"back\\slash");
        // Order matters: the escape character is escaped FIRST so the
        // wildcard escapes it inserts are not themselves re-escaped.
        assert_eq!(escape_like_prefix(r"\%"), r"\\\%");
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
    fn recall_rank_expression_bounds_content_and_keeps_key_arm_whole() {
        let sql = recall_rank_expression(RecallTsqueryParse::Strict);

        // The content arm must rank over a BOUNDED prefix: the unbounded
        // `to_tsvector('simple', m.content)` re-parse per candidate row was
        // measured at 8.07s per recall on the prod leader (2026-10-28) and
        // burned the 2s recall soft budget on every unscoped recall.
        assert!(
            sql.contains("left(m.content, 4096)"),
            "content rank arm must be bounded to the prefix const, got: {sql}"
        );
        assert!(
            !sql.contains("to_tsvector('simple', m.content)"),
            "no unbounded content tsvector may remain in the rank expression, got: {sql}"
        );
        // The key arm stays whole: keys are short and carry the canonical name.
        assert!(
            sql.contains("to_tsvector('simple', m.key) @@ plainto_tsquery('simple', $1)")
                && sql.contains(
                    "ts_rank_cd(to_tsvector('simple', m.key), plainto_tsquery('simple', $1), 32) * 2.0"
                ),
            "key rank arm must stay unbounded at 2.0 weight, got: {sql}"
        );
        // Both arms bind the same query placeholder.
        assert_eq!(sql.matches("plainto_tsquery('simple', $1)").count(), 4);
    }

    /// The 2026-11-18 `2.0ELSE` class (server-proven: every non-empty-query
    /// ranked recall died at parse with "trailing junk after numeric
    /// literal"): a `\`-newline continuation inside a Rust string strips the
    /// newline AND the next line's leading whitespace, fusing adjacent SQL
    /// tokens. This pin holds on the RAW built string — no join may fuse two
    /// tokens — and on the whitespace-normalized form, which must carry the
    /// intended separations regardless of how the whitespace is spelled.
    #[test]
    fn recall_rank_expression_joins_are_whitespace_separated() {
        let sql = recall_rank_expression(RecallTsqueryParse::Strict);
        let normalized = sql.split_whitespace().collect::<Vec<_>>().join(" ");

        // The spaced joins (whitespace-normalized form): the intended token
        // separations across the whole expression.
        assert!(
            normalized.contains("$1) THEN"),
            "CASE WHEN arms must separate the predicate from THEN, got: {sql:?}"
        );
        assert!(
            normalized.contains("* 2.0 ELSE"),
            "the key-arm weight must be separated from ELSE, got: {sql:?}"
        );
        assert!(
            normalized.contains("END + CASE"),
            "the two rank arms must separate around the +, got: {sql:?}"
        );
        assert!(
            normalized.contains("32) ELSE"),
            "the content arm's rank must be separated from its ELSE, got: {sql:?}"
        );

        // The stripped joins (the broken shape) must not exist in the raw
        // string — `2.0ELSE` is the parse-fatal one; the other two are the
        // same continuation-stripping class.
        assert!(
            !sql.contains("2.0ELSE"),
            "a fused 2.0ELSE token is parse-fatal (trailing junk after numeric literal), got: {sql:?}"
        );
        assert!(!sql.contains(")THEN"), "a fused )THEN join, got: {sql:?}");
        assert!(!sql.contains("+CASE"), "a fused +CASE join, got: {sql:?}");

        // Token-equivalence: the key arm whole at 2.0 weight (exactly one),
        // the content arm bounded by the 4096 prefix in BOTH its filter and
        // its rank arm at implicit 1.0 weight.
        assert_eq!(
            sql.matches("* 2.0").count(),
            1,
            "only the key arm carries the 2.0 weight, got: {sql:?}"
        );
        assert_eq!(
            sql.matches("left(m.content, 4096)").count(),
            2,
            "the 4096 prefix must bound both the content filter and its rank arm, got: {sql:?}"
        );
    }

    /// The 2026-11-20 score-display defect: raw `ts_rank_cd` lands ~0.01–0.1
    /// on prod-sized inputs and the tool renders `{:.0}%` of a 0..1 score, so
    /// every row displayed `[0%]` and ranking was invisible. Normalization
    /// flag 32 (`rank/(rank+1)`) lands each arm in 0..1; the key arm keeps
    /// its 2.0 boost applied AFTER normalization, and the final expression is
    /// bounded at 1.0 so the rendered percentage can never exceed 100%.
    #[test]
    fn recall_rank_expression_normalizes_scores_bounded_in_range() {
        let sql = recall_rank_expression(RecallTsqueryParse::Strict);

        assert!(
            sql.contains(
                "ts_rank_cd(to_tsvector('simple', m.key), plainto_tsquery('simple', $1), 32) * 2.0"
            ),
            "the key arm must carry normalization flag 32 before its 2.0 boost, got: {sql}"
        );
        assert!(
            sql.contains(
                "ts_rank_cd(to_tsvector('simple', left(m.content, 4096)), plainto_tsquery('simple', $1), 32)"
            ),
            "the content arm must carry normalization flag 32, got: {sql}"
        );
        assert!(
            sql.contains("LEAST(") && sql.contains(", 1.0)"),
            "the final score expression must be bounded at 1.0 so the rendered percentage stays in range, got: {sql}"
        );
    }

    /// The OR-relaxation shape rides BESIDE the strict shape: identical SQL
    /// except the parse function. `websearch_to_tsquery` honors the OR
    /// keyword and can never raise a syntax error on user input (malformed
    /// text is ignored — the reason the fallback binds a caller-built
    /// `token or token` string instead of building `to_tsquery` syntax), and
    /// `plainto_tsquery` cannot express OR at all.
    #[test]
    fn ranked_recall_sql_or_relaxation_shape_differs_only_in_the_parse_fn() {
        for agent_scoped in [false, true] {
            for (since, until) in [(false, false), (true, false), (false, true), (true, true)] {
                let strict = ranked_recall_sql(
                    "mem.memories",
                    "mem.agents",
                    agent_scoped,
                    since,
                    until,
                    RecallTsqueryParse::Strict,
                );
                let relaxed = ranked_recall_sql(
                    "mem.memories",
                    "mem.agents",
                    agent_scoped,
                    since,
                    until,
                    RecallTsqueryParse::Relaxed,
                );

                assert_eq!(
                    strict.matches("plainto_tsquery('simple', $1)").count(),
                    5,
                    "the strict shape binds plainto at the stage filter (1) and the rank arms (4), got: {strict}"
                );
                assert!(
                    !strict.contains("websearch_to_tsquery"),
                    "the strict shape must not carry the relaxed parse fn, got: {strict}"
                );
                assert_eq!(
                    relaxed.matches("websearch_to_tsquery('simple', $1)").count(),
                    5,
                    "the relaxed shape binds websearch everywhere the strict shape binds plainto, got: {relaxed}"
                );
                assert!(
                    !relaxed.contains("plainto_tsquery"),
                    "the relaxed shape must not carry the strict parse fn, got: {relaxed}"
                );
                assert_eq!(
                    strict.replace("plainto_tsquery", "websearch_to_tsquery"),
                    relaxed,
                    "the two shapes must differ ONLY in the parse function (same candidate stage, same rank expression, same filters); agent_scoped={agent_scoped} since={since} until={until}"
                );
            }
        }
    }

    /// The OR-fallback token builder: the normalized query's tokens (hyphens
    /// already split, empty/punctuation-only tokens dropped — they yield no
    /// lexeme) joined with websearch's OR keyword. The AND expectation is the
    /// SAME token sequence joined by single spaces (what the strict shape
    /// binds); the OR expectation joins them with ` or `. Fewer than two
    /// tokens means there is nothing to relax — no fallback input is built.
    #[test]
    fn or_fallback_input_builds_the_or_shape_from_the_normalized_query() {
        // AND expectation: the normalized query is the conjunction of its tokens.
        assert_eq!(normalize_tsquery_input("bare-json"), "bare json");
        assert_eq!(
            normalize_tsquery_input("pr drain state waiting_streak"),
            "pr drain state waiting_streak"
        );
        // OR expectation: the SAME tokens joined with websearch's OR keyword.
        assert_eq!(
            tsquery_or_fallback_input("bare json").as_deref(),
            Some("bare or json")
        );
        assert_eq!(
            tsquery_or_fallback_input("pr drain state waiting_streak").as_deref(),
            Some("pr or drain or state or waiting_streak")
        );
    }

    /// A single-token query has no fallback: a one-token OR query IS the same
    /// conjunction, so the builder returns None and the relaxation never
    /// fires (pinned at the builder level; the live-tier behavioral pin is
    /// `postgres_recall_hyphen_key_and_or_relaxation`).
    #[test]
    fn or_fallback_input_none_for_single_token() {
        assert_eq!(tsquery_or_fallback_input("recall"), None);
        assert_eq!(tsquery_or_fallback_input("  waiting_stre  "), None);
    }

    /// Punctuation-only input yields no tokens and therefore no fallback:
    /// punctuation contributes no lexemes, so an OR over it is still empty.
    #[test]
    fn or_fallback_input_none_for_punctuation_only() {
        assert_eq!(tsquery_or_fallback_input("!!! ???"), None);
        assert_eq!(tsquery_or_fallback_input("---"), None);
        assert_eq!(tsquery_or_fallback_input("_ :"), None);
    }

    /// Mixed input: punctuation-only tokens are dropped from the join, and a
    /// token is kept when it carries ANY alphanumeric character (the simple
    /// parser will extract its lexeme; `websearch_to_tsquery` ignores the
    /// rest by design).
    #[test]
    fn or_fallback_input_drops_punctuation_only_tokens() {
        assert_eq!(
            tsquery_or_fallback_input("bare !!! json").as_deref(),
            Some("bare or json")
        );
        assert_eq!(
            tsquery_or_fallback_input("(bare) [json]").as_deref(),
            Some("(bare) or [json]")
        );
    }

    /// The ranked recall must bound its candidate set BEFORE ranking: a
    /// `WITH candidates AS MATERIALIZED` stage carries the full-content FTS
    /// predicate (the preserved deep-match contract), the session/scope
    /// predicates, and the recency ORDER BY closed by the cap LIMIT — the
    /// rank expression, the score ORDER BY and the result LIMIT all follow
    /// the candidate join. Measured driver (2026-11-18, idle prod primary,
    /// 86,110 rows): 'circuit breaker' → 10,805 candidates / 6,093 ms —
    /// ranked cost is O(candidates), so the stage must cap it.
    #[test]
    fn ranked_recall_sql_bounds_the_candidate_stage_before_ranking() {
        // Unscoped shape (since-bound): the cap stage closes before the rank
        // expression appears.
        let sql = ranked_recall_sql("mem.memories", "mem.agents", false, true, false, RecallTsqueryParse::Strict);
        let rank_at = sql
            .find("* 2.0")
            .expect("the key-arm rank weight must be present");
        let stage_at = sql
            .find("WITH candidates AS MATERIALIZED")
            .expect("the candidate stage must be a MATERIALIZED CTE");
        assert!(
            stage_at < rank_at,
            "the stage must precede the rank expression"
        );
        let fts_at = sql
            .find(
                "to_tsvector('simple', m.key || ' ' || m.content) @@ plainto_tsquery('simple', $1)",
            )
            .expect("the full-content FTS predicate must be present");
        assert!(
            fts_at < rank_at,
            "the full-content FTS predicate must stay INSIDE the candidate stage (the deep-match contract)"
        );
        let session_at = sql
            .find(
                "($2::TEXT IS NULL OR m.session_id = $2 OR (m.session_id IS NULL AND m.category = ANY($6)))",
            )
            .expect("the session predicate must be present with the durable-global carve-out");
        assert!(
            session_at < rank_at,
            "the session predicate rides the stage"
        );
        let order_at = sql
            .find("ORDER BY m.updated_at DESC")
            .expect("the stage recency ORDER BY must be present");
        let cap_at = sql
            .find("LIMIT $4")
            .expect("the unscoped cap must bind at $4");
        assert!(
            order_at < cap_at && cap_at < rank_at,
            "the cap LIMIT must close the candidate stage — recency ORDER BY first, LIMIT before the rank expression"
        );
        let time_at = sql
            .find("m.created_at >= $5::TIMESTAMPTZ")
            .expect("the unscoped time bound must shift to $5 (4→5 behind the cap)");
        assert!(time_at < rank_at, "the time predicate is a stage predicate");
        assert!(
            !sql.contains("agent_id = ANY"),
            "the unscoped shape carries no allowlist predicate, got: {sql}"
        );

        // The rank arms are unchanged: key arm whole at 2.0 weight, content
        // arm bounded by the 4096 prefix in both its predicate and its rank.
        assert!(
            sql.contains(
                "ts_rank_cd(to_tsvector('simple', m.key), plainto_tsquery('simple', $1), 32) * 2.0",
            ),
            "key rank arm must stay whole at 2.0 weight, got: {sql}"
        );
        assert_eq!(
            sql.matches("left(m.content, 4096)").count(),
            2,
            "the 4096 prefix bounds only the rank expression's two content uses, got: {sql}"
        );

        // Score ORDER BY and result LIMIT follow the candidate join.
        let join_at = sql
            .find("LEFT JOIN mem.agents")
            .expect("the agent alias join must be present");
        assert!(
            join_at > cap_at,
            "the agent join rides the outer query, after the candidate stage"
        );
        let score_at = sql
            .find("ORDER BY score DESC")
            .expect("the score ORDER BY must be present");
        let result_limit_at = sql
            .find("LIMIT $3")
            .expect("the result limit must bind at $3");
        assert!(
            score_at > rank_at && result_limit_at > score_at,
            "the score ORDER BY and the result LIMIT follow the candidate join"
        );

        // The $1=query invariant in BOTH places: the stage FTS filter and
        // the rank expression bind the same query placeholder.
        assert_eq!(
            sql.matches("plainto_tsquery('simple', $1)").count(),
            5,
            "$1 must be the query in the stage filter (1) and the rank expression (4), got: {sql}"
        );

        // Agent-scoped shape (both time bounds): the allowlist predicate
        // rides INSIDE the stage; the cap and time bounds shift behind it.
        let scoped = ranked_recall_sql("mem.memories", "mem.agents", true, true, true, RecallTsqueryParse::Strict);
        let scoped_rank_at = scoped
            .find("* 2.0")
            .expect("the scoped shape carries the same rank expression");
        let allow_at = scoped
            .find("m.agent_id = ANY($4)")
            .expect("the agent-scoped allowlist predicate must bind at $4");
        assert!(
            allow_at < scoped_rank_at,
            "the allowlist predicate must ride INSIDE the candidate stage"
        );
        let scoped_cap_at = scoped
            .find("LIMIT $5")
            .expect("the agent-scoped cap must bind at $5");
        assert!(
            scoped_cap_at < scoped_rank_at,
            "the agent-scoped cap LIMIT closes the stage"
        );
        assert!(
            scoped.find("m.created_at >= $6::TIMESTAMPTZ").is_some()
                && scoped.find("m.created_at <= $7::TIMESTAMPTZ").is_some(),
            "the agent-scoped time bounds shift to $6/$7 (5→6 behind the allowlist + cap)"
        );
        assert!(
            scoped.find("LIMIT $3").is_some(),
            "the result limit stays at $3 in the agent-scoped shape"
        );
    }

    /// The candidate cap clamps to [500, 2000]: the floor keeps tiny asks
    /// from over-narrowing the window below a useful recall set, and the
    /// ceiling must stay >= the largest supported result limit or LIMIT $3
    /// returns short of a full candidate set.
    #[test]
    fn recall_candidate_cap_floor_and_limit_ceiling() {
        assert_eq!(
            recall_candidate_cap(10),
            500,
            "below the floor clamps up to the floor"
        );
        assert_eq!(
            recall_candidate_cap(500),
            500,
            "at the floor stays at the floor"
        );
        assert_eq!(
            recall_candidate_cap(1000),
            1000,
            "between floor and ceiling passes through"
        );
        assert_eq!(
            recall_candidate_cap(2001),
            2000,
            "above the ceiling clamps to the ceiling"
        );
        assert_eq!(
            recall_candidate_cap(100000),
            2000,
            "far above clamps to the ceiling"
        );
    }

    /// The bind layouts, pinned numerically: unscoped $1 query / $2 session /
    /// $3 result limit / $4 cap / $5.. time / categories last; agent-scoped
    /// $1 query / $2 session / $3 result limit / $4 allowlist / $5 cap /
    /// $6.. time / categories last. The cap slot binds before the time
    /// bounds in both variants, and the durable-global carve-out category
    /// array binds after them (the LIMIT stays the outer $3).
    #[test]
    fn ranked_recall_sql_placeholder_layout_pins_cap_before_time_bounds() {
        let unscoped = ranked_recall_sql("mem.memories", "mem.agents", false, true, true, RecallTsqueryParse::Strict);
        assert!(
            unscoped.contains("LIMIT $4"),
            "the unscoped candidate cap binds at $4, got: {unscoped}"
        );
        assert!(
            unscoped.contains("m.created_at >= $5::TIMESTAMPTZ")
                && unscoped.contains("m.created_at <= $6::TIMESTAMPTZ"),
            "the unscoped time bounds shift 4→5, got: {unscoped}"
        );
        assert!(
            unscoped.contains("(m.session_id IS NULL AND m.category = ANY($7))"),
            "the unscoped carve-out category array binds at $7 (after both time bounds), got: {unscoped}"
        );
        assert!(
            unscoped.contains("LIMIT $3"),
            "the result limit binds at $3 after the candidate join, got: {unscoped}"
        );

        let scoped = ranked_recall_sql("mem.memories", "mem.agents", true, true, true, RecallTsqueryParse::Strict);
        assert!(
            scoped.contains("m.agent_id = ANY($4)"),
            "the allowlist binds at $4, got: {scoped}"
        );
        assert!(
            scoped.contains("LIMIT $5"),
            "the agent-scoped cap binds at $5 behind the allowlist, got: {scoped}"
        );
        assert!(
            scoped.contains("m.created_at >= $6::TIMESTAMPTZ")
                && scoped.contains("m.created_at <= $7::TIMESTAMPTZ"),
            "the agent-scoped time bounds shift 5→6, got: {scoped}"
        );
        assert!(
            scoped.contains("(m.session_id IS NULL AND m.category = ANY($8))"),
            "the agent-scoped carve-out category array binds at $8, got: {scoped}"
        );
        assert!(
            scoped.contains("LIMIT $3"),
            "the result limit binds at $3 in the scoped shape, got: {scoped}"
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

    /// The RECALL-TIME-BOUND bind-type seam, pinned at the unit tier: the
    /// bounds the recall paths bind MUST be `DateTime<Utc>` instants (the
    /// helper's return type — a text bind cannot serialize into the
    /// server-inferred `timestamptz` parameter, postgres-types 0.2.14's
    /// `&str::accepts` excludes TIMESTAMPTZ), and a malformed bound fails
    /// LOUDLY naming the bound, never silently dropping the window.
    #[test]
    fn parse_rfc3339_bound_parses_to_an_instant_and_fails_loudly_on_malformed() {
        // A Z-spelled bound parses to its instant.
        let parsed =
            parse_rfc3339_bound("2026-01-01T00:00:00Z").expect("a valid Z-spelled bound parses");
        assert_eq!(
            parsed.to_rfc3339(),
            "2026-01-01T00:00:00+00:00",
            "the bound parses to the instant it spells"
        );
        // A mixed-offset spelling NORMALIZES to the same instant — the
        // ordering defect the string-compare class shipped.
        let mixed = parse_rfc3339_bound("2026-01-01T01:00:00+02:00")
            .expect("an offset-spelled bound parses");
        let equivalent =
            parse_rfc3339_bound("2025-12-31T23:00:00Z").expect("the equivalent Z spelling parses");
        assert_eq!(
            mixed, equivalent,
            "offset spellings normalize to the same instant"
        );

        // A malformed bound fails LOUDLY, naming the defect AND the bound.
        let err = parse_rfc3339_bound("not-a-timestamp").expect_err("a malformed bound must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid RFC 3339 time bound"),
            "the error must name the defect class; got: {msg}"
        );
        assert!(
            msg.contains("not-a-timestamp"),
            "the error must name the offending bound; got: {msg}"
        );
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

    /// Live regression for the cycle-5 prefix enumeration: `list_prefixed`
    /// must be served by the `idx_memories_key_btree` index
    /// (`text_pattern_ops`) as an index (bitmap) scan — the no-LIMIT
    /// full-table scan it replaced measured 15.2s on the prod leader's
    /// 50K-row store, canary v7. The fixture seeds rare matches in a wide
    /// table so the planner cannot satisfy the LIMIT from the recency index
    /// with a cheap filter; the key btree is the plan that finds 10 matches
    /// without touching the whole table.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_list_prefixed_uses_the_key_btree_index() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "prefixidx");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        // Constructs the table AND the recall indexes (boot DDL). The
        // handle's only job here is the boot; the client it owns is dropped
        // inside the runtime (DropOnThread moves the drop off-thread).
        // Production shape: the pgvector column setup adds the
        // namespace/importance columns the recall SELECTs read.
        runtime.block_on(async {
            PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();
        });
        drop(runtime);

        let qualified_table = format!(
            "{}.{}",
            quote_identifier(&schema.name),
            quote_identifier("memories")
        );
        let qualified_agents = format!(
            "{}.{}",
            quote_identifier(&schema.name),
            quote_identifier("agents")
        );
        // 30K wide rows, only 10 under the queried prefix: an updated_at
        // index walk would filter every row before finding the LIMIT; the
        // key btree goes straight to the matches.
        schema
            .admin
            .batch_execute(&format!(
                "INSERT INTO {qualified_table} (id, key, content, category, created_at, updated_at, agent_id)
                 SELECT 'id_' || g, 'noise:' || g, repeat(md5(g::text), 16), 'daily', NOW(), NOW(),
                        (SELECT id FROM {qualified_agents} WHERE alias = 'default' LIMIT 1)
                 FROM generate_series(1, 30000) g;
                 INSERT INTO {qualified_table} (id, key, content, category, created_at, updated_at, agent_id)
                 SELECT 'match_' || g, 'ws:cycle:' || g, repeat('m', 16), 'daily', NOW(), NOW(),
                        (SELECT id FROM {qualified_agents} WHERE alias = 'default' LIMIT 1)
                 FROM generate_series(1, 10) g;",
            ))
            .expect("seeding the prefix-index fixture must succeed");

        // ANALYZE so the planner has real stats for the cost decision.
        schema
            .admin
            .batch_execute(&format!("ANALYZE {qualified_table};"))
            .expect("ANALYZE must succeed");

        let sql = list_prefixed_sql(&qualified_table, &qualified_agents);
        let explain = schema
            .admin
            .query(&format!("EXPLAIN {sql}"), &[&"ws:cycle:", &None::<String>, &10i64])
            .expect("EXPLAIN on the prefix query must succeed");
        let plan: String = explain
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            plan.contains("idx_memories_key_btree"),
            "the prefix enumeration must be served by the key btree index, got plan:\n{plan}"
        );
    }

    /// Live functional contract: only matching keys come back, most-recent
    /// first, LIMIT respected in storage (not truncated client-side).
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_list_prefixed_returns_prefixed_rows_bounded() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "prefixrt");
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
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            for i in 0..5 {
                memory
                    .store(&format!("evo:cycle:{i}"), &format!("cycle {i}"), MemoryCategory::Core, None)
                    .await
                    .unwrap();
            }
            memory
                .store("other:1", "unrelated", MemoryCategory::Core, None)
                .await
                .unwrap();

            let limited = memory.list_prefixed("evo:cycle:", 3, None).await.unwrap();
            assert_eq!(limited.len(), 3, "the LIMIT must be respected in storage");
            assert!(
                limited.iter().all(|e| e.key.starts_with("evo:cycle:")),
                "only matching keys come back, got: {:?}",
                limited.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
            );

            let all = memory.list_prefixed("evo:cycle:", 100, None).await.unwrap();
            assert_eq!(all.len(), 5, "the full enumeration stays available");

            let none = memory.list_prefixed("absent:", 10, None).await.unwrap();
            assert!(none.is_empty(), "a non-matching prefix enumerates nothing");

            // LIKE wildcards in the prefix are literals, not patterns: the
            // tool's contract is a literal starts_with.
            let literal = memory.list_prefixed("evo:cycle:%", 10, None).await.unwrap();
            assert!(
                literal.is_empty(),
                "a % in the prefix must be escaped, not treated as a wildcard"
            );
        });
    }

    /// The FIRST automated execution surface for the ranked-recall SQL (the
    /// 2026-11-18 `2.0ELSE` class): unit-tier fragment assertions could not
    /// see a whitespace join fusing tokens INSIDE the emitted expression —
    /// only executing a NON-EMPTY-query ranked recall parses the built SQL
    /// server-side. RED baseline: every such recall died at parse
    /// ("trailing junk after numeric literal") before the whitespace repair.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_ranked_recall_nonempty_query_parses_and_returns_rows() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "rankrecall");
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
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            memory
                .store(
                    "rank:alpha",
                    "alpha indexed recall probe",
                    MemoryCategory::Core,
                    Some("rank_session"),
                )
                .await
                .unwrap();
            memory
                .store(
                    "rank:beta",
                    "beta also mentions alpha for the rank",
                    MemoryCategory::Core,
                    Some("rank_session"),
                )
                .await
                .unwrap();
            memory
                .store(
                    "rank:none",
                    "unrelated filler content",
                    MemoryCategory::Core,
                    Some("rank_session"),
                )
                .await
                .unwrap();

            let recalled = memory
                .recall("alpha", 10, Some("rank_session"), None, None)
                .await
                .expect("a non-empty-query ranked recall must PARSE and return rows");
            assert!(
                !recalled.is_empty(),
                "the seeded matching rows must come back from the ranked recall"
            );
            assert!(
                recalled
                    .iter()
                    .all(|e| e.key.contains("alpha") || e.content.contains("alpha")),
                "every recalled row must match the query, got: {:?}",
                recalled
                    .iter()
                    .map(|e| (e.key.as_str(), e.content.as_str()))
                    .collect::<Vec<_>>()
            );
        });
    }

    /// The 2026-11-20 bm25 query-quality batch, live: (1) a long hyphen-chain
    /// key is findable by a hyphenated sub-term (the split normalization),
    /// (2) a deliberately-unsatisfiable multi-token query relaxes to the OR
    /// shape and returns the row instead of reading as absence, and (3) a
    /// single-token query never takes the fallback (nothing to relax).
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_recall_hyphen_key_and_or_relaxation() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "recallorrelax");
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
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            memory
                .store(
                    "decision:2026-11-20:deploy-executor-step-output-bare-json",
                    "the deploy executor step emitted bare json output for zebra umbrella",
                    MemoryCategory::Core,
                    Some("orrelax_session"),
                )
                .await
                .unwrap();

            let seeded = memory
                .recall("bare-json", 10, Some("orrelax_session"), None, None)
                .await
                .expect("the hyphenated sub-term recall must PARSE");
            assert!(
                seeded.iter().any(|e| e.key.contains("bare-json")),
                "a hyphenated sub-term must find the long hyphen-chain key via the AND path, got: {:?}",
                seeded.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
            );

            // 'zebra' matches the seeded content; 'qzxnonexistent' matches
            // nothing — the conjunction is unsatisfiable, so the pre-fix
            // behavior is ZERO rows (the absence-costume defect). After the
            // fix the OR-relaxed shape returns the row.
            let relaxed = memory
                .recall(
                    "zebra qzxnonexistent",
                    10,
                    Some("orrelax_session"),
                    None,
                    None,
                )
                .await
                .expect("the unsatisfiable multi-token recall must PARSE");
            assert!(
                relaxed.iter().any(|e| e.key.contains("bare-json")),
                "an unsatisfiable multi-token AND query must relax to OR and return the matching row, got: {:?}",
                relaxed.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
            );

            // A single token that matches nothing stays empty: a one-token OR
            // query IS the same conjunction — there is nothing to relax.
            let single = memory
                .recall("qzxnonexistent", 10, Some("orrelax_session"), None, None)
                .await
                .expect("the single-token zero-hit recall must PARSE");
            assert!(
                single.is_empty(),
                "a single-token zero-hit query must NOT take the fallback path, got: {:?}",
                single.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
            );
        });
    }

    /// The durable-global carve-out, ported to postgres (the sqlite contract,
    /// fusion verdict 2026-11-18, 3/3 seats): a session-scoped recall admits
    /// session-NULL `core`/`daily` rows — durable global knowledge — while
    /// session-NULL rows of every other category and other sessions' rows
    /// (INCLUDING their `core` rows: a session-bound core row is NOT
    /// durable-global) stay excluded; a session-less recall (`session_id =
    /// None`) admits everything. Both recall builders carry the predicate:
    /// the ranked path (a non-empty query) and the recent path (the empty
    /// query — the recall budget's fallback shape).
    ///
    /// RED baseline: the strict session predicate excluded every
    /// session-NULL row, so the global core/daily rows never came back.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_session_scoped_recall_includes_durable_global_rows() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "durglob");
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
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            let seeds: [(&str, &str, MemoryCategory, Option<&str>); 6] = [
                ("glob:core", "orbital telemetry from the core vault", MemoryCategory::Core, None),
                ("glob:daily", "orbital telemetry daily rotation note", MemoryCategory::Daily, None),
                (
                    "glob:custom",
                    "orbital telemetry custom bucket note",
                    MemoryCategory::Custom("notes".into()),
                    None,
                ),
                (
                    "other:core",
                    "orbital telemetry bound to its origin session",
                    MemoryCategory::Core,
                    Some("other-session"),
                ),
                (
                    "other:chat",
                    "orbital telemetry in another chat",
                    MemoryCategory::Conversation,
                    Some("other-session"),
                ),
                (
                    "mine:chat",
                    "orbital telemetry in my chat",
                    MemoryCategory::Conversation,
                    Some("sess-1"),
                ),
            ];
            for (key, content, category, session) in seeds {
                memory.store(key, content, category, session).await.unwrap();
            }

            let expected_scoped = ["glob:core", "glob:daily", "mine:chat"];
            let expected_all = [
                "glob:core",
                "glob:custom",
                "glob:daily",
                "mine:chat",
                "other:chat",
                "other:core",
            ];

            for (label, query) in [("ranked", "orbital"), ("recent", "")] {
                let scoped = memory
                    .recall(query, 10, Some("sess-1"), None, None)
                    .await
                    .unwrap_or_else(|e| panic!("{label} session-scoped recall must succeed: {e}"));
                let mut keys: Vec<&str> =
                    scoped.iter().map(|e| e.key.as_str()).collect();
                keys.sort_unstable();
                assert_eq!(
                    keys, expected_scoped,
                    "{label}: session scope must admit exactly the session's rows plus the session-NULL core/daily carve-out"
                );

                let unscoped = memory
                    .recall(query, 10, None, None, None)
                    .await
                    .unwrap_or_else(|e| panic!("{label} session-less recall must succeed: {e}"));
                let mut keys: Vec<&str> =
                    unscoped.iter().map(|e| e.key.as_str()).collect();
                keys.sort_unstable();
                assert_eq!(
                    keys, expected_all,
                    "{label}: a session-less recall must admit everything (session_id is a conversation-scope axis, never a visibility gate)"
                );
            }
        });
    }

    /// The cross-backend parity pin: the SAME seeded fixture recalled through
    /// the sqlite backend AND the postgres backend returns the SAME key set,
    /// so the two backends' durable-global carve-out contracts can never
    /// diverge again (the carve-out was decided in sqlite and missed in the
    /// postgres migration — this test pins the parity that was lost).
    ///
    /// The sqlite half runs on the stock Noop embedder (the BM25-only path —
    /// the fleet's `search_mode = "bm25"`); the postgres half runs the ranked
    /// path. Same query, same session, same seeded rows → same admitted set.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn sqlite_and_postgres_recall_admit_the_same_durable_global_set() {
        use crate::sqlite::SqliteMemory;
        use tempfile::TempDir;

        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "parity");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let seeds: [(&str, &str, MemoryCategory, Option<&str>); 6] = [
            (
                "glob:core",
                "orbital telemetry from the core vault",
                MemoryCategory::Core,
                None,
            ),
            (
                "glob:daily",
                "orbital telemetry daily rotation note",
                MemoryCategory::Daily,
                None,
            ),
            (
                "glob:custom",
                "orbital telemetry custom bucket note",
                MemoryCategory::Custom("notes".into()),
                None,
            ),
            (
                "other:core",
                "orbital telemetry bound to its origin session",
                MemoryCategory::Core,
                Some("other-session"),
            ),
            (
                "other:chat",
                "orbital telemetry in another chat",
                MemoryCategory::Conversation,
                Some("other-session"),
            ),
            (
                "mine:chat",
                "orbital telemetry in my chat",
                MemoryCategory::Conversation,
                Some("sess-1"),
            ),
        ];

        let sqlite_keys = {
            let tmp = TempDir::new().unwrap();
            let memory = SqliteMemory::new("parity", tmp.path()).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                for (key, content, category, session) in seeds.iter() {
                    memory
                        .store(key, content, category.clone(), *session)
                        .await
                        .unwrap();
                }
                let recalled = memory
                    .recall("orbital", 10, Some("sess-1"), None, None)
                    .await
                    .expect("the sqlite recall must succeed");
                let mut keys: Vec<String> = recalled.iter().map(|e| e.key.clone()).collect();
                keys.sort();
                keys
            })
        };

        runtime.block_on(async {
            let memory = PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            for (key, content, category, session) in seeds.iter() {
                memory
                    .store(key, content, category.clone(), *session)
                    .await
                    .unwrap();
            }
            let recalled = memory
                .recall("orbital", 10, Some("sess-1"), None, None)
                .await
                .expect("the postgres ranked recall must succeed");
            let mut postgres_keys: Vec<&str> = recalled.iter().map(|e| e.key.as_str()).collect();
            postgres_keys.sort_unstable();

            assert_eq!(
                sqlite_keys, postgres_keys,
                "the two backends must admit the same rows under the same session scope"
            );
            assert_eq!(
                postgres_keys,
                ["glob:core", "glob:daily", "mine:chat"],
                "the parity set is the carve-out contract: the session's rows plus session-NULL core/daily"
            );
        });
    }

    /// The ranked recall must bound its candidate set to the NEWEST matching
    /// window before ranking (measured 2026-11-18 on the idle prod primary,
    /// 86,110 rows: 'circuit breaker' → 10,805 candidates / 6,093 ms;
    /// 'drive_sync' → 6,418 candidates / 6,210 ms — the per-candidate
    /// tsvector recheck makes ranked cost O(candidates), so an unbounded
    /// stage burns the recall budget on large stores). RED shape: 800 OLD
    /// docs rank HIGHER (the term repeated ~200× inside the rank prefix)
    /// than 500 NEWER docs (single occurrence), and the old shape's unbounded
    /// stage lets the old high-rank rows outrank into the results.
    ///
    /// Blocking admin SQL stays OUTSIDE the Tokio runtime blocks — the
    /// nested-block_on panic class the pgvector fixture pins.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_ranked_recall_ranks_within_the_newest_candidate_window() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "window");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let memory = runtime.block_on(async {
            let memory = PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            // OLD half: the term repeated ~200× inside the rank prefix →
            // high content rank.
            let old_content = "needle ".repeat(200);
            for i in 0..800 {
                memory
                    .store(
                        &format!("old:needle:{i}"),
                        &old_content,
                        MemoryCategory::Core,
                        None,
                    )
                    .await
                    .unwrap();
            }
            // NEW half: matches with a single occurrence → low rank, but
            // NEWER than every old row.
            let new_content = "filler context needle filler";
            for i in 0..500 {
                memory
                    .store(
                        &format!("new:needle:{i}"),
                        new_content,
                        MemoryCategory::Core,
                        None,
                    )
                    .await
                    .unwrap();
            }
            memory
        });

        // Make the window separation unambiguous: every old row a full day
        // older than the new half (store timestamps share milliseconds, so
        // store-order recency alone is not a safe separator at 1300 rows).
        schema
            .admin
            .batch_execute(&format!(
                "UPDATE {}.memories SET created_at = NOW() - INTERVAL '1 day',
                 updated_at = NOW() - INTERVAL '1 day' WHERE key LIKE 'old:%'",
                quote_identifier(&schema.name)
            ))
            .expect("shifting the old seed set into the past must succeed");

        runtime.block_on(async {
            let recalled = memory
                .recall("needle", 10, None, None, None)
                .await
                .expect("the ranked recall must parse and return rows");
            assert_eq!(recalled.len(), 10, "the result limit must be respected");
            assert!(
                recalled.iter().all(|e| e.key.starts_with("new:")),
                "every result must come from the newest candidate window; got: {:?}",
                recalled.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
            );
        });
    }

    /// Live proof for the RECALL-TIME-BOUND wire fix: a recall with since/until
    /// binds TIMESTAMPTZ instants and returns ONLY the in-window rows. RED
    /// (2026-11-19): the recall paths bound the bounds as `&str` against the
    /// `$n::TIMESTAMPTZ` casts — postgres-types 0.2.14's `&str::accepts`
    /// covers only text-ish types, the server assigns an untyped `$n`
    /// parameter the cast target's type, and every time-bound recall failed
    /// at the wire ("cannot convert between the Rust type `Option<&str>` and
    /// the Postgres type `timestamptz`") — with NO automated coverage (the
    /// V3 Task-1 review finding 6 exposure).
    ///
    /// Covers all four affected bind sites: the unscoped recency + ranked
    /// paths and both agent-scoped paths (`recall_for_agents`).
    ///
    /// Time separation is set by the schema-admin UPDATE (the Task-1 window
    /// precedent — store timestamps share milliseconds, so store-order
    /// recency alone is not a safe separator), and the since/until bounds
    /// derive from the DATABASE's own post-shift timestamps, so membership
    /// never depends on client/server clock skew.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_recall_with_time_bounds_returns_windowed_rows() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "timewin");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let (memory, agent) = runtime.block_on(async {
            let memory = PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            // Three same-term unscoped groups; the admin shift separates
            // them by whole days/hours, not store-order recency.
            for i in 0..12 {
                memory
                    .store(
                        &format!("win:old:{i}"),
                        "timewin marker term",
                        MemoryCategory::Core,
                        None,
                    )
                    .await
                    .unwrap();
            }
            for i in 0..12 {
                memory
                    .store(
                        &format!("win:in:{i}"),
                        "timewin marker term",
                        MemoryCategory::Core,
                        None,
                    )
                    .await
                    .unwrap();
            }
            for i in 0..12 {
                memory
                    .store(
                        &format!("win:future:{i}"),
                        "timewin marker term",
                        MemoryCategory::Core,
                        None,
                    )
                    .await
                    .unwrap();
            }
            // The agent-bound pair for the recall_for_agents legs.
            let agent = memory.ensure_agent_uuid("timewin_agent").await.unwrap();
            memory
                .store_with_agent(
                    "win:agent:in",
                    "timewin marker term",
                    MemoryCategory::Core,
                    None,
                    None,
                    None,
                    Some(&agent),
                )
                .await
                .unwrap();
            memory
                .store_with_agent(
                    "win:agent:old",
                    "timewin marker term",
                    MemoryCategory::Core,
                    None,
                    None,
                    None,
                    Some(&agent),
                )
                .await
                .unwrap();
            (memory, agent)
        });

        schema
            .admin
            .batch_execute(&format!(
                "UPDATE {}.memories SET created_at = NOW() - INTERVAL '1 day',
                 updated_at = NOW() - INTERVAL '1 day'
                 WHERE key LIKE 'win:old:%' OR key = 'win:agent:old';
                UPDATE {}.memories SET created_at = NOW() + INTERVAL '2 hours',
                 updated_at = NOW() + INTERVAL '2 hours' WHERE key LIKE 'win:future:%';
                UPDATE {}.memories SET created_at = NOW(), updated_at = NOW()
                 WHERE key LIKE 'win:in:%' OR key = 'win:agent:in';",
                quote_identifier(&schema.name),
                quote_identifier(&schema.name),
                quote_identifier(&schema.name)
            ))
            .expect("shifting the seed groups must succeed");

        // The bounds derive from the DATABASE's own in-window timestamps:
        // since = min - 1s (excludes the day-old group), until = max + 1s
        // (excludes the +2h group).
        let (since, until) = {
            let row = schema
                .admin
                .query_one(
                    &format!(
                        "SELECT min(created_at), max(created_at) FROM {}.memories
                         WHERE key LIKE 'win:in:%' OR key = 'win:agent:in'",
                        quote_identifier(&schema.name)
                    ),
                    &[],
                )
                .expect("reading the in-window bounds must succeed");
            let min: chrono::DateTime<chrono::Utc> = row.get(0);
            let max: chrono::DateTime<chrono::Utc> = row.get(1);
            (
                (min - chrono::Duration::seconds(1)).to_rfc3339(),
                (max + chrono::Duration::seconds(1)).to_rfc3339(),
            )
        };

        runtime.block_on(async {
            // The ranked path (non-empty query → ranked_recall_sql, $5/$6).
            let ranked = memory
                .recall("timewin marker term", 50, None, Some(&since), Some(&until))
                .await
                .expect("the time-bound ranked recall must bind and return rows");
            let mut ranked_keys: Vec<&str> = ranked.iter().map(|e| e.key.as_str()).collect();
            ranked_keys.sort();
            // The 12 in-window unscoped rows PLUS the in-window agent-bound
            // row: the unscoped paths carry no agent predicate, so the
            // agent-bound row belongs here too (it returns in every leg).
            let mut expected_in: Vec<String> = (0..12).map(|i| format!("win:in:{i}")).collect();
            expected_in.push("win:agent:in".to_string());
            expected_in.sort();
            assert_eq!(
                ranked_keys, expected_in,
                "the ranked recall must return ONLY the in-window rows"
            );

            // The recency path (empty query → recent_recall_sql, $2/$3).
            let recency = memory
                .recall("*", 50, None, Some(&since), Some(&until))
                .await
                .expect("the time-bound recency recall must bind and return rows");
            let recency_keys: Vec<&str> = recency.iter().map(|e| e.key.as_str()).collect();
            assert_eq!(
                recency_keys.len(),
                13,
                "the 12 in-window + the in-window agent row"
            );
            assert!(
                recency_keys
                    .iter()
                    .all(|k| *k == "win:agent:in" || k.starts_with("win:in:")),
                "the recency recall must return ONLY the in-window rows; got: {recency_keys:?}"
            );

            // The agent-scoped ranked path (ranked_recall_sql agent_scoped,
            // $6/$7) and recency path (recent_recall_sql_for_agents, $2/$3).
            let agent_ranked = memory
                .recall_for_agents(
                    &[&agent],
                    "timewin marker term",
                    50,
                    None,
                    Some(&since),
                    Some(&until),
                )
                .await
                .expect("the time-bound agent-scoped ranked recall must bind and return rows");
            let agent_ranked_keys: Vec<&str> =
                agent_ranked.iter().map(|e| e.key.as_str()).collect();
            assert_eq!(
                agent_ranked_keys,
                vec!["win:agent:in"],
                "the agent-scoped ranked recall must return ONLY the in-window agent row"
            );

            let agent_recency = memory
                .recall_for_agents(&[&agent], "*", 50, None, Some(&since), Some(&until))
                .await
                .expect("the time-bound agent-scoped recency recall must bind and return rows");
            let agent_recency_keys: Vec<&str> =
                agent_recency.iter().map(|e| e.key.as_str()).collect();
            assert_eq!(
                agent_recency_keys,
                vec!["win:agent:in"],
                "the agent-scoped recency recall must return ONLY the in-window agent row"
            );
        });
    }

    /// Live companion to the window test: a MALFORMED time bound fails
    /// LOUDLY with an error naming the bound — never a silent drop of the
    /// window (the T2 export precedent: the old string-compare silently
    /// produced an empty export; an unparseable bound must not degrade a
    /// bounded recall into an unbounded one either).
    ///
    /// RED (2026-11-19): the pre-fix failure was the wire-bind error
    /// ("cannot convert between the Rust type `Option<&str>` and the
    /// Postgres type `timestamptz`") — an Err of the WRONG class, naming no
    /// bound; the assertions below pin the parse-error class.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_recall_malformed_since_fails_loudly() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "badbound");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let memory = runtime.block_on(async {
            PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                Some(true),
                None,
                None,
                None,
            )
            .unwrap()
        });

        runtime.block_on(async {
            // The ranked path: the parse guard sits at the recall() seam,
            // shared by both paths.
            let err = memory
                .recall("anything", 5, None, Some("not-a-timestamp"), None)
                .await
                .expect_err("a malformed since bound must fail loudly");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("invalid RFC 3339 time bound"),
                "the error must name the defect class; got: {msg}"
            );
            assert!(
                msg.contains("not-a-timestamp"),
                "the error must name the offending bound; got: {msg}"
            );

            // The recency path: a malformed until fails the same way.
            let err = memory
                .recall("*", 5, None, None, Some("2026-13-45T99:99:99Z"))
                .await
                .expect_err("a malformed until bound must fail loudly");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("invalid RFC 3339 time bound"),
                "the recency path must fail loudly too; got: {msg}"
            );
        });
    }

    /// CONTRACT PIN (passes today — labeled a pin, not a RED): the candidate
    /// stage must keep the FULL-content FTS predicate
    /// (`to_tsvector('simple', m.key || ' ' || m.content)`). A row whose
    /// ONLY term match sits beyond the 4096-char rank prefix must still be
    /// recalled — it ranks by its key arm instead of a deep-position content
    /// rank. If the two-stage rewrite bounded the stage filter to the rank
    /// prefix, this row would vanish from recall entirely.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_ranked_recall_deep_body_match_still_recalled() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "deepmatch");
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
                // Production shape: the namespace/importance columns ride the
                // pgvector column setup, so the fixture needs it enabled.
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

            // The ONLY match for the query sits at char ~5001 — beyond the
            // 4096-char rank prefix.
            let deep_content = format!("{} needle", "z".repeat(5000));
            memory
                .store("deep:needle", &deep_content, MemoryCategory::Core, None)
                .await
                .unwrap();

            let recalled = memory
                .recall("needle", 10, None, None, None)
                .await
                .expect("the ranked recall must parse and return rows");
            assert!(
                recalled.iter().any(|e| e.key == "deep:needle"),
                "a deep-body match beyond the rank prefix must still be recalled; got: {:?}",
                recalled.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
            );
        });
    }

    /// The 2026-11-17/18 fleet-death class, automated: server-side backends
    /// terminated under the memory backend (a patroni restart is the prod
    /// shape) must NOT poison the backend. RED in the single-client world:
    /// the one dead client never heals — every later operation fails until
    /// the process restarts (the SOP/cron fleet stopped at 2026-11-17
    /// 23:40Z until manual pod rolls; tokio-postgres has no auto-reconnect).
    ///
    /// The memory connections tag themselves via the URL's
    /// `application_name` so the schema admin can enumerate the pool's
    /// backends server-side. Admin SQL stays OUTSIDE the Tokio runtime
    /// blocks (the nested-block_on panic class the pgvector fixture pins).
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_pool_replaces_a_terminated_connection_transparently() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "poolterm");
        let pool_url = format!("{database_url}?application_name=zc_mem_poolterm");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let memory = PostgresMemory::new(
            "ci",
            &pool_url,
            &schema.name,
            "memories",
            Some(5),
            None,
            // Production shape: the namespace/importance columns ride the
            // pgvector column setup, so the fixture needs it enabled.
            Some(true),
            None,
            None,
            Some(2),
        )
        .unwrap();

        let pool_pids = |admin: &mut Client| -> Vec<i32> {
            admin
                .query(
                    "SELECT pid FROM pg_stat_activity WHERE application_name = 'zc_mem_poolterm'",
                    &[],
                )
                .expect("pg_stat_activity enumeration must succeed")
                .iter()
                .map(|row| row.get::<_, i32>(0))
                .collect()
        };

        let before = pool_pids(&mut schema.admin);
        assert!(!before.is_empty(), "the boot must hold its connections");

        // Terminate EVERY backend serving the pool — the patroni-restart
        // shape (the admin's own session carries no application_name, so it
        // is not matched).
        schema
            .admin
            .batch_execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE application_name = 'zc_mem_poolterm'",
            )
            .expect("the terminate sweep must succeed");

        runtime.block_on(async {
            // The NEXT memory operation must succeed: dead connections are
            // discarded, one reconnect is attempted, and the operation runs
            // once on the fresh connection — transparently to the caller.
            memory
                .store(
                    "term:probe",
                    "survives the terminate",
                    MemoryCategory::Core,
                    None,
                )
                .await
                .expect("the first operation after a backend terminate must succeed");
            let got = memory
                .get("term:probe")
                .await
                .expect("the read after the terminate must succeed")
                .expect("the stored row must be readable");
            assert_eq!(got.content, "survives the terminate");
            let recalled = memory
                .recall("survives", 5, None, None, None)
                .await
                .expect("a ranked recall after the terminate must also succeed");
            assert!(
                recalled.iter().any(|e| e.key == "term:probe"),
                "the ranked recall must surface the stored row, got: {:?}",
                recalled.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
            );
        });

        // No later op ever runs on the terminated pids: they are gone
        // server-side, and the backends now serving the pool are fresh.
        let after = pool_pids(&mut schema.admin);
        assert!(
            !after.iter().any(|pid| before.contains(pid)),
            "the terminated pids must be gone; before: {before:?}, after: {after:?}"
        );
    }

    /// The pool hands out DISTINCT connections: three simultaneous
    /// acquisitions carry three different server backends
    /// (`pg_backend_pid()`), and a released connection is REUSED from the
    /// idle set — the re-acquired pid repeats instead of paying a new
    /// backend. RED: the acquire surface does not exist pre-change
    /// (compile-RED, the fork precedent).
    ///
    /// The acquisitions run on the PLAIN test thread: the synchronous
    /// client's internal `block_on` must never meet an active Tokio runtime
    /// (the nested-runtime panic class).
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_pool_hands_out_distinct_connections() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "poolpids");
        let memory = PostgresMemory::new(
            "ci",
            &database_url,
            &schema.name,
            "memories",
            Some(5),
            None,
            // Production shape: the namespace/importance columns ride the
            // pgvector column setup, so the fixture needs it enabled.
            Some(true),
            None,
            None,
            Some(3),
        )
        .unwrap();
        let pool = memory.pool.get().clone();

        let mut pids = Vec::new();
        let mut guards = Vec::new();
        for _ in 0..3 {
            let mut guard = pool
                .clone()
                .acquire()
                .expect("the warmed pool must hand out a connection");
            let pid: i32 = guard
                .query_one("SELECT pg_backend_pid()", &[])
                .expect("pg_backend_pid must answer")
                .get(0);
            pids.push(pid);
            guards.push(guard);
        }
        for i in 0..3 {
            for j in (i + 1)..3 {
                assert_ne!(
                    pids[i], pids[j],
                    "each simultaneous acquisition must carry a distinct backend"
                );
            }
        }

        // Release all three; the pool's idle set holds them (no connection
        // is created or dropped by a release).
        for guard in guards {
            guard.release();
        }
        assert_eq!(
            pool.owned_connections(),
            3,
            "a release must not change the pool's connection census"
        );

        let mut again = pool
            .clone()
            .acquire()
            .expect("the released connections must be acquirable");
        let pid_again: i32 = again
            .query_one("SELECT pg_backend_pid()", &[])
            .expect("pg_backend_pid must answer")
            .get(0);
        assert!(
            pids.contains(&pid_again),
            "the re-acquired connection must be an idle REUSE of a released backend, got {pid_again}, seen {pids:?}"
        );
        again.release();
        assert_eq!(pool.owned_connections(), 3);
    }

    /// The boot degradation guard: when extra pool connections cannot be
    /// established, the boot still SUCCEEDS with a smaller pool and the
    /// degraded pool serves operations.
    ///
    /// The limit rides a DEDICATED non-superuser role: role CONNECTION
    /// LIMITS are NOT enforced for superusers (verified against the fixture:
    /// a superuser's second connect ignores the limit, an ordinary role's
    /// second connect dies with `FATAL: too many connections for role`). The
    /// shared `zeroclaw_test` role is the container superuser, so it cannot
    /// carry the limit without breaking the whole suite. The dedicated role
    /// owns the test schema (the boot DDL runs as it) and its CONNECTION
    /// LIMIT 1 leaves exactly ONE slot for the pool: the primary connects,
    /// every extra connect is refused, and the pool degrades to one
    /// connection.
    ///
    /// `pgvector` stays OFF for this fixture (the limited role cannot CREATE
    /// EXTENSION); the admin adds the `namespace`/`importance` columns AFTER
    /// the boot so the store/get statements resolve. The role + schema are
    /// dropped in teardown, so later serial-tier tests are not poisoned even
    /// on a mid-test panic.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_pool_degrades_when_extra_connections_fail() {
        let database_url = postgres_test_url();
        let mut schema = TestSchema::create(&database_url, "pooldegrade");
        let schema_ident = quote_identifier(&schema.name);
        schema
            .admin
            .batch_execute(
                r#"DO $$ BEGIN
                    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zc_pool_degrade') THEN
                        DROP OWNED BY zc_pool_degrade CASCADE;
                        DROP ROLE zc_pool_degrade;
                    END IF;
                END $$;
                CREATE ROLE zc_pool_degrade LOGIN PASSWORD 'pool-degrade-pw';
                ALTER ROLE zc_pool_degrade CONNECTION LIMIT 1;"#,
            )
            .expect("the dedicated limit role must be creatable");
        schema
            .admin
            .batch_execute(
                // The boot's `CREATE SCHEMA IF NOT EXISTS` pays the
                // database-level CREATE privilege check even when the schema
                // already exists (and the role owns it).
                "GRANT CREATE ON DATABASE zeroclaw_test TO zc_pool_degrade;",
            )
            .expect("the database CREATE grant must succeed");
        schema
            .admin
            .batch_execute(&format!(
                "ALTER SCHEMA {schema_ident} OWNER TO zc_pool_degrade;"
            ))
            .expect("the schema ownership must transfer to the limited role");

        // Teardown guard: the role owns the table until the schema
        // cascades, so the cleanup drops the schema FIRST. The role's
        // database-level CREATE grant lingers in pg_database.datacl and
        // blocks DROP ROLE — DROP OWNED revokes it before the role drops.
        // Best-effort: a failed cleanup cannot poison later tests (the
        // limit is role-local) and the next run's DO block heals it. The
        // guard carries its OWN admin connection (the sync Client cannot
        // be cloned).
        struct RoleCleanup {
            admin: Client,
            schema_ident: String,
        }
        impl Drop for RoleCleanup {
            fn drop(&mut self) {
                let _ = self.admin.batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE;
                     DROP OWNED BY zc_pool_degrade CASCADE;
                     DROP ROLE IF EXISTS zc_pool_degrade;",
                    self.schema_ident
                ));
            }
        }
        let cleanup = RoleCleanup {
            admin: Client::connect(&database_url, NoTls)
                .expect("the teardown admin connection must open"),
            schema_ident: schema_ident.clone(),
        };

        let role_url = "postgres://zc_pool_degrade:pool-degrade-pw@127.0.0.1:5432/zeroclaw_test";
        let memory = PostgresMemory::new(
            "ci",
            role_url,
            &schema.name,
            "memories",
            Some(5),
            None,
            Some(false), // pgvector OFF: the limited role cannot CREATE EXTENSION
            None,
            None,
            Some(4),
        )
        .expect("the boot must succeed with a degraded pool");

        // The boot created the table as the limited role; the admin then
        // adds the columns the store/get statements reference.
        let qualified_table = format!("{schema_ident}.memories");
        schema
            .admin
            .batch_execute(&format!(
                "ALTER TABLE {qualified_table} ADD COLUMN namespace TEXT DEFAULT 'default';
                 ALTER TABLE {qualified_table} ADD COLUMN importance REAL;"
            ))
            .expect("the admin column additions must succeed");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            memory
                .store(
                    "degrade:key",
                    "degraded pool serves",
                    MemoryCategory::Core,
                    None,
                )
                .await
                .expect("the degraded pool must serve stores");
            let got = memory
                .get("degrade:key")
                .await
                .expect("the degraded pool must serve reads")
                .expect("the stored row must be readable");
            assert_eq!(got.content, "degraded pool serves");
            assert!(
                memory.health_check().await,
                "health_check on the degraded pool reports TRUE (heal-in-place semantics)"
            );
        });

        // The failed extra connects degraded the pool to exactly ONE
        // connection (the failed boot connects are NOT retried into the
        // census; the pool can still expand at runtime if the limit lifts).
        assert_eq!(
            memory.pool.get().owned_connections(),
            1,
            "the pool must carry exactly the one connection that survived the boot limit"
        );

        drop(memory);
        drop(runtime);
        drop(cleanup); // DROP SCHEMA CASCADE + DROP ROLE (TestSchema's Drop no-ops after)
    }

    /// REGRESSION PIN (not RED — labeled as such): 8 concurrent store+recall
    /// futures on a pool of 4 all succeed with the correct rows. The pool's
    /// condvar parks the overflow waiters and hands out each released
    /// connection, instead of serializing every operation behind one mutex
    /// or failing the overflow outright.
    #[test]
    #[ignore = "requires ZEROCLAW_TEST_POSTGRES_URL"]
    fn postgres_pool_serves_concurrent_memory_ops() {
        let database_url = postgres_test_url();
        let schema = TestSchema::create(&database_url, "poolconc");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let memory = Arc::new(
            PostgresMemory::new(
                "ci",
                &database_url,
                &schema.name,
                "memories",
                Some(5),
                None,
                // Production shape: the namespace/importance columns ride
                // the pgvector column setup, so the fixture needs it
                // enabled.
                Some(true),
                None,
                None,
                Some(4),
            )
            .unwrap(),
        );

        runtime.block_on(async {
            let mut handles = Vec::new();
            for i in 0..8 {
                let memory = Arc::clone(&memory);
                handles.push(tokio::spawn(async move {
                    let key = format!("pool:conc:{i}");
                    let content = format!("concurrent payload {i}");
                    memory
                        .store(&key, &content, MemoryCategory::Core, None)
                        .await?;
                    let got = memory
                        .get(&key)
                        .await?
                        .expect("the stored row must be readable");
                    assert_eq!(got.content, content, "row {key} must round-trip");
                    // The empty-query recall is the indexed recent path: at
                    // most 9 rows exist in the fixture, so a LIMIT 20 recall
                    // returns every row regardless of recency ties.
                    let recalled = memory.recall("", 20, None, None, None).await?;
                    assert!(
                        recalled.iter().any(|e| e.key == key),
                        "the recent recall must surface {key}, got: {:?}",
                        recalled.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
                    );
                    Ok::<(), anyhow::Error>(())
                }));
            }
            for handle in handles {
                handle
                    .await
                    .expect("the concurrent task must not panic")
                    .expect("each concurrent store+recall must succeed");
            }
        });
    }

    /// The pool-size resolver clamps to [1, POSTGRES_POOL_CAP]; `None` takes
    /// the default. The knob is `Option<u64>` END TO END (the
    /// `recall_soft_timeout_ms` precedent) — `None` means the default with
    /// NO debops config change.
    #[test]
    fn resolve_pool_size_clamps_to_bounds() {
        assert_eq!(
            resolve_pool_size(None),
            4,
            "None takes POSTGRES_POOL_DEFAULT"
        );
        assert_eq!(
            resolve_pool_size(None),
            POSTGRES_POOL_DEFAULT as usize,
            "None resolves to the const, not a magic number"
        );
        assert_eq!(
            resolve_pool_size(Some(0)),
            1,
            "0 clamps UP to 1 — a zero-sized pool would deadlock every operation"
        );
        assert_eq!(resolve_pool_size(Some(1)), 1);
        assert_eq!(resolve_pool_size(Some(4)), 4);
        assert_eq!(
            resolve_pool_size(Some(17)),
            16,
            "above POSTGRES_POOL_CAP clamps down"
        );
        assert_eq!(
            resolve_pool_size(Some(64)),
            16,
            "far above POSTGRES_POOL_CAP clamps down"
        );
        assert_eq!(
            resolve_pool_size(Some(POSTGRES_POOL_CAP)),
            16,
            "the cap itself passes through"
        );
    }

    /// The PRIMARY connection-loss signal is the OR of the two closed
    /// markers; only (false, false) is not a connection loss. The io-source
    /// belt and the full classify-then-retry flow pin at the LIVE tier (the
    /// terminate test) — no public constructor yields a connection-loss
    /// `postgres::Error` (`io()` and `closed()` are pub(crate) in
    /// tokio-postgres 0.7.18; the `Kind` enum is private). Constructor
    /// visibility is NOT hacked to make this unit-testable.
    #[test]
    fn connection_loss_predicate_is_the_closed_disjunction() {
        assert!(
            !connection_loss_disjunction(false, false),
            "(false, false) is not a connection loss"
        );
        assert!(
            connection_loss_disjunction(true, false),
            "err closed → loss"
        );
        assert!(
            connection_loss_disjunction(false, true),
            "client closed → loss"
        );
        assert!(
            connection_loss_disjunction(true, true),
            "both closed → loss"
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

#[cfg(test)]
mod hyphen_tsquery_tests {
    use super::normalize_tsquery_input;

    /// The 2026-11-20 defect: a compound query could never match a key whose
    /// term sits inside a longer hyphen chain. Splitting makes the query the
    /// conjunction of its words, which both forms satisfy.
    #[test]
    fn splits_hyphenated_compounds_into_words() {
        assert_eq!(normalize_tsquery_input("bare-json"), "bare json");
        assert_eq!(
            normalize_tsquery_input("deploy-executor-step-output-bare-json"),
            "deploy executor step output bare json"
        );
        assert_eq!(normalize_tsquery_input("a--b"), "a  b");
    }

    /// Non-hyphenated queries pass through untouched (no behaviour change for
    /// the overwhelmingly common case).
    #[test]
    fn leaves_plain_queries_untouched() {
        assert_eq!(normalize_tsquery_input("circuit breaker"), "circuit breaker");
        assert_eq!(normalize_tsquery_input("recall"), "recall");
        assert_eq!(normalize_tsquery_input(""), "");
        assert_eq!(normalize_tsquery_input("wasmtime_heapless"), "wasmtime_heapless");
    }
}
