//! Pre-call partition: per-step elapsed-ms accumulators for the span the
//! `pre_call_ms` attr measures — engine entry (`run_tool_call_loop_impl`) to
//! the first provider send. Each mark pair is two `Instant::now` calls around
//! one step; nothing measured is copied or serialized. The residual
//! (`pre_other_ms`) keeps the family reconciling to the row's `pre_call_ms`.

use std::time::Duration;

/// The `pre_*_ms` attr family emitted on the iteration-0 `llm_request` row,
/// in bucket order. `pre_other_ms` is last: it is the row's residual, not a
/// marked bucket. Test-side single home: the trace-emission tests pin the
/// family's presence by this list, so a bucket rename or reorder fails them
/// here first. (`emit_into` writes the family; [`MARKED_KEYS`] carries the
/// production-side bucket order.)
#[cfg(test)]
pub(crate) const PRE_CALL_PARTITION_KEYS: [&str; 14] = [
    "pre_loop_setup_ms",
    "pre_memory_ms",
    "pre_pacing_ms",
    "pre_step_scope_ms",
    "pre_resolve_ms",
    "pre_tool_specs_ms",
    "pre_assemble_ms",
    "pre_hooks_ms",
    "pre_estimate_ms",
    "pre_trim_ms",
    "pre_preflight_ms",
    "pre_announce_ms",
    "pre_fingerprint_ms",
    "pre_other_ms",
];

/// The thirteen marked buckets in family order (`pre_other_ms` excluded).
const MARKED_KEYS: [&str; 13] = [
    "pre_loop_setup_ms",
    "pre_memory_ms",
    "pre_pacing_ms",
    "pre_step_scope_ms",
    "pre_resolve_ms",
    "pre_tool_specs_ms",
    "pre_assemble_ms",
    "pre_hooks_ms",
    "pre_estimate_ms",
    "pre_trim_ms",
    "pre_preflight_ms",
    "pre_announce_ms",
    "pre_fingerprint_ms",
];

/// Accumulated pre-call step timings for one tool-call loop run.
///
/// Marks run on every iteration (each pair is nanoseconds of overhead) and
/// accumulate across them, but the family is emitted only on the iteration-0
/// `llm_request` row — the row whose `pre_call_ms` IS the turn's pre-call
/// latency (the 2026-10-28 prod capture's unpartitioned 3.19s segment). On
/// that row `pre_other_ms` closes the gap to the total exactly:
/// `sum(named) + pre_other_ms == pre_call_ms` (saturating at the
/// millisecond-rounding edge, where the named sum can exceed the total by 1).
#[derive(Debug, Default)]
pub(crate) struct PreCallTimings {
    /// Loop setup: the `ToolLoop`/`ResolvedAgentExecution` destructures, the
    /// TurnState construction and sync, the ingress-policy decision and the
    /// internal-principal stamp — engine entry through the memory mark.
    loop_setup_ms: u64,
    /// Memory recall/inject (`render_memory_context`).
    memory_ms: u64,
    /// Loop-head setup between the memory mark and the iteration loop: the
    /// max-iterations resolution, the loop-ignore tool set, the loop-detector
    /// config, the display/guard loop locals, the shared `TurnCtx` (base_ctx),
    /// the SOP step-exec cache, and the reported-usage seed.
    pacing_ms: u64,
    /// The per-iteration head ahead of the route resolution: step-scope tool
    /// exclusions, the steering yield/drain, the cancellation check, the
    /// shared-budget reservation, preflight history maintenance, and the
    /// pending-model-switch check.
    step_scope_ms: u64,
    /// Route/model resolution: vision-route provider resolution and the
    /// per-call context-limits resolution.
    resolve_ms: u64,
    /// Tool-spec machinery: `build_iteration_tool_specs` (per-tool `spec()`
    /// builds + the deterministic sort), the native-mode refresh, and the
    /// deferred-builtin prompt section.
    tool_specs_ms: u64,
    /// Request assembly: `prepare_messages_with_source_rows` plus the
    /// post-hook prompt-anchor/protocol framing refreshes.
    assemble_ms: u64,
    /// The per-call `before_llm_call` hook chain.
    hooks_ms: u64,
    /// Token estimation: the system floor, `estimate_history_tokens`, and
    /// `estimate_tool_schema_tokens` (which serializes every spec's
    /// parameters JSON — the known tools-serialization cost lives here).
    estimate_ms: u64,
    /// The pre-dispatch trim gate: the post-hook snapshot, the trim decision
    /// and any whole-turn rebuild loop, including the trim event emission.
    trim_ms: u64,
    /// The pre-announce dispatch gates: the retained-tokens context-window
    /// gate, the image-recovery view, `enforce_tool_loop_budget`, the
    /// single-tool-rounds notice, the strict-parsing capability gate, and the
    /// `request_tools` selection.
    preflight_ms: u64,
    /// The announce prologue inside `announce_llm_request` ahead of the
    /// fingerprint: the `WaitingOnModel` progress send, the draft-reasoning
    /// status delta, and the `LlmRequest` observer record. Channel sends can
    /// suspend — a slow consumer lands here.
    announce_ms: u64,
    /// `prefix_fingerprint` inside `announce_llm_request` (system + tools
    /// serialized for hashing — a known per-request serialization).
    fingerprint_ms: u64,
}

impl PreCallTimings {
    pub(crate) fn loop_setup(&mut self, elapsed: Duration) {
        self.loop_setup_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn memory(&mut self, elapsed: Duration) {
        self.memory_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn pacing(&mut self, elapsed: Duration) {
        self.pacing_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn step_scope(&mut self, elapsed: Duration) {
        self.step_scope_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn resolve(&mut self, elapsed: Duration) {
        self.resolve_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn tool_specs(&mut self, elapsed: Duration) {
        self.tool_specs_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn assemble(&mut self, elapsed: Duration) {
        self.assemble_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn hooks(&mut self, elapsed: Duration) {
        self.hooks_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn estimate(&mut self, elapsed: Duration) {
        self.estimate_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn trim(&mut self, elapsed: Duration) {
        self.trim_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn preflight(&mut self, elapsed: Duration) {
        self.preflight_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn announce(&mut self, elapsed: Duration) {
        self.announce_ms += elapsed.as_millis() as u64;
    }

    pub(crate) fn fingerprint(&mut self, elapsed: Duration) {
        self.fingerprint_ms += elapsed.as_millis() as u64;
    }

    /// Insert the partition attrs into the `llm_request` attr map.
    /// `pre_call_ms` is the row's total (engine entry → emission); the named
    /// buckets cover the marked steps and `pre_other_ms` carries the rest —
    /// after the sub-bucket partition that is only the unmarked slivers (the
    /// engine-entry clock/init lines, the inter-mark statement boundaries,
    /// and any gap between the existing named marks) so the family always
    /// sums to the total.
    pub(crate) fn emit_into(
        &self,
        map: &mut serde_json::Map<String, serde_json::Value>,
        pre_call_ms: u64,
    ) {
        let marked_values: [u64; 13] = [
            self.loop_setup_ms,
            self.memory_ms,
            self.pacing_ms,
            self.step_scope_ms,
            self.resolve_ms,
            self.tool_specs_ms,
            self.assemble_ms,
            self.hooks_ms,
            self.estimate_ms,
            self.trim_ms,
            self.preflight_ms,
            self.announce_ms,
            self.fingerprint_ms,
        ];
        let named_sum: u64 = marked_values.iter().sum();
        let other = pre_call_ms.saturating_sub(named_sum);
        for (key, value) in MARKED_KEYS.iter().zip(marked_values) {
            map.insert((*key).to_string(), serde_json::json!(value));
        }
        map.insert("pre_other_ms".to_string(), serde_json::json!(other));
    }
}

#[cfg(test)]
mod tests {
    use super::{PreCallTimings, MARKED_KEYS, PRE_CALL_PARTITION_KEYS};
    use std::time::Duration;

    #[test]
    fn family_reconciles_to_the_pre_call_total() {
        let mut timings = PreCallTimings::default();
        timings.loop_setup(Duration::from_millis(9));
        timings.memory(Duration::from_millis(120));
        timings.pacing(Duration::from_millis(6));
        timings.step_scope(Duration::from_millis(8));
        timings.resolve(Duration::from_millis(5));
        timings.tool_specs(Duration::from_millis(4));
        timings.assemble(Duration::from_millis(1));
        timings.hooks(Duration::from_millis(2));
        timings.estimate(Duration::from_millis(3));
        timings.trim(Duration::from_millis(0));
        timings.preflight(Duration::from_millis(10));
        timings.announce(Duration::from_millis(11));
        timings.fingerprint(Duration::from_millis(7));

        let mut map = serde_json::Map::new();
        timings.emit_into(&mut map, 200);

        for key in PRE_CALL_PARTITION_KEYS {
            assert!(
                map[key].is_u64(),
                "{key} must be emitted as a u64, got: {map:?}"
            );
        }
        assert_eq!(map["pre_loop_setup_ms"].as_u64(), Some(9));
        assert_eq!(map["pre_memory_ms"].as_u64(), Some(120));
        assert_eq!(map["pre_pacing_ms"].as_u64(), Some(6));
        assert_eq!(map["pre_step_scope_ms"].as_u64(), Some(8));
        assert_eq!(map["pre_preflight_ms"].as_u64(), Some(10));
        assert_eq!(map["pre_announce_ms"].as_u64(), Some(11));
        assert_eq!(map["pre_fingerprint_ms"].as_u64(), Some(7));
        let named_sum: u64 = MARKED_KEYS
            .iter()
            .map(|key| map[*key].as_u64().expect("u64 checked above"))
            .sum();
        assert_eq!(
            named_sum + map["pre_other_ms"].as_u64().expect("u64 checked above"),
            200,
            "the family must partition the row's pre_call_ms"
        );
        assert_eq!(
            map["pre_other_ms"].as_u64(),
            Some(14),
            "residual = total minus every named sub-bucket (9+120+6+8+5+4+1+2+3+0+10+11+7)"
        );
    }

    #[test]
    fn other_saturates_at_the_rounding_edge() {
        let mut timings = PreCallTimings::default();
        timings.memory(Duration::from_millis(50));

        let mut map = serde_json::Map::new();
        // A total smaller than the named sum: millisecond rounding can make
        // the named sum exceed the total read a moment later by 1.
        timings.emit_into(&mut map, 10);
        assert_eq!(
            map["pre_other_ms"].as_u64(),
            Some(0),
            "residual saturates at zero"
        );
        assert_eq!(
            map["pre_memory_ms"].as_u64(),
            Some(50),
            "marked buckets stay intact"
        );
    }

    #[test]
    fn zero_timings_still_emit_the_full_family() {
        let mut map = serde_json::Map::new();
        PreCallTimings::default().emit_into(&mut map, 0);
        for key in PRE_CALL_PARTITION_KEYS {
            assert_eq!(map[key].as_u64(), Some(0), "{key} present even at zero");
        }
    }
}
