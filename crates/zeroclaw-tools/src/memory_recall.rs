use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::{Memory, POSTGRES_RECALL_CANDIDATE_CAP, RecallExcludes};

/// Let the agent search its own memory
/// Render a score as a percentage of the STRONGEST hit in the same result set.
///
/// Raw `ts_rank_cd` values are tiny (~0.01–0.1), so an ABSOLUTE percentage
/// truncates every row to `[0%]` — the ranking the caller asked for is then
/// invisible, and a well-ordered list is indistinguishable from an arbitrary
/// one (prod observation 2026-11-20). Relative display makes the ordering
/// legible: the strongest hit reads 100%, the rest proportionally.
fn format_relative_score(score: Option<f64>, max_score: f64) -> String {
    score.map_or_else(String::new, |s| {
        let pct = if max_score > 0.0 {
            (s / max_score) * 100.0
        } else {
            0.0
        };
        format!(" [{pct:.0}%]")
    })
}

pub struct MemoryRecallTool {
    memory: Arc<dyn Memory>,
    /// Three-axis exclude set sourced from `[memory] exclude_namespaces /
    /// exclude_categories / exclude_key_prefixes`. Applied when the caller
    /// does not opt out via `scope = "all"` (the audit/debug escape hatch).
    excludes: RecallExcludes,
    /// Wall-clock budget for the prefix-recall listing path, derived from
    /// the same `[memory] recall_soft_timeout_ms` knob as the recall-budget
    /// decorator (`0` disables — legacy unbounded listing).
    ///
    /// Why the bound lives HERE and not in [`BudgetedMemory::list`]: the
    /// prefix-recall mode is the one recall-shaped consumer of `list()`,
    /// and the measured prod gap (canary v7: `memory_recall (15220ms)`,
    /// 2026-10-27) was exactly this path — the decorator deliberately
    /// passes `list` through unbounded because `memory_export` (GDPR) and
    /// the RPC memory-listing surface ride the same method and a bulk
    /// listing is their point. Bounding the tool's own use keeps the
    /// cycle-3 contract (a caller-visible recall is bounded ≈ budget;
    /// here: one attempt, no fallback — an enumeration has no recency
    /// fallback semantic, so an over-budget listing fails bounded rather
    /// than returning unrelated recent entries).
    prefix_list_budget: Option<Duration>,
}

impl MemoryRecallTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            excludes: RecallExcludes::default(),
            prefix_list_budget: None,
        }
    }

    /// Construct with the config-sourced three-axis excludes. Used by the
    /// agent runtime where the live `MemoryConfig` is available; the bare
    /// [`new`](Self::new) constructor is kept for tests and config-less
    /// callers (empty excludes → no-op filter).
    pub fn new_with_excludes(memory: Arc<dyn Memory>, excludes: RecallExcludes) -> Self {
        Self {
            memory,
            excludes,
            prefix_list_budget: None,
        }
    }

    /// Construct with the excludes AND the prefix-recall wall budget from
    /// `[memory] recall_soft_timeout_ms` (`0` disables the bound). Every
    /// production construction site must use this constructor — the
    /// legacy constructors exist for tests and config-less callers only
    /// and leave the prefix-recall path unbounded.
    pub fn new_with_excludes_and_budget(
        memory: Arc<dyn Memory>,
        excludes: RecallExcludes,
        recall_soft_timeout_ms: u64,
    ) -> Self {
        Self {
            memory,
            excludes,
            prefix_list_budget: if recall_soft_timeout_ms == 0 {
                None
            } else {
                Some(Duration::from_millis(recall_soft_timeout_ms))
            },
        }
    }

    /// Handle prefix recall mode: list all entries whose key starts with `prefix`.
    async fn handle_prefix_recall(&self, prefix: &str, limit: usize) -> anyhow::Result<ToolResult> {
        // The enumeration rides `list_prefixed` — the prefix-pushed,
        // LIMITed contract — not the unbounded full-store `list` scan the
        // mode used before cycle-5 (measured 15.2s on the prod leader,
        // canary v7). The wall budget stays as the belt-and-braces bound on
        // top of the storage-side LIMIT: a slow backend is still bounded,
        // and an over-budget enumeration fails bounded (an enumeration has
        // no recency-fallback semantic — a partial listing would overstate
        // completeness).
        // The wall-clock belt-and-braces is REMOVED for the serialized
        // shared-client path (2026-11-14, the guide-delta-scan live run): the
        // Postgres backend serializes every memory operation on ONE
        // connection behind a std::sync::Mutex, so the tokio timeout wrapped
        // the MUTEX WAIT — a prior attempt's own budget hold blocked the
        // lock, and the belt killed a 0.3 ms indexed query that had not
        // started. The enforced bound is the SERVER-side
        // statement_timeout inside `list_prefixed`
        // (`with_statement_timeout` — cancelled at the soft budget by the
        // database, freeing the shared connection), and the lock wait is
        // bounded by the prior attempt's `op_timeout`. Both phases are
        // bounded by construction; a second, redundant wall clock that
        // measures the mutex is the defect.
        //
        // A consumer needing a hard wall-clock bound on the whole call
        // (including lock waits) should bound the TURN, not the attempt.
        let listed = self.memory.list_prefixed(prefix, limit, None).await;
        let entries = listed?;
        // `list_prefixed` pushes the prefix filter and the limit into
        // storage; this re-check is a defensive contract pin so the tool's
        // semantics hold regardless of a backend's override fidelity.
        let matched: Vec<_> = entries
            .into_iter()
            .filter(|e| e.key.starts_with(prefix))
            .take(limit)
            .collect();

        if matched.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No memories found.".into(),
                error: None,
            });
        }

        let mut output = format!("Found {} memories:\n", matched.len());
        for entry in &matched {
            let _ = writeln!(
                output,
                "- [{}] {}: {}",
                entry.category, entry.key, entry.content
            );
        }
        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search long-term memory; omit query for recent"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords; omit or '*' for recent memories"
                },
                "limit": {
                    "type": "integer"
                },
                "since": {
                    "type": "string",
                    "description": "RFC 3339 timestamp"
                },
                "until": {
                    "type": "string",
                    "description": "RFC 3339 timestamp"
                },
                "search_mode": {
                    "type": "string",
                    "enum": ["bm25", "embedding", "hybrid"]
                },
                "category": {
                    "type": "string",
                    "description": "core, workstream, daily, conversation, or custom"
                },
                "namespace": {
                    "type": "string"
                },
                "key_prefix": {
                    "type": "string"
                },
                "scope": {
                    "type": "string",
                    "enum": ["filtered", "all"],
                    "description": "filtered=default exclusions; all=no exclusions"
                },
                "prefix": {
                    "type": "string",
                    "description": "enumerate all keys with this prefix; ignores query/filters"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let since = args.get("since").and_then(|v| v.as_str());
        let until = args.get("until").and_then(|v| v.as_str());

        // Validate date strings
        if let Some(s) = since
            && chrono::DateTime::parse_from_rfc3339(s).is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid 'since' date: {s}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
            });
        }
        if let Some(u) = until
            && chrono::DateTime::parse_from_rfc3339(u).is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid 'until' date: {u}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
            });
        }
        if let (Some(s), Some(u)) = (since, until)
            && let (Ok(s_dt), Ok(u_dt)) = (
                chrono::DateTime::parse_from_rfc3339(s),
                chrono::DateTime::parse_from_rfc3339(u),
            )
            && s_dt >= u_dt
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'since' must be before 'until'".into()),
            });
        }

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5, |v| v as usize)
            // Clamp to the ranked recall's candidate ceiling BEFORE the
            // backend call: the backend's candidate stage caps at
            // POSTGRES_RECALL_CANDIDATE_CAP, and the downstream ×2 fetch
            // widening (recall_filtered) plus AgentScopedMemory's second
            // doubling multiply whatever passes through here — an unclamped
            // model-authored `limit: 100000` would reopen the seconds-scale
            // ranked-recall regression.
            .min(POSTGRES_RECALL_CANDIDATE_CAP);

        // Inclusion filters (category / namespace / key_prefix) are applied
        // CLIENT-SIDE after the backend call, so asking the backend for
        // exactly `limit` rows starves any filter over a sparse category: the
        // 2026-11-20 prod measurement had `category=core` return 3 rows at
        // limit=10 and 5 at limit=30 in a store holding 2,408 core rows,
        // because recency is dominated by `sop` ledger writes. Widening the
        // fetch before filtering restores `limit` as a floor on what the
        // caller asked for, and the post-filter truncates back to `limit`.
        //
        // Bounded by the ranked-recall candidate ceiling: the widening must
        // never reopen the seconds-scale ranked-recall regression.
        let has_inclusion_filter = args
            .get("category")
            .and_then(|v| v.as_str())
            .is_some()
            || args
                .get("namespace")
                .and_then(|v| v.as_str())
                .is_some()
            || args
                .get("key_prefix")
                .and_then(|v| v.as_str())
                .is_some();
        let fetch_limit = if has_inclusion_filter {
            limit.saturating_mul(20).min(POSTGRES_RECALL_CANDIDATE_CAP)
        } else {
            limit
        };

        // `search_mode` is a SCHEMA-ONLY parameter today: the postgres backend
        // has no vector index (0 of 91,698 prod rows carry an embedding), so
        // embedding/hybrid silently execute the same keyword path as bm25.
        // A caller that asked for similarity and got keyword results cannot
        // tell the difference — the 2026-11-20 probe read that silence as
        // "the mode returned rows", which is true and misleading. Say it
        // instead: results are unchanged, the fallback is disclosed.
        let requested_mode = args
            .get("search_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("bm25");
        let mode_note = if requested_mode.eq_ignore_ascii_case("bm25") {
            String::new()
        } else {
            format!(
                "\nNote: search_mode=\"{requested_mode}\" fell back to keyword (bm25) search — \
                 this store has no vector index, so these results are keyword-ranked, not \
                 similarity-ranked."
            )
        };

        // Prefix mode: list all entries whose key starts with the given prefix
        if let Some(prefix) = args.get("prefix").and_then(|v| v.as_str()) {
            let prefix = prefix.trim();
            if prefix.is_empty() {
                return Ok(ToolResult {
                    success: true,
                    output: "No memories found.".into(),
                    error: None,
                });
            }
            return self.handle_prefix_recall(prefix, limit).await;
        }

        // `scope = "all"` is the audit/debug escape hatch: skip the
        // config-sourced excludes and recall verbatim. Default ("filtered")
        // applies the three-axis excludes via `recall_filtered`.
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("filtered");
        let skip_excludes = scope.eq_ignore_ascii_case("all");
        // `scope="all"` states its intent as an EXPLICITLY EMPTY exclusion set,
        // never as a bare `recall`. Bare `recall` used to mean "no excludes by
        // construction", but the operator's axes are the store's default since
        // v20, so the hatch silently started excluding — measured on prod
        // 2026-11-21: `category="sop", scope="all"` returned "No memories
        // found" while `prefix="sop_run_"` returned those same rows. Saying
        // "exclude nothing" out loud cannot be re-broken by whichever default
        // a backend adopts next.
        let excludes = if skip_excludes {
            RecallExcludes::default()
        } else {
            self.excludes.clone()
        };
        let recall_result = self
            .memory
            .recall_filtered(query, fetch_limit, None, since, until, &excludes)
            .await;

        match recall_result {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: format!("No memories found.{mode_note}").into(),
                error: None,
            }),
            Ok(entries) => {
                // Three-axis inclusion filtering (category + namespace + key_prefix)
                let category_f = args.get("category").and_then(|v| v.as_str());
                let namespace_f = args.get("namespace").and_then(|v| v.as_str());
                let key_prefix_f = args.get("key_prefix").and_then(|v| v.as_str());
                let entries: Vec<_> = entries
                    .into_iter()
                    .filter(|e| {
                        category_f.map_or(true, |c| {
                            e.category.to_string().to_lowercase() == c.to_lowercase()
                        })
                    })
                    .filter(|e| namespace_f.map_or(true, |ns| e.namespace == ns))
                    .filter(|e| key_prefix_f.map_or(true, |kp| e.key.starts_with(kp)))
                    .take(limit)
                    .collect();
                if entries.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: "No memories found.".into(),
                        error: None,
                    });
                }
                let max_score = entries
                    .iter()
                    .filter_map(|e| e.score)
                    .fold(0.0_f64, f64::max);
                let mut output = format!("Found {} memories:\n", entries.len());
                for entry in &entries {
                    let score = format_relative_score(entry.score, max_score);
                    let _ = writeln!(
                        output,
                        "- [{}] {}: {}{score}",
                        entry.category, entry.key, entry.content
                    );
                }
                output.push_str(&mode_note);
                Ok(ToolResult {
                    success: true,
                    output: output.into(),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Memory recall failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use zeroclaw_memory::{MemoryCategory, MemoryEntry, SqliteMemory, is_recent_recall_query};

    fn seeded_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    /// Fake backend whose `list` sleeps (the measured prod pathology: the
    /// prefix-recall path rode the postgres `list` — a no-LIMIT full-table
    /// scan — 15.2s measured on canary v7). The fake has no `list_prefixed`
    /// override, so the tool's call composes the default through this slow
    /// `list`: the test proves the LIST budget bounds the enumeration even
    /// for a backend that never got the native override. The ranked
    /// `recall` stays instant so a test that times out proves the LIST
    /// budget, not the recall budget, is doing the bounding.
    struct SlowListMemory {
        list_delay: Duration,
        list_error: bool,
    }

    impl SlowListMemory {
        fn new(list_delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                list_delay,
                list_error: false,
            })
        }
    }

    fn prefixed_entry() -> MemoryEntry {
        MemoryEntry {
            principal_id: None,
            id: "p:1".into(),
            key: "p:1".into(),
            content: "prefixed entry".into(),
            category: MemoryCategory::Core,
            timestamp: "2026-10-27T00:00:00Z".into(),
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

    #[async_trait]
    impl Memory for SlowListMemory {
        fn name(&self) -> &str {
            "slow-list"
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
            tokio::time::sleep(self.list_delay).await;
            if self.list_error {
                anyhow::bail!("list backend down");
            }
            Ok(vec![prefixed_entry()])
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
    impl ::zeroclaw_api::attribution::Attributable for SlowListMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "SlowListMemory"
        }
    }

    /// Control (legacy constructors stay unbounded): with no budget the same
    /// slow listing runs to completion — this documents the mechanism the
    /// budget closes and guards the legacy byte-identical contract.
    #[tokio::test]
    async fn legacy_constructor_prefix_recall_has_no_budget() {
        let mem = SlowListMemory::new(Duration::from_millis(400));
        let tool = MemoryRecallTool::new_with_excludes(mem, RecallExcludes::default());
        let start = Instant::now();
        let result = tool.execute(json!({"prefix": "p:"})).await.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "the legacy constructor must not bound the listing"
        );
        assert!(result.success);
        assert!(result.output.contains("prefixed entry"));
    }

    /// The measured prod gap (canary v7, `memory_recall (15220ms)`): the
    /// prefix-recall mode rode `list()` — which the recall-budget decorator
    /// passes through unbounded — so a full-enumeration recall stalled the
    /// tool past the recall budget's ≈3s contract. The tool owns the bound
    /// for its recall-shaped use of `list`: an over-budget listing fails
    /// bounded (no fake fallback data for an enumeration).
    /// `recall_soft_timeout_ms`'s prefix-path wall-clock belt was REMOVED
    /// (2026-11-14, the guide-delta-scan live run): the Postgres backend
    /// serializes memory operations on ONE shared client behind a mutex, so
    /// the tokio timeout wrapped the MUTEX WAIT — a prior attempt's own
    /// budget hold blocked the lock and the belt killed a 0.3 ms indexed
    /// query that had not started. The enforced bound is the backend's own
    /// server-side `statement_timeout` (postgres.rs
    /// `with_statement_timeout`); the tool passes the backend's latency
    /// through rather than layering a second wall clock that measures the
    /// mutex.
    #[tokio::test]
    async fn prefix_recall_surfaces_backend_latency_without_a_second_belt() {
        let mem = SlowListMemory::new(Duration::from_secs(5));
        let tool = MemoryRecallTool::new_with_excludes_and_budget(
            mem,
            RecallExcludes::default(),
            100, // recall_soft_timeout_ms — no longer bounds the prefix path
        );
        let result = tool.execute(json!({"prefix": "p:"})).await.unwrap();

        assert!(
            result.success,
            "a slow backend's latency surfaces honestly: no second belt              kills a slow-but-honest call (the backend's own bound governs)"
        );
    }

    /// `recall_soft_timeout_ms = 0` disables the budget entirely — the
    /// decorator's disabled contract, mirrored here: the legacy unbounded
    /// listing passes through untouched.
    #[tokio::test]
    async fn prefix_recall_budget_zero_is_legacy_unbounded() {
        let mem = SlowListMemory::new(Duration::from_millis(400));
        let tool =
            MemoryRecallTool::new_with_excludes_and_budget(mem, RecallExcludes::default(), 0);
        let start = Instant::now();
        let result = tool.execute(json!({"prefix": "p:"})).await.unwrap();

        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "a disabled budget must not interrupt the listing"
        );
        assert!(result.success);
        assert!(result.output.contains("prefixed entry"));
    }

    /// A listing within the budget returns entries unchanged — the bound
    /// must not degrade the healthy path.
    #[tokio::test]
    async fn prefix_recall_within_budget_returns_entries() {
        let mem = SlowListMemory::new(Duration::from_millis(50));
        let tool = MemoryRecallTool::new_with_excludes_and_budget(
            mem,
            RecallExcludes::default(),
            2000,
        );
        let result = tool.execute(json!({"prefix": "p:"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("prefixed entry"));
    }

    /// Fake backend distinguishing `list` from `list_prefixed` so the tool's
    /// routing is observable: the prefix mode must ride `list_prefixed` — the
    /// prefix-pushed, LIMITed contract — not the unbounded full-store `list`
    /// scan (measured 15.2s on the prod leader's 50K-row postgres store,
    /// canary v7, before the evo SOP's enumeration was bounded-failed).
    struct PrefixRoutedMemory {
        list_calls: std::sync::atomic::AtomicUsize,
        list_prefixed_calls: std::sync::atomic::AtomicUsize,
        last_prefix_limit: Mutex<Option<usize>>,
    }

    impl PrefixRoutedMemory {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                list_calls: std::sync::atomic::AtomicUsize::new(0),
                list_prefixed_calls: std::sync::atomic::AtomicUsize::new(0),
                last_prefix_limit: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl Memory for PrefixRoutedMemory {
        fn name(&self) -> &str {
            "prefix-routed"
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
            self.list_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![MemoryEntry {
                principal_id: None,
                id: "list-scan:1".into(),
                key: "p:1".into(),
                content: "list-scan entry".into(),
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
            }])
        }

        async fn list_prefixed(
            &self,
            _prefix: &str,
            limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.list_prefixed_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.last_prefix_limit.lock().unwrap() = Some(limit);
            Ok(vec![MemoryEntry {
                principal_id: None,
                id: "prefixed:1".into(),
                key: "p:1".into(),
                content: "prefixed entry".into(),
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
            }])
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
    impl ::zeroclaw_api::attribution::Attributable for PrefixRoutedMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "PrefixRoutedMemory"
        }
    }

    /// The prefix-recall mode must route through `list_prefixed` — the
    /// bounded, prefix-pushed enumeration contract — and must NOT ride the
    /// unbounded full-store `list` scan the mode used before cycle-5.
    #[tokio::test]
    async fn prefix_recall_routes_through_list_prefixed() {
        let mem = PrefixRoutedMemory::new();
        let tool = MemoryRecallTool::new(mem.clone());

        let result = tool.execute(json!({"prefix": "p:"})).await.unwrap();

        assert!(result.success);
        assert!(
            result.output.contains("prefixed entry"),
            "the prefix mode must read the list_prefixed results; got {}",
            result.output
        );
        assert_eq!(
            mem.list_prefixed_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "prefix mode must call list_prefixed"
        );
        assert_eq!(
            mem.list_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "prefix mode must not ride the unbounded full-store list scan"
        );
    }

    /// The tool's `limit` must reach `list_prefixed` so a backend pushes the
    /// bound into storage instead of truncating after a full scan.
    #[tokio::test]
    async fn prefix_recall_threads_limit_into_list_prefixed() {
        let mem = PrefixRoutedMemory::new();
        let tool = MemoryRecallTool::new(mem.clone());

        let result = tool
            .execute(json!({"prefix": "p:", "limit": 7}))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(
            mem.list_prefixed_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
        );
        assert_eq!(*mem.last_prefix_limit.lock().unwrap(), Some(7));
    }

    /// Fake backend capturing the limit the tool hands the memory stack, so
    /// the candidate-cap clamp at the parse site is observable without a
    /// live backend. Overriding `recall_filtered` captures the limit BEFORE
    /// the trait default's ×2 fetch widening (and AgentScopedMemory's second
    /// doubling), which multiply whatever passes through here.
    struct LimitCaptureMemory {
        last_limit: Mutex<Option<usize>>,
    }

    impl LimitCaptureMemory {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                last_limit: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl Memory for LimitCaptureMemory {
        fn name(&self) -> &str {
            "limit-capture"
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
            Ok(Vec::new())
        }

        async fn list_prefixed(
            &self,
            _prefix: &str,
            _limit: usize,
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

        async fn recall_filtered(
            &self,
            _query: &str,
            limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
            _excludes: &RecallExcludes,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            *self.last_limit.lock().unwrap() = Some(limit);
            Ok(Vec::new())
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
    impl ::zeroclaw_api::attribution::Attributable for LimitCaptureMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "LimitCaptureMemory"
        }
    }

    /// A model-authored `limit` reaching the tool must clamp to the ranked
    /// recall's candidate ceiling BEFORE the backend call: the backend's
    /// candidate stage caps at 2000, and the downstream ×2 fetch widening
    /// (`recall_filtered`) plus AgentScopedMemory's second doubling multiply
    /// whatever passes through here — an unclamped `limit: 100000` ask would
    /// reopen the seconds-scale ranked-recall regression.
    #[tokio::test]
    async fn memory_recall_limit_is_clamped_to_the_candidate_cap() {
        let mem = LimitCaptureMemory::new();
        let tool = MemoryRecallTool::new(mem.clone());

        tool.execute(json!({"query": "needle", "limit": 100000}))
            .await
            .unwrap();
        assert_eq!(
            *mem.last_limit.lock().unwrap(),
            Some(POSTGRES_RECALL_CANDIDATE_CAP),
            "a model-authored limit above the candidate ceiling must clamp before the backend call"
        );

        tool.execute(json!({"query": "needle", "limit": 7}))
            .await
            .unwrap();
        assert_eq!(
            *mem.last_limit.lock().unwrap(),
            Some(7),
            "a limit within the ceiling passes through unchanged"
        );
    }

    struct QueryEchoMemory {
        last_query: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl Memory for QueryEchoMemory {
        fn name(&self) -> &str {
            "query_echo"
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
            *self.last_query.lock().unwrap() = Some(query.to_string());
            if is_recent_recall_query(query) {
                Ok(vec![MemoryEntry {
                    principal_id: None,
                    id: "recent".into(),
                    key: "recent".into(),
                    content: "recent memory".into(),
                    category: MemoryCategory::Core,
                    timestamp: "2026-05-03T00:00:00Z".into(),
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
                }])
            } else {
                Ok(Vec::new())
            }
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
    impl ::zeroclaw_api::attribution::Attributable for QueryEchoMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "QueryEchoMemory"
        }
    }

    #[tokio::test]
    async fn recall_empty() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"query": "anything"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memories found"));
    }

    #[tokio::test]
    async fn recall_finds_match() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("tz", "Timezone is EST", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"query": "Rust"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(result.output.contains("Found 1"));
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let (_tmp, mem) = seeded_mem();
        for i in 0..10 {
            mem.store(
                &format!("k{i}"),
                &format!("Rust fact {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"query": "Rust", "limit": 3}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 3"));
    }

    #[tokio::test]
    async fn bare_recall_returns_recent_entries() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 1"));
        assert!(result.output.contains("Rust"));
    }

    #[tokio::test]
    async fn recall_star_query_returns_recent_entries() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("tz", "Timezone is EST", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"query": "*"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 2"));
        assert!(result.output.contains("Rust"));
        assert!(result.output.contains("EST"));
    }

    #[tokio::test]
    async fn recall_star_query_uses_backend_recent_query_contract() {
        let last_query = Arc::new(Mutex::new(None));
        let mem = Arc::new(QueryEchoMemory {
            last_query: last_query.clone(),
        });
        let tool = MemoryRecallTool::new(mem);

        let result = tool.execute(json!({"query": "*"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("recent memory"));
        assert_eq!(*last_query.lock().unwrap(), Some("*".into()));
    }

    #[tokio::test]
    async fn recall_time_only_returns_entries() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryRecallTool::new(mem);
        // Time-only: since far in past
        let result = tool
            .execute(json!({"since": "2020-01-01T00:00:00Z", "limit": 5}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 1"));
        assert!(result.output.contains("Rust"));
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        assert_eq!(tool.name(), "memory_recall");
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
    }

    #[tokio::test]
    async fn inclusion_filters_widen_the_fetch_before_filtering() {
        // category/namespace/key_prefix are applied client-side, so asking the
        // backend for exactly `limit` rows starves sparse categories (prod:
        // 3 core rows at limit=10 in a store holding 2,408). The fetch must
        // widen, and the post-filter must truncate back to `limit`.
        let mem = LimitCaptureMemory::new();
        let tool = MemoryRecallTool::new(mem.clone());
        let _ = tool
            .execute(json!({"query": "anything", "limit": 3, "category": "core"}))
            .await
            .unwrap();
        assert_eq!(
            *mem.last_limit.lock().unwrap(),
            Some(60),
            "a category filter must widen the backend fetch (3 x 20)"
        );
    }

    #[tokio::test]
    async fn non_bm25_mode_discloses_the_keyword_fallback() {
        // embedding/hybrid have no vector index behind them, so they execute
        // the keyword path. Returning results without saying so is the quiet
        // lie the 2026-11-20 probe caught; the note makes the fallback legible.
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"query": "*", "limit": 5, "search_mode": "hybrid"}))
            .await
            .unwrap();
        assert!(
            result.output.contains("fell back to keyword"),
            "a non-bm25 mode must disclose the fallback: {}",
            result.output
        );
    }

    #[test]
    fn score_formatted_as_percent() {
        // Relative to the strongest hit: raw ts_rank_cd is tiny, so an
        // absolute percentage showed every row as [0%].
        assert_eq!(format_relative_score(Some(0.05), 0.05), " [100%]");
        assert_eq!(format_relative_score(Some(0.025), 0.05), " [50%]");
        assert_eq!(format_relative_score(Some(0.0), 0.05), " [0%]");
        assert_eq!(format_relative_score(None, 0.05), "");
        assert_eq!(format_relative_score(Some(0.0), 0.0), " [0%]");

        let score: Option<f64> = None;
        let formatted = score.map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
        assert_eq!(formatted, "");
    }

    #[tokio::test]
    async fn recall_prefix_returns_only_matching_keys() {
        let (_tmp, mem) = seeded_mem();
        mem.store("proj:a:1", "first", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("proj:a:2", "second", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("proj:b:1", "third", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"prefix": "proj:a:"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("proj:a:1"));
        assert!(result.output.contains("proj:a:2"));
        assert!(!result.output.contains("proj:b:1"));
    }

    #[tokio::test]
    async fn recall_prefix_respects_limit() {
        let (_tmp, mem) = seeded_mem();
        for i in 0..5 {
            mem.store(
                &format!("proj:a:{i}"),
                &format!("entry {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"prefix": "proj:a:", "limit": 2}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 2"));
    }

    #[tokio::test]
    async fn recall_prefix_empty_returns_nothing() {
        let (_tmp, mem) = seeded_mem();
        mem.store("some_key", "value", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"prefix": "nonexistent:"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memories found"));
    }

    #[test]
    fn schema_includes_search_mode_parameter() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        let schema = tool.parameters_schema();
        let search_mode = &schema["properties"]["search_mode"];
        assert_eq!(search_mode["type"], "string");
        let enum_values = search_mode["enum"].as_array().unwrap();
        assert_eq!(enum_values.len(), 3);
        assert!(enum_values.contains(&json!("bm25")));
        assert!(enum_values.contains(&json!("embedding")));
        assert!(enum_values.contains(&json!("hybrid")));
    }

    #[test]
    fn eager_schema_is_slim() {
        // `memory_recall` is in the runtime's EAGER_BUILTINS: its spec rides
        // the wire on every call. The slimmed eager wire keeps the SHAPE
        // (names/types/enums/required) plus one-liner descriptions only where
        // the semantic is not inferable from the name alone (the bare-`*`
        // recent-recall contract, the RFC 3339 format, the scope semantics);
        // the discovery prose lives in TOOLS.md.
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        assert!(
            tool.description().chars().count() <= 60,
            "eager tool description must stay a short clause: {}",
            tool.description()
        );
        let schema = tool.parameters_schema();
        let wire = serde_json::to_string(&schema).unwrap();
        assert!(
            wire.len() <= 768,
            "eager schema wire must stay under 768 bytes, got {}: {wire}",
            wire.len()
        );
        // Shape snapshot: the full property set keeps its types.
        let props = schema["properties"].as_object().expect("properties object");
        let expected: &[(&str, &str)] = &[
            ("query", "string"),
            ("limit", "integer"),
            ("since", "string"),
            ("until", "string"),
            ("search_mode", "string"),
            ("category", "string"),
            ("namespace", "string"),
            ("key_prefix", "string"),
            ("scope", "string"),
            ("prefix", "string"),
        ];
        assert_eq!(props.len(), expected.len(), "property set must not drift");
        for (name, ty) in expected {
            assert_eq!(
                props[*name]["type"].as_str(),
                Some(*ty),
                "property `{name}` must keep its type on the eager wire"
            );
        }
        // The semantic enums stay — they are shape, not prose.
        let search_mode_enum = schema["properties"]["search_mode"]["enum"]
            .as_array()
            .expect("search_mode enum");
        assert_eq!(search_mode_enum.len(), 3);
        let scope_enum = schema["properties"]["scope"]["enum"]
            .as_array()
            .expect("scope enum");
        assert_eq!(scope_enum.len(), 2);
        // One-liners only; no defaults/examples on the wire.
        for (name, prop) in props {
            if let Some(desc) = prop.get("description").and_then(|d| d.as_str()) {
                assert!(
                    desc.chars().count() <= 64,
                    "property `{name}` description must stay a one-liner: {desc}"
                );
            }
            assert!(
                prop.get("default").is_none(),
                "property `{name}` must not carry a default on the eager wire"
            );
            assert!(
                prop.get("examples").is_none(),
                "property `{name}` must not carry examples on the eager wire"
            );
        }
    }

    #[tokio::test]
    async fn filtered_scope_applies_config_excludes() {
        let (_tmp, mem) = seeded_mem();
        // telemetry entry should be excluded by namespace prefix "telemetry".
        mem.store_with_metadata(
            "sop:1",
            "SOP runbook",
            MemoryCategory::Custom("sop".into()),
            None,
            Some("telemetry_sop"),
            None,
        )
        .await
        .unwrap();
        mem.store("core:1", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let excludes = RecallExcludes {
            namespaces: vec!["telemetry".into()],
            ..Default::default()
        };
        let tool = MemoryRecallTool::new_with_excludes(mem, excludes);
        // Default scope is "filtered" → telemetry_sop entry is dropped.
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(!result.output.contains("SOP runbook"));
    }

    /// Records WHICH recall entry point the tool uses and with what excludes.
    ///
    /// The sqlite-backed hatch test kept passing after v20, because a bare
    /// `recall` on sqlite carries no default excludes — the deployment did.
    /// This pins the CALL SHAPE instead, so the hatch cannot regress on any
    /// backend: `scope="all"` must arrive as `recall_filtered` with an EMPTY
    /// set, never as a bare `recall`.
    struct ExcludesRecordingMemory {
        recorded: Arc<Mutex<Option<RecallExcludes>>>,
        bare_recall_called: Arc<Mutex<bool>>,
    }

    fn recorded_entry(id: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            principal_id: None,
            id: id.into(),
            key: id.into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: "2026-05-03T00:00:00Z".into(),
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

    #[async_trait]
    impl Memory for ExcludesRecordingMemory {
        fn name(&self) -> &str {
            "excludes_recording"
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
            *self.bare_recall_called.lock().unwrap() = true;
            Ok(vec![recorded_entry("bare", "bare recall row")])
        }

        async fn recall_filtered(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
            excludes: &RecallExcludes,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            *self.recorded.lock().unwrap() = Some(excludes.clone());
            Ok(vec![recorded_entry("filtered", "filtered recall row")])
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

    impl ::zeroclaw_api::attribution::Attributable for ExcludesRecordingMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "ExcludesRecordingMemory"
        }
    }

    fn recording_tool(
        configured: RecallExcludes,
    ) -> (
        MemoryRecallTool,
        Arc<Mutex<Option<RecallExcludes>>>,
        Arc<Mutex<bool>>,
    ) {
        let recorded = Arc::new(Mutex::new(None));
        let bare = Arc::new(Mutex::new(false));
        let mem = Arc::new(ExcludesRecordingMemory {
            recorded: Arc::clone(&recorded),
            bare_recall_called: Arc::clone(&bare),
        });
        (
            MemoryRecallTool::new_with_excludes(mem, configured),
            recorded,
            bare,
        )
    }

    /// The prod defect, 2026-11-21: `category="sop", scope="all"` returned
    /// "No memories found" while `prefix="sop_run_"` returned those same rows,
    /// because the hatch called bare `recall` — which inherits the store's
    /// operator axes since v20.
    #[tokio::test]
    async fn scope_all_sends_an_explicitly_empty_exclude_set() {
        let configured = RecallExcludes {
            namespaces: vec!["telemetry".into()],
            categories: vec!["sop".into()],
            key_prefixes: vec!["sop_run_".into()],
        };
        let (tool, recorded, bare) = recording_tool(configured);
        let result = tool.execute(json!({"scope": "all"})).await.unwrap();
        assert!(result.success);

        let seen = recorded
            .lock()
            .unwrap()
            .clone()
            .expect("recall_filtered must be called");
        assert!(
            seen.namespaces.is_empty() && seen.categories.is_empty() && seen.key_prefixes.is_empty(),
            "scope=all must state 'exclude nothing', got: {seen:?}"
        );
        assert!(
            !*bare.lock().unwrap(),
            "scope=all must never fall back to a bare recall — that is how it inherited the store defaults"
        );
    }

    /// The default scope still carries the configured axes (unchanged).
    #[tokio::test]
    async fn default_scope_still_carries_the_configured_excludes() {
        let configured = RecallExcludes {
            namespaces: vec!["telemetry".into()],
            categories: vec!["sop".into()],
            key_prefixes: Vec::new(),
        };
        let (tool, recorded, _bare) = recording_tool(configured.clone());
        tool.execute(json!({})).await.unwrap();
        let seen = recorded
            .lock()
            .unwrap()
            .clone()
            .expect("recall_filtered must be called");
        assert_eq!(seen.namespaces, configured.namespaces);
        assert_eq!(seen.categories, configured.categories);
    }

    #[tokio::test]
    async fn scope_all_bypasses_config_excludes() {
        let (_tmp, mem) = seeded_mem();
        mem.store_with_metadata(
            "sop:1",
            "SOP runbook",
            MemoryCategory::Custom("sop".into()),
            None,
            Some("telemetry_sop"),
            None,
        )
        .await
        .unwrap();
        mem.store("core:1", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let excludes = RecallExcludes {
            namespaces: vec!["telemetry".into()],
            ..Default::default()
        };
        let tool = MemoryRecallTool::new_with_excludes(mem, excludes);
        // scope=all is the audit escape hatch: excludes are NOT applied.
        let result = tool.execute(json!({"scope": "all"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(
            result.output.contains("SOP runbook"),
            "scope=all must return the excluded entry for audit/debug"
        );
    }

    #[tokio::test]
    async fn filtered_scope_category_exclude_exact_match() {
        let (_tmp, mem) = seeded_mem();
        mem.store(
            "sop:1",
            "SOP runbook",
            MemoryCategory::Custom("sop".into()),
            None,
        )
        .await
        .unwrap();
        mem.store("core:1", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let excludes = RecallExcludes {
            categories: vec!["sop".into()],
            ..Default::default()
        };
        let tool = MemoryRecallTool::new_with_excludes(mem, excludes);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(!result.output.contains("SOP runbook"));
    }

    /// The tool-plane session-scope guard (the fusion session-scope program,
    /// Candidate B's pins, 2026-11-18): the `memory_recall` tool is
    /// SESSION-LESS — its execute passes `session_id = None` to the backend
    /// on BOTH call paths (the filtered default and the `scope = "all"`
    /// escape hatch), and its schema exposes no `session_id` parameter.
    /// `session_id` is a conversation-scope axis set by the wrapper planes,
    /// never a model-authored tool argument.
    struct SessionRecordingMemory {
        recorded: Mutex<Vec<Option<String>>>,
    }

    impl SessionRecordingMemory {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Memory for SessionRecordingMemory {
        fn name(&self) -> &str {
            "session-recording"
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
            session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.recorded
                .lock()
                .unwrap()
                .push(session_id.map(String::from));
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
    impl ::zeroclaw_api::attribution::Attributable for SessionRecordingMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "SessionRecordingMemory"
        }
    }

    #[tokio::test]
    async fn recall_execute_passes_session_none_to_the_backend_on_both_scopes() {
        let mem = SessionRecordingMemory::new();
        let tool = MemoryRecallTool::new_with_excludes(mem.clone(), RecallExcludes::default());

        // The filtered default path (recall_filtered)...
        tool.execute(json!({"query": "anything"})).await.unwrap();
        // ...and the scope=all escape hatch (recall) — both session-less.
        tool.execute(json!({"query": "anything", "scope": "all"}))
            .await
            .unwrap();

        let recorded = mem.recorded.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![None, None],
            "the recall tool must pass session_id=None to the backend on both call paths"
        );
    }

    #[test]
    fn recall_tool_schema_hides_session_scope() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        assert!(
            tool.parameters_schema()["properties"]
                .get("session_id")
                .is_none(),
            "the tool plane is session-less — the schema must not expose a session_id parameter, got: {}",
            tool.parameters_schema()
        );
    }
}
