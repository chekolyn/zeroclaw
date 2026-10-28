//! Per-call wall-clock budget for memory recall (tuning cycle 3).
//!
//! [`BudgetedMemory`] bounds each recall call: the ranked primary attempt
//! gets a soft wall-clock budget, and a call that exceeds it degrades to the
//! backend's recent-entries path (the empty-query contract) instead of
//! stalling the turn. Measured prod shape (2026-10-27): a 50K-row postgres
//! store seq-scanned per recall, costing 6.3–6.7s per `memory_recall` call
//! and 174.5s across 13 calls on one digest run — a latency bound was the
//! missing resource limit (size budgets already exist downstream:
//! `MemoryInjectConfig` caps entries/chars; this bounds TIME).
//!
//! The decorator wraps the per-agent handle (above `AgentScopedMemory`), so
//! the bound covers the whole per-call fan-out including the category-refill
//! loop — one budget per caller-visible recall, not per backend query.
//!
//! Degradation contract:
//!
//! - primary returns within the budget → ranked results, unchanged;
//! - primary errors before the budget elapses → the error propagates (a
//!   real backend failure must not be masked by a recency fallback);
//! - primary times out OR errors at/after the budget → one recent-entries
//!   fallback attempt within a bounded floor, then a WARN with the elapsed
//!   time; if the fallback also exceeds its budget the recall errors — the
//!   call is bounded either way;
//! - `soft_timeout == 0` disables the budget entirely (byte-identical
//!   pass-through, no timing overhead).
//!
//! Every bounded recall emits a trace record carrying `elapsed_ms`, the
//! result count, and whether the result was degraded, so future measurement
//! cycles see recall latency and breadth without new instrumentation.

use crate::traits::{Memory, MemoryCategory, MemoryEntry, PrincipalScope};
use async_trait::async_trait;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default per-call recall budget: 2 seconds. Healthy indexed recall is
/// milliseconds; this bounds the degraded tail (the measured prod pathology
/// was 6.3–6.7s per call).
pub const DEFAULT_RECALL_SOFT_TIMEOUT: Duration = Duration::from_millis(2000);

/// Wall-clock budget for the recent-entries fallback attempt. The fallback
/// path is the backend's cheapest recall (indexed recency), so a generous
/// floor is enough; it keeps the TOTAL call bounded at roughly
/// `soft_timeout + FALLBACK_BUDGET_FLOOR` even when the primary hangs.
const FALLBACK_BUDGET_FLOOR: Duration = Duration::from_millis(1000);

/// Decorator that bounds recall wall-clock time and degrades gracefully.
/// Wraps the per-agent handle; non-generic over the inner backend so it can
/// wrap `Arc<dyn Memory>` compositions (the [`RetrievalPipeline`] pattern).
pub struct BudgetedMemory {
    inner: Arc<dyn Memory>,
    soft_timeout: Duration,
}

impl BudgetedMemory {
    pub fn new(inner: Arc<dyn Memory>, soft_timeout: Duration) -> Self {
        Self {
            inner,
            soft_timeout,
        }
    }

    fn disabled(&self) -> bool {
        self.soft_timeout.is_zero()
    }

    /// Bound one recall attempt: the ranked primary gets the soft budget; a
    /// budget-exhausted attempt (tokio timeout, or a backend error arriving
    /// at/after the budget — the shape a server-side per-attempt
    /// `statement_timeout` produces) gets exactly one recent-entries
    /// fallback within a bounded floor. Emits the elapsed/count/degraded
    /// trace record either way.
    async fn bounded<P, PF, F, FF>(
        &self,
        primary: P,
        fallback: F,
        label: &str,
    ) -> anyhow::Result<Vec<MemoryEntry>>
    where
        P: FnOnce() -> PF,
        PF: Future<Output = anyhow::Result<Vec<MemoryEntry>>>,
        F: FnOnce() -> FF,
        FF: Future<Output = anyhow::Result<Vec<MemoryEntry>>>,
    {
        let start = Instant::now();
        let outcome = tokio::time::timeout(self.soft_timeout, primary()).await;
        let elapsed = start.elapsed();
        match outcome {
            Ok(Ok(entries)) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": elapsed.as_millis() as u64,
                            "count": entries.len(),
                            "degraded": false,
                        })),
                    "memory recall completed within the soft budget"
                );
                return Ok(entries);
            }
            // A real backend failure that did NOT consume the budget: the
            // recency fallback must not mask it.
            Ok(Err(error)) if elapsed < self.soft_timeout => return Err(error),
            // Budget exhausted: tokio timeout, or an error that arrived after
            // the budget elapsed (the server-side per-attempt cancel shape).
            Ok(Err(error)) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": elapsed.as_millis() as u64,
                            "error": error.to_string(),
                        })),
                    "memory recall errored after exhausting the soft budget; degrading"
                );
            }
            Err(_) => {}
        }

        // Degrade: one recent-entries attempt within a bounded floor. The
        // total call stays bounded at roughly soft + floor even when the
        // primary hangs.
        let fallback_budget = FALLBACK_BUDGET_FLOOR
            .max(self.soft_timeout.saturating_sub(start.elapsed()))
            .min(self.soft_timeout + FALLBACK_BUDGET_FLOOR);
        let fallback_start = Instant::now();
        match tokio::time::timeout(fallback_budget, fallback()).await {
            Ok(Ok(entries)) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": fallback_start.elapsed().as_millis() as u64,
                            "count": entries.len(),
                            "degraded": true,
                            "soft_timeout_ms": self.soft_timeout.as_millis() as u64,
                        })),
                    "memory recall exceeded the soft budget; degraded to recent-entries fallback"
                );
                Ok(entries)
            }
            Ok(Err(error)) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": fallback_start.elapsed().as_millis() as u64,
                            "degraded": true,
                            "error": error.to_string(),
                        })),
                    "memory recall fallback errored after the soft budget"
                );
                Err(error)
            }
            Err(_) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": fallback_start.elapsed().as_millis() as u64,
                            "soft_timeout_ms": self.soft_timeout.as_millis() as u64,
                        })),
                    "memory recall fallback also exceeded its budget; recall failed bounded"
                );
                Err(anyhow::anyhow!(
                    "memory recall ({label}) exceeded the soft budget ({:?}) and its fallback budget",
                    self.soft_timeout
                ))
            }
        }
    }
    /// Bound one enumeration attempt with NO fallback: an enumeration has no
    /// recency-fallback semantic — a partial or unrelated listing would
    /// overstate completeness to the caller (the evo SOPs' prefix
    /// enumeration) — so a budget-exhausted attempt (tokio timeout, or a
    /// backend error arriving at/after the budget) FAILS BOUNDED, carrying
    /// the same trace shape as [`Self::bounded`].
    async fn bounded_enumeration<P, PF>(
        &self,
        primary: P,
        label: &str,
    ) -> anyhow::Result<Vec<MemoryEntry>>
    where
        P: FnOnce() -> PF,
        PF: Future<Output = anyhow::Result<Vec<MemoryEntry>>>,
    {
        let start = Instant::now();
        let outcome = tokio::time::timeout(self.soft_timeout, primary()).await;
        let elapsed = start.elapsed();
        match outcome {
            Ok(Ok(entries)) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": elapsed.as_millis() as u64,
                            "count": entries.len(),
                            "degraded": false,
                        })),
                    "memory enumeration completed within the soft budget"
                );
                Ok(entries)
            }
            // A real backend failure that did NOT consume the budget: it
            // propagates as-is.
            Ok(Err(error)) if elapsed < self.soft_timeout => Err(error),
            Ok(Err(error)) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": elapsed.as_millis() as u64,
                            "error": error.to_string(),
                        })),
                    "memory enumeration errored after exhausting the soft budget"
                );
                Err(error)
            }
            Err(_) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "recall": label,
                            "elapsed_ms": elapsed.as_millis() as u64,
                            "soft_timeout_ms": self.soft_timeout.as_millis() as u64,
                        })),
                    "memory enumeration exceeded the soft budget; failed bounded"
                );
                Err(anyhow::anyhow!(
                    "memory enumeration ({label}) exceeded the soft budget ({:?})",
                    self.soft_timeout
                ))
            }
        }
    }
}

impl ::zeroclaw_api::attribution::Attributable for BudgetedMemory {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        self.inner.role()
    }

    fn alias(&self) -> &str {
        self.inner.alias()
    }
}

#[async_trait]
impl Memory for BudgetedMemory {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if self.disabled() {
            return self
                .inner
                .recall(query, limit, session_id, since, until)
                .await;
        }
        self.bounded(
            || self.inner.recall(query, limit, session_id, since, until),
            || self.inner.recall("", limit, session_id, since, until),
            "recall",
        )
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
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if self.disabled() {
            return self
                .inner
                .recall_for_agents(allowed_agent_ids, query, limit, session_id, since, until)
                .await;
        }
        self.bounded(
            || {
                self.inner
                    .recall_for_agents(allowed_agent_ids, query, limit, session_id, since, until)
            },
            || {
                self.inner
                    .recall_for_agents(allowed_agent_ids, "", limit, session_id, since, until)
            },
            "recall_for_agents",
        )
        .await
    }

    async fn recall_for_principal(
        &self,
        scope: &PrincipalScope,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if self.disabled() {
            return self
                .inner
                .recall_for_principal(scope, query, limit, session_id, since, until)
                .await;
        }
        self.bounded(
            || {
                self.inner
                    .recall_for_principal(scope, query, limit, session_id, since, until)
            },
            || {
                self.inner
                    .recall_for_principal(scope, "", limit, session_id, since, until)
            },
            "recall_for_principal",
        )
        .await
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner.store(key, content, category, session_id).await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.inner.get(key).await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.list(category, session_id).await
    }

    /// The prefix enumeration rides the backend's `list_prefixed` under the
    /// same wall-clock budget as the ranked recall — but with NO
    /// recent-entries fallback: an enumeration has no recency-fallback
    /// semantic, so an over-budget enumeration fails bounded rather than
    /// returning a partial or unrelated listing (see
    /// [`Self::bounded_enumeration`]).
    async fn list_prefixed(
        &self,
        prefix: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if self.disabled() {
            return self.inner.list_prefixed(prefix, limit, session_id).await;
        }
        self.bounded_enumeration(
            || self.inner.list_prefixed(prefix, limit, session_id),
            "list_prefixed",
        )
        .await
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(key).await
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> anyhow::Result<bool> {
        self.inner.forget_for_agent(key, agent_id).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
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
    ) -> anyhow::Result<()> {
        self.inner
            .store_with_agent(
                key, content, category, session_id, namespace, importance, agent_id,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn entry(key: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            principal_id: None,
            id: key.into(),
            key: key.into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: "2026-10-27T00:00:00Z".into(),
            session_id: None,
            score: Some(0.9),
            namespace: "default".into(),
            importance: None,
            superseded_by: None,
            kind: None,
            pinned: false,
            tenant_id: None,
            agent_alias: None,
            agent_id: None,
        }
    }

    /// `MemoryEntry` has no `PartialEq` (field set drifts); compare on the
    /// identity pair the recall contract cares about.
    fn shape(entries: &[MemoryEntry]) -> Vec<(&str, &str)> {
        entries
            .iter()
            .map(|e| (e.key.as_str(), e.content.as_str()))
            .collect()
    }

    /// Fake backend: the ranked recall sleeps (the measured prod pathology,
    /// abstracted) while the recent-entries path (empty query) is instant —
    /// the shape the postgres fast path gives the real backend.
    struct SlowPrimaryMemory {
        primary_delay: Duration,
        primary_error: bool,
        fallback_delay: Duration,
        fallback_error: bool,
        ranked: Vec<MemoryEntry>,
        recent: Vec<MemoryEntry>,
        ranked_calls: AtomicUsize,
        recent_calls: AtomicUsize,
        agents_ranked_calls: AtomicUsize,
        agents_recent_calls: AtomicUsize,
    }

    impl SlowPrimaryMemory {
        fn new(primary_delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                primary_delay,
                primary_error: false,
                fallback_delay: Duration::ZERO,
                fallback_error: false,
                ranked: vec![entry("ranked", "ranked result")],
                recent: vec![entry("recent", "recent fallback result")],
                ranked_calls: AtomicUsize::new(0),
                recent_calls: AtomicUsize::new(0),
                agents_ranked_calls: AtomicUsize::new(0),
                agents_recent_calls: AtomicUsize::new(0),
            })
        }

        fn failing_fallback(mut self) -> Self {
            self.fallback_error = true;
            self
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for SlowPrimaryMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "slow-primary"
        }
    }

    #[async_trait]
    impl Memory for SlowPrimaryMemory {
        fn name(&self) -> &str {
            "slow-primary"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            if query.is_empty() {
                self.recent_calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.fallback_delay).await;
                if self.fallback_error {
                    anyhow::bail!("fallback backend down");
                }
                return Ok(self.recent.clone());
            }
            self.ranked_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.primary_delay).await;
            if self.primary_error {
                anyhow::bail!("primary backend down");
            }
            Ok(self.ranked.clone())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        async fn store_with_agent(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.store(key, content, category, session_id).await
        }

        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            if query.is_empty() {
                self.agents_recent_calls.fetch_add(1, Ordering::SeqCst);
            } else {
                self.agents_ranked_calls.fetch_add(1, Ordering::SeqCst);
            }
            self.recall(query, limit, session_id, since, until).await
        }
    }

    const BUDGET: Duration = Duration::from_millis(100);
    /// CI slack over `BUDGET + FALLBACK_BUDGET_FLOOR` for scheduler jitter.
    const BOUNDED_SLACK: Duration = Duration::from_millis(1500);

    #[tokio::test]
    async fn slow_primary_degrades_to_recent_fallback_within_budget() {
        let fake = SlowPrimaryMemory::new(Duration::from_secs(2));
        let mem = BudgetedMemory::new(fake.clone(), BUDGET);
        let start = Instant::now();

        let results = mem.recall("query", 5, None, None, None).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(shape(&results), vec![("recent", "recent fallback result")]);
        assert_eq!(
            fake.recent_calls.load(Ordering::SeqCst),
            1,
            "the recent-entries fallback must run exactly once"
        );
        assert!(
            elapsed < BUDGET + FALLBACK_BUDGET_FLOOR + BOUNDED_SLACK,
            "a recall must stay bounded (soft {BUDGET:?} + fallback floor); took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn fallback_timeout_is_bounded() {
        let mut fake = SlowPrimaryMemory::new(Duration::from_secs(2));
        let fallback_delay = Duration::from_secs(2);
        let mut_inner = Arc::get_mut(&mut fake).expect("single owner");
        mut_inner.fallback_delay = fallback_delay;
        let mem = BudgetedMemory::new(fake, BUDGET);
        let start = Instant::now();

        let result = mem.recall("query", 5, None, None, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "an unbounded primary AND fallback must error");
        assert!(
            elapsed < BUDGET + FALLBACK_BUDGET_FLOOR + BOUNDED_SLACK,
            "the total call must stay bounded even when both attempts stall; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn fast_primary_returns_ranked_results_unchanged() {
        let fake = SlowPrimaryMemory::new(Duration::ZERO);
        let mem = BudgetedMemory::new(fake.clone(), BUDGET);

        let results = mem.recall("query", 5, None, None, None).await.unwrap();

        assert_eq!(shape(&results), vec![("ranked", "ranked result")]);
        assert_eq!(
            fake.recent_calls.load(Ordering::SeqCst),
            0,
            "a primary within budget must never touch the fallback"
        );
    }

    #[tokio::test]
    async fn disabled_budget_is_passthrough() {
        let fake = SlowPrimaryMemory::new(Duration::from_millis(250));
        let mem = BudgetedMemory::new(fake.clone(), Duration::ZERO);
        let start = Instant::now();

        let results = mem.recall("query", 5, None, None, None).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(shape(&results), vec![("ranked", "ranked result")]);
        assert!(
            elapsed >= Duration::from_millis(250),
            "the disabled budget must not interrupt the primary attempt"
        );
        assert_eq!(fake.recent_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn backend_error_within_budget_propagates() {
        let mut fake = SlowPrimaryMemory::new(Duration::ZERO);
        Arc::get_mut(&mut fake)
            .expect("single owner")
            .primary_error = true;
        let mem = BudgetedMemory::new(fake.clone(), BUDGET);

        let result = mem.recall("query", 5, None, None, None).await;

        assert!(result.is_err(), "a fast backend error must propagate as-is");
        assert_eq!(
            fake.recent_calls.load(Ordering::SeqCst),
            0,
            "a real failure before the budget must not be masked by the fallback"
        );
    }

    #[tokio::test]
    async fn recall_for_agents_degrades_to_recency_fallback() {
        let fake = SlowPrimaryMemory::new(Duration::from_secs(2));
        let mem = BudgetedMemory::new(fake.clone(), BUDGET);
        let start = Instant::now();

        let results = mem
            .recall_for_agents(&["agent-1"], "query", 5, None, None, None)
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(shape(&results), vec![("recent", "recent fallback result")]);
        assert_eq!(
            fake.agents_recent_calls.load(Ordering::SeqCst),
            1,
            "the fallback must reuse the same variant with an empty query"
        );
        assert!(
            elapsed < BUDGET + FALLBACK_BUDGET_FLOOR + BOUNDED_SLACK,
            "took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn degrading_fallback_error_surfaces_with_budget_intact() {
        let fake = Arc::try_unwrap(SlowPrimaryMemory::new(Duration::from_secs(2)))
            .ok()
            .expect("single owner")
            .failing_fallback();
        let fake = Arc::new(fake);
        let mem = BudgetedMemory::new(fake, BUDGET);
        let start = Instant::now();

        let result = mem.recall("query", 5, None, None, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "the fallback error must surface");
        assert!(
            elapsed < BUDGET + FALLBACK_BUDGET_FLOOR + BOUNDED_SLACK,
            "took {elapsed:?}"
        );
    }

    #[test]
    fn default_soft_timeout_is_two_seconds() {
        assert_eq!(DEFAULT_RECALL_SOFT_TIMEOUT, Duration::from_millis(2000));
    }

    #[tokio::test]
    async fn decorator_coerces_to_arc_dyn_memory() {
        let inner = SlowPrimaryMemory::new(Duration::ZERO);
        let budgeted = Arc::new(BudgetedMemory::new(inner, BUDGET)) as Arc<dyn Memory>;
        let results = budgeted
            .recall("query", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    /// Fake backend whose `list_prefixed` sleeps (the measured prod
    /// enumeration shape, abstracted) while `list` is instant — so a test
    /// that observes the budget proves the DECORATOR bounds `list_prefixed`
    /// itself, not a default compose through the fast `list`.
    struct SlowPrefixMemory {
        prefix_delay: Duration,
        prefix_error: bool,
        prefix_calls: AtomicUsize,
        list_calls: AtomicUsize,
    }

    impl SlowPrefixMemory {
        fn new(prefix_delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                prefix_delay,
                prefix_error: false,
                prefix_calls: AtomicUsize::new(0),
                list_calls: AtomicUsize::new(0),
            })
        }

        fn prefixed_entry(key: &str) -> MemoryEntry {
            MemoryEntry {
                principal_id: None,
                id: key.into(),
                key: key.into(),
                content: "prefixed enumeration result".into(),
                category: MemoryCategory::Core,
                timestamp: "2026-10-28T00:00:00Z".into(),
                session_id: None,
                score: None,
                namespace: "default".into(),
                importance: None,
                superseded_by: None,
                kind: None,
                pinned: false,
                tenant_id: None,
                agent_alias: None,
                agent_id: None,
            }
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for SlowPrefixMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "slow-prefix"
        }
    }

    #[async_trait]
    impl Memory for SlowPrefixMemory {
        fn name(&self) -> &str {
            "slow-prefix"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn list_prefixed(
            &self,
            _prefix: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.prefix_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.prefix_delay).await;
            if self.prefix_error {
                anyhow::bail!("prefix backend down");
            }
            Ok(vec![Self::prefixed_entry("ws:one"), Self::prefixed_entry("ws:two")])
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        async fn store_with_agent(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.recall(query, limit, session_id, since, until).await
        }
    }

    /// The decorator routes the prefix enumeration through the inner
    /// backend's `list_prefixed` — not the default list-based compose — and
    /// a within-budget enumeration returns the backend's entries unchanged.
    #[tokio::test]
    async fn list_prefixed_within_budget_returns_prefixed_entries() {
        let fake = SlowPrefixMemory::new(Duration::ZERO);
        let mem = BudgetedMemory::new(fake.clone(), BUDGET);

        let results = mem.list_prefixed("ws:", 10, None).await.unwrap();

        assert_eq!(results.len(), 2, "the prefixed entries come through");
        assert_eq!(
            fake.prefix_calls.load(Ordering::SeqCst),
            1,
            "the decorator must call the backend's list_prefixed"
        );
        assert_eq!(
            fake.list_calls.load(Ordering::SeqCst),
            0,
            "the prefix enumeration must not ride the unbounded list compose"
        );
    }

    /// An over-budget enumeration FAILS BOUNDED (no truncated-result
    /// fallback: a partial enumeration would overstate completeness to the
    /// SOP consumer). The call returns an error naming the budget, within
    /// roughly the soft budget.
    #[tokio::test]
    async fn list_prefixed_over_budget_fails_bounded() {
        let fake = SlowPrefixMemory::new(Duration::from_secs(2));
        let mem = BudgetedMemory::new(fake, BUDGET);
        let start = Instant::now();

        let result = mem.list_prefixed("ws:", 10, None).await;
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "an over-budget enumeration must fail bounded, not return a partial listing"
        );
        assert!(
            result
                .err()
                .is_some_and(|e| e.to_string().contains("soft budget")),
            "the bounded failure must name the soft budget"
        );
        assert!(
            elapsed < BUDGET + BOUNDED_SLACK,
            "the enumeration must stay bounded by the soft budget; took {elapsed:?}"
        );
    }

    /// `soft_timeout == 0` disables the budget entirely — the decorator's
    /// disabled contract, mirrored for the enumeration: byte-identical
    /// pass-through.
    #[tokio::test]
    async fn list_prefixed_disabled_budget_is_passthrough() {
        let fake = SlowPrefixMemory::new(Duration::from_millis(250));
        let mem = BudgetedMemory::new(fake, Duration::ZERO);
        let start = Instant::now();

        let results = mem.list_prefixed("ws:", 10, None).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 2);
        assert!(
            elapsed >= Duration::from_millis(250),
            "a disabled budget must not interrupt the enumeration"
        );
    }
}
