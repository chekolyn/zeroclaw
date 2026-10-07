//! Loop detection guardrail for the agent tool-call loop.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// ── Configuration ────────────────────────────────────────────────

/// The steering hint injected into the agent's next prompt (via the
/// Warning -> `[Loop Detection]` system-message path in
/// `collect_tool_results`) when a benign-repeat tool trips an
/// identical-results detector. A benign tool's unchanged repeat output
/// usually means the step's work is already complete — the run must be
/// steered to a close, not reaped.
pub(crate) const BENIGN_REPEAT_STEERING_HINT: &str =
    "the tool's result is unchanged; the step's work may be complete — end the turn";

/// Configuration for the loop detector, typically derived from
/// `PacingConfig` fields at the call site.
#[derive(Debug, Clone)]
pub struct LoopDetectorConfig {
    /// Master switch. When `false`, `record` always returns `Ok`.
    pub enabled: bool,
    /// Number of recent calls retained for pattern analysis.
    pub window_size: usize,
    /// How many consecutive exact-repeat calls before escalation starts.
    pub max_repeats: usize,
    /// Tools exempt from the no-progress detector. These are typically
    /// read-only search tools that can legitimately return identical
    /// results for different queries (e.g. content_search returning
    /// "no results" for different queries). Exact-repeat and ping-pong
    /// detection still apply to these tools.
    pub no_progress_exempt_tools: Vec<String>,
    /// The benign-repeat class: idempotent status/read tools (sop_status,
    /// cron_list, memory_recall) whose identical repeat output usually
    /// means the step's work is already complete. An identical-results
    /// trip on one of these tools steers — a Warning carrying the
    /// steering hint below, injected into the agent's next prompt — at
    /// every escalation level, instead of refusing the call (Block) or
    /// aborting the run (Break). The run-abort is retained for write-path
    /// tools (file_write/shell/mqtt_publish, the non-benign default)
    /// where identical results are a real no-progress loop.
    pub benign_repeat_tools: Vec<String>,
}

impl Default for LoopDetectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_size: 20,
            max_repeats: 3,
            no_progress_exempt_tools: vec![
                "content_search".to_string(),
                "web_search_tool".to_string(),
                "glob_search".to_string(),
            ],
            benign_repeat_tools: vec![
                "sop_status".to_string(),
                "cron_list".to_string(),
                "memory_recall".to_string(),
            ],
        }
    }
}

// ── Result enum ──────────────────────────────────────────────────

/// Outcome of a loop-detection check after recording a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopDetectionResult {
    /// No pattern detected — continue normally.
    Ok,
    /// A suspicious pattern was detected; the caller should inject a
    /// system-level nudge message into the conversation.
    Warning(String),
    /// The tool call should be refused (output replaced with an error).
    Block(String),
    /// The agent turn should be terminated immediately.
    Break(String),
}

// ── Internal types ───────────────────────────────────────────────

/// A single recorded tool invocation inside the sliding window.
#[derive(Debug, Clone)]
struct ToolCallRecord {
    /// Tool name.
    name: String,
    /// Hash of the serialised arguments.
    args_hash: u64,
    /// Hash of the tool's output/result.
    result_hash: u64,
}

/// Produce a deterministic hash for a JSON value that is invariant under
/// object-key reordering.  Implemented as a streaming walker that feeds
/// structural tags + sorted keys + leaves directly to a [`Hasher`], so the
/// only allocations along the path are the per-vector key slices plus a
/// short owned `String` per numeric leaf for the canonical-text form.
///
/// The hot path is `record()` in `collect_tool_results`, called once per
/// successful tool call inside the agent loop.  The previous implementation
/// (`canonicalise` + `serde_json::to_string`) deep-cloned the entire args
/// tree and then serialised it, so on long tool-heavy turns each call added
/// one tree-sized `Value` clone plus one proportional owned `String`.
/// This streaming form avoids both: code review of the walker establishes
/// the changed allocation shape; the regression suite in this module
/// (see `streaming_hash_matches_legacy_canonicalise_equivalence`) pins the
/// resulting equality classes and confirms the walker does not panic on
/// large inputs. RSS / monotonic-arena behavior is not measured here.
fn hash_value(value: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_value_into(value, &mut hasher);
    hasher.finish()
}

/// Walker backing [`hash_value`].  Emits a deterministic byte stream keyed by
/// structure: structural tags disambiguate `"123"` (string) from `123`
/// (number), `true` from `"true"`, `null` from `"null"`.  Object keys are
/// sorted on the fly; the key→value boundary is marked so two objects with
/// the same keys and leaves hash identically regardless of insertion order.
fn hash_value_into<H: Hasher>(value: &serde_json::Value, hasher: &mut H) {
    match value {
        serde_json::Value::Null => {
            "null".hash(hasher);
        }
        serde_json::Value::Bool(b) => {
            "bool".hash(hasher);
            b.hash(hasher);
        }
        serde_json::Value::Number(n) => {
            "num".hash(hasher);
            // Hash the canonical serialized text instead of `Number::hash`.
            // `serde_json::Number`'s `Hash` impl routes integer `0` and float
            // `0.0` through `0.0f64.to_bits()` and they collide, and it
            // normalizes `+0.0` / `-0.0`. The legacy `canonicalise` +
            // `serde_json::to_string` path produced `"0"`, `"0.0"`, and
            // `"-0.0"` as distinct tokens; this preserves that equality
            // classification per the reference oracle.
            let text = n.to_string();
            text.len().hash(hasher);
            text.hash(hasher);
        }
        serde_json::Value::String(s) => {
            "str".hash(hasher);
            // Length-prefix the string content so that, e.g. ["ab", "c"] and
            // ["a", "bc"] (different leaf structure) cannot collide.
            s.len().hash(hasher);
            s.hash(hasher);
        }
        serde_json::Value::Array(arr) => {
            "arr".hash(hasher);
            arr.len().hash(hasher);
            for item in arr {
                hash_value_into(item, hasher);
            }
        }
        serde_json::Value::Object(map) => {
            "obj".hash(hasher);
            map.len().hash(hasher);
            // Sort keys on the fly. `Vec<(&String, &Value)>` is the only
            // allocation; the values themselves are borrowed, so a 1 MB
            // payload costs (key references + Vec overhead) rather than a
            // second tree-sized clone.
            let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            for (k, v) in sorted {
                // Mark the key→value boundary so, e.g. `{"ab":"c"}` and
                // `{"a":"bc"}` cannot collide on a shared suffix.
                0u8.hash(hasher);
                k.len().hash(hasher);
                k.hash(hasher);
                1u8.hash(hasher);
                hash_value_into(v, hasher);
            }
        }
    }
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

// ── Detector ─────────────────────────────────────────────────────

/// Stateful loop detector that lives for the duration of a single
/// `run_tool_call_loop` invocation.
pub struct LoopDetector {
    config: LoopDetectorConfig,
    window: VecDeque<ToolCallRecord>,
}

impl LoopDetector {
    pub fn new(config: LoopDetectorConfig) -> Self {
        Self {
            window: VecDeque::with_capacity(config.window_size),
            config,
        }
    }

    pub fn record(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> LoopDetectionResult {
        if !self.config.enabled {
            return LoopDetectionResult::Ok;
        }

        let record = ToolCallRecord {
            name: name.to_string(),
            args_hash: hash_value(args),
            result_hash: hash_str(result),
        };

        // Maintain sliding window.
        if self.window.len() >= self.config.window_size {
            self.window.pop_front();
        }
        self.window.push_back(record);

        // Run detectors in escalation order (most severe first).
        if let Some(result) = self.detect_exact_repeat() {
            return result;
        }
        if let Some(result) = self.detect_ping_pong() {
            return result;
        }
        if let Some(result) = self.detect_no_progress() {
            return result;
        }

        LoopDetectionResult::Ok
    }

    /// The benign-repeat class consult: is this tool an idempotent
    /// status/read whose identical repeat output means the step's work is
    /// likely already complete?
    fn is_benign_repeat_tool(&self, name: &str) -> bool {
        self.config.benign_repeat_tools.iter().any(|t| t == name)
    }

    /// The silent no-progress exemption consult (read-only search tools).
    fn is_no_progress_exempt(&self, name: &str) -> bool {
        self.config
            .no_progress_exempt_tools
            .iter()
            .any(|t| t == name)
    }

    fn detect_exact_repeat(&self) -> Option<LoopDetectionResult> {
        let max = self.config.max_repeats;
        if self.window.len() < max {
            return None;
        }

        let last = self.window.back()?;
        let consecutive = self
            .window
            .iter()
            .rev()
            .take_while(|r| r.name == last.name && r.args_hash == last.args_hash)
            .count();

        // The benign-repeat class (RCA fix #4): an idempotent status/read
        // tool repeating identically steers — the Warning + steering hint
        // rides the existing nudge path (`collect_tool_results` appends it
        // as a system message into the agent's next prompt) — at EVERY
        // escalation level, instead of refusing the call (Block) or
        // aborting the run (Break). The abort is retained for write-path
        // tools: for those, an identical repeat is a real no-progress loop.
        if consecutive >= max && self.is_benign_repeat_tool(&last.name) {
            return Some(LoopDetectionResult::Warning(format!(
                "tool '{}' called {} times consecutively with identical arguments — {}",
                last.name, consecutive, BENIGN_REPEAT_STEERING_HINT
            )));
        }

        if consecutive >= max + 2 {
            Some(LoopDetectionResult::Break(format!(
                "Circuit breaker: tool '{}' called {} times consecutively with identical arguments",
                last.name, consecutive
            )))
        } else if consecutive > max {
            Some(LoopDetectionResult::Block(format!(
                "Blocked: tool '{}' called {} times consecutively with identical arguments",
                last.name, consecutive
            )))
        } else if consecutive >= max {
            Some(LoopDetectionResult::Warning(format!(
                "Warning: tool '{}' has been called {} times consecutively with identical arguments. \
                 Try a different approach.",
                last.name, consecutive
            )))
        } else {
            None
        }
    }

    /// Pattern 2: Two tools alternating (A->B->A->B) for 4+ full cycles
    /// (i.e. 8 consecutive entries following the pattern).
    fn detect_ping_pong(&self) -> Option<LoopDetectionResult> {
        const MIN_CYCLES: usize = 4;
        let needed = MIN_CYCLES * 2; // each cycle = 2 calls

        if self.window.len() < needed {
            return None;
        }

        let tail: Vec<&ToolCallRecord> = self.window.iter().rev().take(needed).collect();
        // tail[0] is most recent; pattern: A, B, A, B, ...
        let a_name = &tail[0].name;
        let b_name = &tail[1].name;

        if a_name == b_name {
            return None;
        }

        let is_ping_pong = tail.iter().enumerate().all(|(i, r)| {
            if i % 2 == 0 {
                &r.name == a_name
            } else {
                &r.name == b_name
            }
        });

        if !is_ping_pong {
            return None;
        }

        // Count total alternating length for escalation.
        let mut cycles = MIN_CYCLES;
        let extended: Vec<&ToolCallRecord> = self.window.iter().rev().collect();
        for extra_pair in extended.chunks(2).skip(MIN_CYCLES) {
            if extra_pair.len() == 2
                && &extra_pair[0].name == a_name
                && &extra_pair[1].name == b_name
            {
                cycles += 1;
            } else {
                break;
            }
        }

        if cycles >= MIN_CYCLES + 2 {
            Some(LoopDetectionResult::Break(format!(
                "Circuit breaker: tools '{}' and '{}' have been alternating for {} cycles",
                a_name, b_name, cycles
            )))
        } else if cycles > MIN_CYCLES {
            Some(LoopDetectionResult::Block(format!(
                "Blocked: tools '{}' and '{}' have been alternating for {} cycles",
                a_name, b_name, cycles
            )))
        } else {
            Some(LoopDetectionResult::Warning(format!(
                "Warning: tools '{}' and '{}' appear to be alternating ({} cycles). \
                 Consider a different strategy.",
                a_name, b_name, cycles
            )))
        }
    }

    /// Pattern 3: Same tool called 5+ times (with different args each time)
    /// but producing the exact same result hash every time, counted across the
    /// whole window so interleaved unrelated calls do not reset the streak.
    fn detect_no_progress(&self) -> Option<LoopDetectionResult> {
        const MIN_CALLS: usize = 5;

        if self.window.len() < MIN_CALLS {
            return None;
        }

        let last = self.window.back()?;

        // The declared silent exemption (wired here — it was dead config
        // from its introduction through v0.8.5: `no_progress_exempt_tools`
        // was never consulted, so search tools returning "no results" for
        // different queries were exposed to the no-progress breaker).
        // Exempt tools produce no trip at all; exact-repeat and ping-pong
        // detection still apply to them.
        if self.is_no_progress_exempt(&last.name) {
            return None;
        }

        // the stuck agent ran 43 near-duplicate shell calls returning
        // byte-identical output, interleaved with other tools; filter (not a
        // consecutive take_while) is what lets that non-adjacent run be counted.
        let same_tool_same_result: Vec<&ToolCallRecord> = self
            .window
            .iter()
            .filter(|r| r.name == last.name && r.result_hash == last.result_hash)
            .collect();

        let count = same_tool_same_result.len();
        if count < MIN_CALLS {
            return None;
        }

        // Verify they have *different* args (otherwise exact_repeat handles it).
        let unique_args: std::collections::HashSet<u64> =
            same_tool_same_result.iter().map(|r| r.args_hash).collect();
        if unique_args.len() < 2 {
            // All same args — this is exact-repeat territory, not no-progress.
            return None;
        }

        // The benign-repeat class (RCA fix #4): steering at every escalation
        // level for idempotent status/reads — never Block, never Break. The
        // agent re-reads the same unchanged state; the hint tells it the
        // step's work may already be done. The abort ladder below stays the
        // write-path contract (file_write/shell/mqtt_publish).
        if self.is_benign_repeat_tool(&last.name) {
            return Some(LoopDetectionResult::Warning(format!(
                "tool '{}' called {} times with different arguments but identical results — {}",
                last.name, count, BENIGN_REPEAT_STEERING_HINT
            )));
        }

        if count >= MIN_CALLS + 2 {
            Some(LoopDetectionResult::Break(format!(
                "Circuit breaker: tool '{}' called {} times with different arguments but identical results — no progress",
                last.name, count
            )))
        } else if count > MIN_CALLS {
            Some(LoopDetectionResult::Block(format!(
                "Blocked: tool '{}' called {} times with different arguments but identical results",
                last.name, count
            )))
        } else {
            Some(LoopDetectionResult::Warning(format!(
                "Warning: tool '{}' called {} times with different arguments but identical results. \
                 The current approach may not be making progress.",
                last.name, count
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn default_config() -> LoopDetectorConfig {
        LoopDetectorConfig::default()
    }

    fn config_with_repeats(max_repeats: usize) -> LoopDetectorConfig {
        LoopDetectorConfig {
            enabled: true,
            window_size: 20,
            max_repeats,
            no_progress_exempt_tools: Vec::new(),
            benign_repeat_tools: Vec::new(),
        }
    }

    // ── Exact repeat tests ───────────────────────────────────────

    #[test]
    fn exact_repeat_warning_at_threshold() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"path": "/tmp/foo"});

        assert_eq!(
            det.record("file_read", &args, "contents"),
            LoopDetectionResult::Ok
        );
        assert_eq!(
            det.record("file_read", &args, "contents"),
            LoopDetectionResult::Ok
        );
        // 3rd consecutive = warning
        match det.record("file_read", &args, "contents") {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("file_read"));
                assert!(msg.contains("3 times"));
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn exact_repeat_block_at_threshold_plus_one() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"cmd": "ls"});

        for _ in 0..3 {
            det.record("shell", &args, "output");
        }
        match det.record("shell", &args, "output") {
            LoopDetectionResult::Block(msg) => {
                assert!(msg.contains("shell"));
                assert!(msg.contains("4 times"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn exact_repeat_break_at_threshold_plus_two() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"q": "test"});

        for _ in 0..4 {
            det.record("search", &args, "no results");
        }
        match det.record("search", &args, "no results") {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("Circuit breaker"));
                assert!(msg.contains("search"));
            }
            other => panic!("expected Break, got {other:?}"),
        }
    }

    #[test]
    fn exact_repeat_resets_on_different_call() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"x": 1});

        det.record("tool_a", &args, "r1");
        det.record("tool_a", &args, "r1");
        // Interject a different tool — resets the consecutive exact-repeat streak.
        det.record("tool_b", &json!({}), "r2");
        det.record("tool_a", &args, "r1");
        // Only 2 consecutive exact repeats now; a different-result call must
        // not trip exact-repeat (and distinct results keep no-progress quiet).
        assert_eq!(
            det.record("tool_a", &json!({"x": 999}), "r_distinct"),
            LoopDetectionResult::Ok
        );
    }

    // ── Ping-pong tests ──────────────────────────────────────────

    #[test]
    fn ping_pong_warning_at_four_cycles() {
        let mut det = LoopDetector::new(default_config());
        let args = json!({});

        // 4 full cycles = 8 calls: A B A B A B A B
        for i in 0..8 {
            let name = if i % 2 == 0 { "read" } else { "write" };
            let result = det.record(name, &args, &format!("r{i}"));
            if i < 7 {
                assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
            } else {
                match result {
                    LoopDetectionResult::Warning(msg) => {
                        assert!(msg.contains("read"));
                        assert!(msg.contains("write"));
                        assert!(msg.contains("4 cycles"));
                    }
                    other => panic!("expected Warning at cycle 4, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn ping_pong_escalates_with_more_cycles() {
        let mut det = LoopDetector::new(default_config());
        let args = json!({});

        // 5 cycles = 10 calls.  The 10th call (completing cycle 5) triggers Block.
        for i in 0..10 {
            let name = if i % 2 == 0 { "fetch" } else { "parse" };
            det.record(name, &args, &format!("r{i}"));
        }
        // 11th call extends to 5.5 cycles; detector still counts 5 full -> Block.
        let r = det.record("fetch", &args, "r10");
        match r {
            LoopDetectionResult::Block(msg) => {
                assert!(msg.contains("fetch"));
                assert!(msg.contains("parse"));
                assert!(msg.contains("5 cycles"));
            }
            other => panic!("expected Block at 5 cycles, got {other:?}"),
        }
    }

    #[test]
    fn ping_pong_not_triggered_for_same_tool() {
        let mut det = LoopDetector::new(default_config());
        let args = json!({});

        // Same tool repeated is not ping-pong.
        for _ in 0..10 {
            det.record("read", &args, "data");
        }
        // The exact_repeat detector fires, not ping_pong.
        // Verify by checking message content doesn't mention "alternating".
        let r = det.record("read", &args, "data");
        if let LoopDetectionResult::Break(msg) | LoopDetectionResult::Block(msg) = r {
            assert!(
                !msg.contains("alternating"),
                "should be exact-repeat, not ping-pong"
            );
        }
    }

    // ── No-progress tests ────────────────────────────────────────

    #[test]
    fn no_progress_warning_at_five_different_args_same_result() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..5 {
            let args = json!({"query": format!("attempt_{i}")});
            let result = det.record("search", &args, "no results found");
            if i < 4 {
                assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
            } else {
                match result {
                    LoopDetectionResult::Warning(msg) => {
                        assert!(msg.contains("search"));
                        assert!(msg.contains("identical results"));
                    }
                    other => panic!("expected Warning, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn no_progress_escalates_to_block_and_break() {
        let mut det = LoopDetector::new(default_config());

        // 6 calls with different args, same result.
        for i in 0..6 {
            let args = json!({"q": format!("v{i}")});
            det.record("web_fetch", &args, "timeout");
        }
        // 7th call: count=7 which is >= MIN_CALLS(5)+2 -> Break.
        let r7 = det.record("web_fetch", &json!({"q": "v6"}), "timeout");
        match r7 {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("web_fetch"));
                assert!(msg.contains("7 times"));
                assert!(msg.contains("no progress"));
            }
            other => panic!("expected Break at 7 calls, got {other:?}"),
        }
    }

    #[test]
    fn no_progress_not_triggered_when_results_differ() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..8 {
            let args = json!({"q": format!("v{i}")});
            let result = det.record("search", &args, &format!("result_{i}"));
            assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
        }
    }

    #[test]
    fn no_progress_triggered_when_interleaved_with_other_calls() {
        // same tool + same result repeated non-consecutively, with
        // varied unrelated calls interleaved, must still be detected. The old
        // take_while logic reset the streak on any interleaved call.
        let mut det = LoopDetector::new(default_config());

        let mut last = LoopDetectionResult::Ok;
        for i in 0..5 {
            let args = json!({"q": format!("v{i}")});
            last = det.record("search", &args, "no results found");
            // Interleave a distinct unrelated tool each time so neither
            // ping-pong nor exact-repeat fires before no-progress.
            det.record(
                &format!("reader_{i}"),
                &json!({"path": format!("/f{i}")}),
                &format!("body_{i}"),
            );
        }

        match last {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("search"), "got: {msg}");
                assert!(msg.contains("identical results"), "got: {msg}");
            }
            other => panic!("expected Warning on 5th interleaved probe, got {other:?}"),
        }
    }

    #[test]
    fn no_progress_not_triggered_when_all_args_identical() {
        // If args are all the same, exact_repeat should fire, not no_progress.
        let mut det = LoopDetector::new(config_with_repeats(6));
        let args = json!({"q": "same"});

        for _ in 0..5 {
            det.record("search", &args, "no results");
        }
        // 6th call = exact repeat at threshold (max_repeats=6) -> Warning.
        // no_progress requires >=2 unique args, so it must NOT fire.
        let r = det.record("search", &args, "no results");
        match r {
            LoopDetectionResult::Warning(msg) => {
                assert!(
                    msg.contains("identical arguments"),
                    "should be exact-repeat Warning, got: {msg}"
                );
            }
            other => panic!("expected exact-repeat Warning, got {other:?}"),
        }
    }

    // ── Disabled / config tests ──────────────────────────────────

    #[test]
    fn disabled_detector_always_returns_ok() {
        let config = LoopDetectorConfig {
            enabled: false,
            ..Default::default()
        };
        let mut det = LoopDetector::new(config);
        let args = json!({"x": 1});

        for _ in 0..20 {
            assert_eq!(det.record("tool", &args, "same"), LoopDetectionResult::Ok);
        }
    }

    #[test]
    fn window_size_limits_memory() {
        let config = LoopDetectorConfig {
            enabled: true,
            window_size: 5,
            max_repeats: 3,
            no_progress_exempt_tools: Vec::new(),
            benign_repeat_tools: Vec::new(),
        };
        let mut det = LoopDetector::new(config);
        let args = json!({"x": 1});

        // Fill window with 5 different tools.
        for i in 0..5 {
            det.record(&format!("tool_{i}"), &args, "result");
        }
        assert_eq!(det.window.len(), 5);

        // Adding one more evicts the oldest.
        det.record("tool_5", &args, "result");
        assert_eq!(det.window.len(), 5);
        assert_eq!(det.window.front().unwrap().name, "tool_1");
    }

    // ── Ping-pong with varying args ─────────────────────────────

    #[test]
    fn ping_pong_detects_alternation_with_varying_args() {
        let mut det = LoopDetector::new(default_config());

        // A->B->A->B with different args each time — ping-pong cares only
        // about tool names, not argument equality.
        for i in 0..8 {
            let name = if i % 2 == 0 { "read" } else { "write" };
            let args = json!({"attempt": i});
            let result = det.record(name, &args, &format!("r{i}"));
            if i < 7 {
                assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
            } else {
                match result {
                    LoopDetectionResult::Warning(msg) => {
                        assert!(msg.contains("read"));
                        assert!(msg.contains("write"));
                        assert!(msg.contains("4 cycles"));
                    }
                    other => panic!("expected Warning at cycle 4, got {other:?}"),
                }
            }
        }
    }

    // ── Window eviction test ────────────────────────────────────

    #[test]
    fn window_eviction_prevents_stale_pattern_detection() {
        let config = LoopDetectorConfig {
            enabled: true,
            window_size: 6,
            max_repeats: 3,
            no_progress_exempt_tools: Vec::new(),
            benign_repeat_tools: Vec::new(),
        };
        let mut det = LoopDetector::new(config);
        let args = json!({"x": 1});

        // 2 consecutive calls of "tool_a".
        det.record("tool_a", &args, "r");
        det.record("tool_a", &args, "r");

        // Fill the rest of the window with different tools (evicting the
        // first "tool_a" calls as the window is only 6).
        for i in 0..5 {
            det.record(&format!("other_{i}"), &json!({}), "ok");
        }

        // Now "tool_a" again — only 1 consecutive, not 3.
        let r = det.record("tool_a", &args, "r");
        assert_eq!(
            r,
            LoopDetectionResult::Ok,
            "stale entries should be evicted"
        );
    }

    // ── hash_value key-order independence ────────────────────────

    #[test]
    fn hash_value_is_key_order_independent() {
        let a = json!({"alpha": 1, "beta": 2});
        let b = json!({"beta": 2, "alpha": 1});
        assert_eq!(
            hash_value(&a),
            hash_value(&b),
            "hash_value must produce identical hashes regardless of JSON key order"
        );
    }

    #[test]
    fn hash_value_nested_key_order_independent() {
        let a = json!({"outer": {"x": 1, "y": 2}, "z": [1, 2]});
        let b = json!({"z": [1, 2], "outer": {"y": 2, "x": 1}});
        assert_eq!(
            hash_value(&a),
            hash_value(&b),
            "nested objects must also be key-order independent"
        );
    }

    #[test]
    fn hash_value_distinguishes_string_from_number_and_bool() {
        // The streaming walker tags leaves by JSON type, so a number `1`,
        // the string `"1"`, and the bool `true` must all produce distinct
        // hashes (this used to fall out of serde_json::to_string).
        let num = json!(1);
        let str_num = json!("1");
        let truthy = json!(true);
        assert_ne!(hash_value(&num), hash_value(&str_num));
        assert_ne!(hash_value(&num), hash_value(&truthy));
        assert_ne!(hash_value(&str_num), hash_value(&truthy));
        assert_ne!(hash_value(&json!(null)), hash_value(&json!("null")));
    }

    #[test]
    fn hash_value_distinguishes_array_ordering() {
        // Arrays are order-sensitive; [1, 2] and [2, 1] must hash differently.
        assert_ne!(
            hash_value(&json!([1, 2])),
            hash_value(&json!([2, 1])),
            "array ordering is part of the value and must affect the hash"
        );
    }

    #[test]
    fn hash_value_handles_large_args_without_panicking() {
        // Pin the streaming walker on a payload that would have triggered
        // a multi-MB transient allocation in the legacy canonicalise +
        // serde_json::to_string path. We do not assert on allocation count
        // (test-only instrumentation is brittle on glibc arenas) — we just
        // assert the hash is stable and the call returns.
        let big_payload = "x".repeat(1024 * 1024);
        let args = json!({ "body": big_payload, "kind": "echo" });
        let h1 = hash_value(&args);
        let h2 = hash_value(&args);
        assert_eq!(h1, h2, "hash must be deterministic for the same input");

        // And a permuted key order over the same payload must match.
        let args_permuted = json!({ "kind": "echo", "body": big_payload });
        assert_eq!(
            h1,
            hash_value(&args_permuted),
            "key-order independence must hold for large payloads too"
        );
    }

    // ── Legacy equivalence oracle ─────────────────────────────────

    /// Legacy canonicalisation: deep-clone with object keys sorted recursively.
    /// Retained as a **test-only oracle** so the streaming `hash_value` can be
    /// compared against the old `canonicalise` + `serde_json::to_string` + hash
    /// path on representative inputs.
    #[cfg(test)]
    fn legacy_canonicalise(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                sorted.sort_by_key(|(k, _)| *k);
                let new_map: serde_json::Map<String, serde_json::Value> = sorted
                    .into_iter()
                    .map(|(k, v)| (k.clone(), legacy_canonicalise(v)))
                    .collect();
                serde_json::Value::Object(new_map)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(legacy_canonicalise).collect())
            }
            other => other.clone(),
        }
    }

    /// Legacy hash path: canonicalise → `serde_json::to_string` → `DefaultHasher`.
    fn legacy_hash(value: &serde_json::Value) -> u64 {
        use std::hash::Hasher;
        let canonical = serde_json::to_string(&legacy_canonicalise(value)).unwrap_or_default();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        canonical.hash(&mut hasher);
        hasher.finish()
    }

    /// Equivalence battery: the streaming `hash_value` must classify the same
    /// value pairs as equal or different as the legacy `canonicalise` +
    /// `serde_json::to_string` + hash path across nested objects, arrays of
    /// objects, large and escaped strings, numbers, booleans, nulls, and mixed
    /// values.
    #[test]
    fn streaming_hash_matches_legacy_canonicalise_equivalence() {
        let cases: &[(&str, serde_json::Value, serde_json::Value, bool)] = &[
            // Same logical content, different key order → equal
            (
                "key-order",
                json!({"alpha": 1, "beta": 2}),
                json!({"beta": 2, "alpha": 1}),
                true,
            ),
            // Nested objects, key-order invariant
            (
                "nested-objects",
                json!({"outer": {"inner": {"a": 1, "b": 2}}}),
                json!({"outer": {"inner": {"b": 2, "a": 1}}}),
                true,
            ),
            // Arrays of objects
            (
                "array-of-objects",
                json!({"items": [{"x": 1, "y": 2}, {"x": 3, "y": 4}]}),
                json!({"items": [{"y": 2, "x": 1}, {"y": 4, "x": 3}]}),
                true,
            ),
            // Mixed values: null, bool, number, string
            (
                "mixed-leaves",
                json!({"n": null, "b": true, "num": 42, "s": "hello"}),
                json!({"s": "hello", "b": true, "n": null, "num": 42}),
                true,
            ),
            // Different string values → not equal
            (
                "different-string",
                json!({"x": "a"}),
                json!({"x": "b"}),
                false,
            ),
            // Different number → not equal
            ("different-number", json!({"x": 1}), json!({"x": 2}), false),
            // Different boolean → not equal
            (
                "different-bool",
                json!({"x": true}),
                json!({"x": false}),
                false,
            ),
            // null vs string → not equal
            (
                "null-vs-string",
                json!({"x": null}),
                json!({"x": "null"}),
                false,
            ),
            // Nested difference → not equal
            (
                "nested-diff",
                json!({"a": {"b": 1}}),
                json!({"a": {"b": 2}}),
                false,
            ),
            // Array ordering matters → not equal
            ("array-ordering", json!([1, 2, 3]), json!([3, 2, 1]), false),
            // Large escaped string
            (
                "large-string",
                json!({"data": "x".repeat(10000) + "\"escaped\"\n\t\\"}),
                json!({"data": "x".repeat(10000) + "\"escaped\"\n\t\\"}),
                true,
            ),
            // Large string differs at end
            (
                "large-string-diff",
                json!({"data": "x".repeat(10000) + "a"}),
                json!({"data": "x".repeat(10000) + "b"}),
                false,
            ),
            // Numeric equivalence battery. The legacy path serialised
            // numbers via serde_json, so `0`, `0.0`, and `-0.0` produced
            // distinct tokens `0`, `0.0`, `-0.0`. The previous
            // `Number::hash` impl routed integer 0 and float 0.0 through
            // `0.0f64.to_bits()` (colliding) and normalized `+0.0` /
            // `-0.0`. After the number-text fix above these cases
            // reclassify in line with legacy equality.
            (
                "int-zero-vs-float-zero",
                json!({"x": 0}),
                json!({"x": 0.0}),
                false,
            ),
            (
                "positive-zero-vs-negative-zero",
                json!({"x": 0.0}),
                json!({"x": -0.0}),
                false,
            ),
            (
                "int-zero-vs-negative-zero",
                json!({"x": 0}),
                json!({"x": -0.0}),
                false,
            ),
            (
                "same-float-text",
                json!({"x": 1.5}),
                json!({"x": 1.5}),
                true,
            ),
            (
                "different-float-text",
                json!({"x": 1.5}),
                json!({"x": 2.5}),
                false,
            ),
            (
                "int-vs-float-one",
                json!({"x": 1}),
                json!({"x": 1.0}),
                false,
            ),
            (
                "int-vs-negative-float",
                json!({"x": -1}),
                json!({"x": -1.0}),
                false,
            ),
            (
                "large-int-vs-large-float",
                json!({"x": 123456789012345_i64}),
                json!({"x": 123456789012345.0}),
                false,
            ),
        ];

        for (label, a, b, expect_equal) in cases {
            let stream_a = hash_value(a);
            let stream_b = hash_value(b);
            let legacy_a = legacy_hash(a);
            let legacy_b = legacy_hash(b);

            let stream_eq = stream_a == stream_b;
            let legacy_eq = legacy_a == legacy_b;

            assert_eq!(
                stream_eq, legacy_eq,
                "{label}: streaming ({stream_a} vs {stream_b}) and legacy ({legacy_a} vs {legacy_b}) must agree on equality"
            );
            assert_eq!(
                stream_eq, *expect_equal,
                "{label}: expected equal={expect_equal}, got streaming_eq={stream_eq}"
            );
        }
    }

    // ── Escalation order tests ───────────────────────────────────

    #[test]
    fn exact_repeat_takes_priority_over_no_progress() {
        // If tool+args are identical, exact_repeat fires before no_progress.
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"q": "same"});

        det.record("s", &args, "r");
        det.record("s", &args, "r");
        let r = det.record("s", &args, "r");
        match r {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("identical arguments"));
            }
            other => panic!("expected exact-repeat Warning, got {other:?}"),
        }
    }

    // ── No-progress exemption tests ──────────────────────────────

    #[test]
    fn no_progress_exempt_tool_different_args_identical_result_returns_ok() {
        // content_search is a read-only search tool that can legitimately
        // return identical results for different queries — the silent
        // exemption (no trip at all). The exemption consult was declared
        // since v0.8.4 but never wired (dead config); this fix wires it.
        let mut det = LoopDetector::new(default_config());

        for i in 0..7 {
            let args = json!({"query": format!("attempt_{i}")});
            let result = det.record("content_search", &args, "no results found");
            assert_eq!(
                result,
                LoopDetectionResult::Ok,
                "content_search call {i} should be exempt from no-progress"
            );
        }
    }

    #[test]
    fn no_progress_exempt_tool_same_args_still_triggers_exact_repeat() {
        // The silent exemption only applies to detect_no_progress.
        // Exact-repeat detection still fires when an exempt tool is called
        // with the SAME arguments repeatedly. 7 identical calls with
        // max_repeats=3 -> consecutive=7 >= max_repeats+2(5) -> Break.
        let mut det = LoopDetector::new(LoopDetectorConfig {
            no_progress_exempt_tools: vec!["content_search".to_string()],
            ..config_with_repeats(3)
        });
        let args = json!({"key": "same_key"});

        for _ in 0..6 {
            det.record("content_search", &args, "some result");
        }
        // 7th consecutive identical call -> exact-repeat Break (circuit breaker).
        match det.record("content_search", &args, "some result") {
            LoopDetectionResult::Break(msg) => {
                assert!(
                    msg.contains("content_search"),
                    "should mention the tool: {msg}"
                );
                assert!(
                    msg.contains("identical arguments"),
                    "should be exact-repeat Break, got: {msg}"
                );
            }
            other => panic!("expected exact-repeat Break, got {other:?}"),
        }
    }

    #[test]
    fn no_progress_non_exempt_tool_still_triggers_break() {
        // Backward compatibility: a non-exempt tool (e.g. "shell") called 7
        // times with different args but identical results must still trip
        // the no-progress circuit breaker.
        let mut det = LoopDetector::new(default_config());

        for i in 0..6 {
            let args = json!({"cmd": format!("echo {i}")});
            det.record("shell", &args, "done");
        }
        // 7th call: count=7 >= MIN_CALLS(5)+2 -> Break.
        match det.record("shell", &json!({"cmd": "echo 6"}), "done") {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("shell"));
                assert!(msg.contains("no progress"));
            }
            other => panic!("expected no-progress Break for non-exempt tool, got {other:?}"),
        }
    }

    // ── The benign-repeat class (RCA fix #4) ─────────────────────
    //
    // The 2026-10 verify-improvement SOP runs died 3× to circuit
    // breakers AFTER completing their real work: the CB treated the
    // agent's confused-retry flailing on idempotent status/reads as a
    // run-killer. A benign tool's identical repeat output usually means
    // the step's work is already complete — steer, don't reap.

    #[test]
    fn benign_default_class_pins_the_ruling_tools() {
        // The shipped default posture: the benign-repeat class is exactly
        // the ruling's idempotent status/reads; memory_recall moved here
        // from the silent-exempt default (steering is strictly stronger:
        // no abort in BOTH detectors + the hint).
        let config = LoopDetectorConfig::default();
        assert_eq!(
            config.benign_repeat_tools,
            vec![
                "sop_status".to_string(),
                "cron_list".to_string(),
                "memory_recall".to_string(),
            ]
        );
        assert_eq!(
            config.no_progress_exempt_tools,
            vec![
                "content_search".to_string(),
                "web_search_tool".to_string(),
                "glob_search".to_string(),
            ],
            "memory_recall must not stay in the exempt default: benign steering supersedes it"
        );
    }

    #[test]
    fn benign_exact_repeat_trips_as_steering_warning_not_abort() {
        // RCA incident #1 verbatim: "tool 'sop_status' called 5 times
        // consecutively with identical arguments" — previously a Break at
        // the 5th call (max_repeats=3 + 2). The benign class steers instead:
        // every trip level is the Warning + steering hint; never Block
        // (the call is never refused), never Break (the run is never
        // aborted).
        let mut det = LoopDetector::new(default_config());
        let args = json!({"run_id": "run-123"});

        assert_eq!(
            det.record("sop_status", &args, "status: running"),
            LoopDetectionResult::Ok
        );
        assert_eq!(
            det.record("sop_status", &args, "status: running"),
            LoopDetectionResult::Ok
        );
        for call in 3..=7 {
            match det.record("sop_status", &args, "status: running") {
                LoopDetectionResult::Warning(msg) => {
                    assert!(msg.contains("sop_status"), "call {call}: {msg}");
                    assert!(msg.contains("identical arguments"), "call {call}: {msg}");
                    assert!(
                        msg.contains(BENIGN_REPEAT_STEERING_HINT),
                        "call {call} must carry the steering hint: {msg}"
                    );
                }
                other => panic!(
                    "call {call}: expected steering Warning, got {other:?} \
                     — a benign status read must never abort the run"
                ),
            }
        }
    }

    #[test]
    fn benign_no_progress_trips_as_steering_warning_not_abort() {
        // RCA incident #2's shape on a benign tool: different arguments,
        // identical results, 5..7 calls — previously Block at 6 and Break
        // at 7. The benign class steers at every level instead.
        let mut det = LoopDetector::new(default_config());

        for call in 0..7 {
            let args = json!({"cron_id": format!("job_{call}")});
            match det.record("cron_list", &args, "no cron jobs found") {
                r if call < 4 => assert_eq!(
                    r,
                    LoopDetectionResult::Ok,
                    "call {call} is below the no-progress threshold"
                ),
                LoopDetectionResult::Warning(msg) => {
                    assert!(msg.contains("cron_list"), "call {call}: {msg}");
                    assert!(msg.contains("identical results"), "call {call}: {msg}");
                    assert!(
                        msg.contains(BENIGN_REPEAT_STEERING_HINT),
                        "call {call} must carry the steering hint: {msg}"
                    );
                }
                other => panic!(
                    "call {call}: expected steering Warning, got {other:?} \
                     — a benign read's no-progress trip must never abort the run"
                ),
            }
        }
    }

    #[test]
    fn benign_steering_is_non_escalating() {
        // The benign trip NEVER escalates: a status read hammered far past
        // every old escalation threshold keeps steering with the same
        // Warning; the old ladder (Block at >max, Break at >=max+2 /
        // count >= 7) is disarmed for benign tools at every level.
        let mut det = LoopDetector::new(default_config());
        let args = json!({"scope": "all"});

        for call in 1..=20 {
            match det.record("sop_status", &args, "status: idle") {
                LoopDetectionResult::Warning(msg) => {
                    assert!(
                        msg.contains(BENIGN_REPEAT_STEERING_HINT),
                        "call {call}: {msg}"
                    );
                }
                LoopDetectionResult::Ok if call < 3 => {}
                other => panic!(
                    "call {call}: benign steering must stay a Warning forever, got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn benign_memory_recall_exact_repeat_steers_not_breaks() {
        // memory_recall is benign (the ruling's list): its identical-args
        // repeats steered. This re-anchors the old exempt-vs-exact-repeat
        // contract — exact-repeat detection still FIRES, but for a benign
        // tool the trip is the steering Warning, not the Break.
        let mut det = LoopDetector::new(default_config());
        let args = json!({"key_prefix": "proj:cheknet"});

        // Calls 1-2 are below the exact-repeat threshold (max_repeats=3);
        // from the 3rd call on, every trip steers — never Break.
        for call in 1..=9 {
            match det.record("memory_recall", &args, "no memories") {
                LoopDetectionResult::Warning(msg) => {
                    assert!(msg.contains("memory_recall"), "call {call}: {msg}");
                    assert!(
                        msg.contains(BENIGN_REPEAT_STEERING_HINT),
                        "call {call}: {msg}"
                    );
                }
                LoopDetectionResult::Ok if call < 3 => {}
                other => panic!(
                    "call {call}: expected benign steering Warning for memory_recall, got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn benign_memory_recall_no_progress_steers_not_breaks() {
        // The pre-fix red state: the never-wired silent exemption left
        // memory_recall's different-args/identical-result runs exposed to
        // the no-progress Break (a search-probe loop reaped mid-work). The
        // benign class steers it: Warning + hint at every trip level.
        let mut det = LoopDetector::new(default_config());

        for call in 0..7 {
            let args = json!({"key_prefix": format!("attempt_{call}")});
            match det.record("memory_recall", &args, "no results found") {
                r if call < 4 => assert_eq!(r, LoopDetectionResult::Ok, "call {call}"),
                LoopDetectionResult::Warning(msg) => {
                    assert!(msg.contains("memory_recall"), "call {call}: {msg}");
                    assert!(
                        msg.contains(BENIGN_REPEAT_STEERING_HINT),
                        "call {call}: {msg}"
                    );
                }
                other => panic!("call {call}: expected benign steering Warning, got {other:?}"),
            }
        }
    }

    #[test]
    fn write_path_no_progress_kill_retained_for_mqtt_publish() {
        // RCA incident #2 verbatim: "tool 'mqtt_publish' called 7 times
        // with different arguments but identical results — no progress".
        // mqtt_publish is a write-path tool — the ruling RETAINS the
        // run-abort: identical results from writes are a real no-progress
        // loop.
        let mut det = LoopDetector::new(default_config());

        for i in 0..6 {
            let args = json!({"topic": format!("zeroclaw/tasks/{i}"), "payload": "x"});
            det.record("mqtt_publish", &args, "published");
        }
        match det.record(
            "mqtt_publish",
            &json!({"topic": "zeroclaw/tasks/6", "payload": "x"}),
            "published",
        ) {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("mqtt_publish"), "{msg}");
                assert!(msg.contains("no progress"), "{msg}");
                assert!(!msg.contains(BENIGN_REPEAT_STEERING_HINT), "{msg}");
            }
            other => panic!("write-path no-progress Break must be retained, got {other:?}"),
        }
    }

    #[test]
    fn write_path_exact_repeat_kill_retained_for_file_write() {
        // file_write identical-args repeats stay on the kill ladder:
        // Block at the 4th consecutive call, Break at the 5th.
        let mut det = LoopDetector::new(default_config());
        let args = json!({"path": "/tmp/ledger.toml", "content": "same"});

        for _ in 0..3 {
            det.record("file_write", &args, "written");
        }
        match det.record("file_write", &args, "written") {
            LoopDetectionResult::Block(msg) => {
                assert!(msg.contains("file_write"), "{msg}");
            }
            other => panic!("expected exact-repeat Block at 4th call, got {other:?}"),
        }
        match det.record("file_write", &args, "written") {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("Circuit breaker"), "{msg}");
                assert!(msg.contains("file_write"), "{msg}");
            }
            other => panic!("expected exact-repeat Break at 5th call, got {other:?}"),
        }
    }

    #[test]
    fn mixed_benign_steering_does_not_disarm_the_write_path_kill() {
        // Mixed sequence: benign sop_status trips (steered Warnings) share
        // the window with a write tool's no-progress run — the steering
        // must not disarm the kill for the write-path tool, and the
        // benign entries must not trip anything on their own account.
        let mut det = LoopDetector::new(default_config());

        // Steered benign flailing (would-be incident #1) — warnings only
        // (the first two calls are below the exact-repeat threshold).
        for call in 0..5 {
            let r = det.record("sop_status", &json!({"run_id": "r1"}), "status: running");
            if call < 2 {
                assert_eq!(r, LoopDetectionResult::Ok, "call {call}");
            } else {
                assert!(matches!(r, LoopDetectionResult::Warning(_)), "got {r:?}");
            }
        }
        // A write tool's genuine no-progress loop — the kill ladder must
        // step through its normal escalation (5th: Warning, 6th: Block,
        // 7th: Break) exactly as it would without the benign entries in
        // the window; the steering must not disarm any rung.
        for i in 0..6 {
            let args = json!({"cmd": format!("kubectl get pod p{i}")});
            let r = det.record("shell", &args, "pod not found");
            assert!(
                matches!(
                    r,
                    LoopDetectionResult::Ok
                        | LoopDetectionResult::Warning(_)
                        | LoopDetectionResult::Block(_)
                ),
                "write tool rung {i} must follow the raw ladder, got {r:?}"
            );
        }
        match det.record(
            "shell",
            &json!({"cmd": "kubectl get pod p6"}),
            "pod not found",
        ) {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("shell"), "{msg}");
                assert!(msg.contains("no progress"), "{msg}");
            }
            other => panic!("the write-path Break must survive benign steering, got {other:?}"),
        }
    }

    #[test]
    fn benign_no_progress_counts_interleaved_calls_and_steers() {
        // The interleaved-counting semantics (filter, not take_while) hold
        // for benign steering too: non-adjacent benign repeats with varied
        // unrelated tools interleaved still steer — never abort.
        let mut det = LoopDetector::new(default_config());

        let mut last = LoopDetectionResult::Ok;
        for i in 0..5 {
            let args = json!({"run": format!("probe_{i}")});
            last = det.record("sop_status", &args, "no change");
            det.record(
                &format!("reader_{i}"),
                &json!({"path": format!("/f{i}")}),
                &format!("body_{i}"),
            );
        }
        match last {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("sop_status"), "got: {msg}");
                assert!(msg.contains(BENIGN_REPEAT_STEERING_HINT), "got: {msg}");
            }
            other => panic!("expected steering Warning on 5th interleaved probe, got {other:?}"),
        }
    }
}
