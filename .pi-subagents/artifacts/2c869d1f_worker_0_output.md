### Spec Compliance

- ✅ Spec compliant — all requirements from the brief are implemented: 5 args added to struct + parameter_schema + parsing helper; all 3 `BackgroundDelegateResult` construction sites updated with the new fields; R2 (started) and R3 (terminal) publish blocks present, `#[cfg(feature = "channel-mqtt")]` gated, using `topic_for` + `tokio::spawn(mqtt_bus::publish(...))` with `retain=true`; `unassigned` fallback when project_id/milestone_id None; no `mqtt_bus.rs` changes; no hardcoded secrets; 6 new tests verify real behavior (arg parsing, defaults, empty-string edge cases, non-numeric ttl fallback, topic hierarchy contract, parameter schema inclusion).
- ⚠️ Cannot verify from diff: TDD RED step (the diff only shows the final committed state; the report claims FAIL→PASS but the intermediate RED state is not preserved). Controller independently verified 101 tests pass + builds clean with AND without the feature.

### Strengths

- `parse_delegate_event_args` is cleanly extracted as a pure, standalone function returning a `DelegateEventArgs` struct — makes the arg parsing testable in isolation without constructing a full `DelegateTool` or broker. This is the right decomposition.
- Empty-string-to-None filtering (`.filter(|s| !s.is_empty())`) is a thoughtful edge case that prevents `""` from polluting topic segments — would have produced `zeroclaw/projects//milestones/...` topics otherwise.
- Non-numeric `ttl_seconds` fallback test (`"not_a_number"` → 300) covers a real JSON type-coercion edge case.
- R2 and R3 publish blocks are correctly placed at the exact seams specified (after `write_result_atomic` for started; after `update_status` for terminal).
- The `tokio::spawn(async move { ... })` pattern correctly handles the `'static` lifetime requirement for the spawned publish task — the report notes this was a fix during implementation (topic/payload didn't live long enough for a bare `spawn`).
- `event_args` is cloned once at L1309 for the background task closure, and R2 only borrows field clones before that point — no use-after-move.
- R2 payload preserves `project_id` as `Option<String>` (serialized as `null` or `"p1"`) while the topic uses the `unassigned` fallback — correct separation of payload data vs topic routing.

### Issues

#### Critical (Must Fix)
(none)

#### Important (Should Fix)
(none)

#### Minor (Nice to Have)

1. `delegate.rs:1463` — R3 maps `BackgroundTaskStatus::Running => "completed"` as a defensive default. The brief's code example specified this same mapping, so it's plan-mandated. However, a task reaching the terminal seam in `Running` state is anomalous; silently publishing "completed" could mask a bug. Consider logging a warning or publishing an "unknown" state instead. Non-blocking — the comment says "shouldn't happen at terminal seam" and the brief explicitly chose this mapping.

2. `Cargo.toml` — the `rumqttc` optional dep + `channel-mqtt` feature were added in this commit, but the report notes they were "from Task 4, was uncommitted." Task 4's `mqtt_bus.rs` uses `rumqttc::...` and the feature gate was needed for Task 4's tests to pass. The Task 4 commit (`5a922bfd`) should have included this Cargo.toml change. Process nit, not a code defect — the build works correctly.

3. The configured-path (actual MQTT publish to a real broker) is not unit-tested. This is explicitly deferred to Task 18 (e2e) per the brief, so it's not a gap — but the R2/R3 `tokio::spawn` blocks are only exercised at runtime with a configured broker. The no-op path (mqtt unconfigured) is covered by existing delegate tests passing unchanged.

### Assessment

**Task quality:** Approved

**Reasoning:** All spec requirements are met — 5 args threaded through struct/schema/parsing/construction-sites, R2+R3 publish blocks correctly placed and cfg-gated with `unassigned` fallback, 6 real behavior tests covering parsing/defaults/edge-cases/topic-contract/schema. No Critical or Important issues; the 3 Minor items are a plan-mandated defensive default, a process nit about which commit carried the Cargo.toml change, and the explicitly-deferred integration test.