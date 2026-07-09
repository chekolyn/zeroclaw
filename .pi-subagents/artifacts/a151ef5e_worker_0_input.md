# Task for worker

You are a delegated subagent running from a fork of the parent session. Treat the inherited conversation as reference-only context, not a live thread to continue. Do not continue or answer prior messages as if they are waiting for a reply. Your sole job is to execute the task below and return a focused result for that task using your tools.

Task:
Implement Task 4 (Rust R1 — mqtt_bus publisher helper) of an implementation plan. This is a TDD Rust task in the zeroclaw-src repo.

WORKING DIRECTORY (absolute, use this): /Users/sa/git/zeroclaw-src/.worktrees/event-driven-swarm-engine
This is a git worktree on branch `cheknet-patched-v0.8.2-event-driven` (already created for you).

READ THIS FIRST — your full requirements, with exact signatures, design, and source-audit context: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-4-brief.md

Where this task fits: We're adding an outbound MQTT publish helper to the zeroclaw-runtime crate so SOPs and delegate hooks can publish events to the bus. This is the single enabling Rust addition (R1) for the event-driven swarm engine. The MQTT broker (Mosquitto) is already deployed to canary; the existing inbound listener `run_mqtt_sop_listener` consumes events. This task adds the outbound publish path.

Global constraints (binding):
- TDD IRON LAW: failing test FIRST (verify it fails for the right reason), then implement, then green. No production code without a failing test.
- Use `cargo` with `source ~/.cargo/env` first (e.g. `source ~/.cargo/env && cargo test -p zeroclaw-runtime mqtt_bus::tests`).
- Graceful degradation: `publish()` MUST be a no-op (returns Ok) when MQTT unconfigured — never panic, never return Err for "not configured".
- Do NOT modify `daemon/mod.rs` or `delegate.rs` (those are Task 5).
- Match existing code style: `std::sync::LazyLock` for statics, `anyhow::Result`, `zeroclaw_log::record!` for logging (look at how other runtime modules log).

Key context (from source audit — verify these yourself by reading the files):
- `rumqttc` is a dependency of `zeroclaw-channels` but may NOT be a direct dep of `zeroclaw-runtime`. Check `crates/zeroclaw-runtime/Cargo.toml`; if `rumqttc` is missing, add it with the SAME version as in `crates/zeroclaw-channels/Cargo.toml`.
- Existing listener: `crates/zeroclaw-channels/src/orchestrator/mqtt.rs` — uses `rumqttc::{AsyncClient, MqttOptions, QoS, Transport}`. Mirror its MqttOptions construction (broker host/port from broker_url, keep_alive, credentials, TLS when use_tls). Prefer a SELF-CONTAINED broker_url parse in mqtt_bus.rs (don't depend on channels internals).
- `zeroclaw_config::schema::Config` has `config.channels.mqtt: HashMap<String, MqttConfig>` (schema.rs ~line 13554). `MqttConfig`: enabled, broker_url, client_id, qos, username: Option, password: Option, use_tls, keep_alive_secs.
- For the global client use a `static LazyLock<tokio::sync::Mutex<Option<AsyncClient>>>` (or std Mutex — AsyncClient.publish is async so tokio::sync::Mutex is safer). `AsyncClient` is Clone. Use a unique client_id like `format!("{}-pub", cfg.client_id)` to avoid clashing with the listener.
- Add `pub mod mqtt_bus;` to `crates/zeroclaw-runtime/src/lib.rs`.

Exact steps (from the brief):
1. Ensure `rumqttc` + `tokio` (macros feature) deps in zeroclaw-runtime/Cargo.toml.
2. Write failing tests in `mqtt_bus.rs`: `topic_for_sanitizes_segments` (assert `topic_for(&["projects","Foo Bar/Baz"]) == "zeroclaw/projects/foo_bar_baz"`, empty→"zeroclaw/", `+`/`#` replaced) and `publish_noop_when_mqtt_unconfigured` (no init() → publish returns Ok, no panic).
3. Run `source ~/.cargo/env && cargo test -p zeroclaw-runtime mqtt_bus::tests` → FAIL (module missing).
4. Implement `mqtt_bus.rs` (init + publish + topic_for) + add `pub mod mqtt_bus;` to lib.rs.
5. Run `source ~/.cargo/env && cargo test -p zeroclaw-runtime mqtt_bus::tests` → PASS.
6. Run `source ~/.cargo/env && cargo build -p zeroclaw-runtime` → compiles.
7. Commit: `feat(runtime): add mqtt_bus publisher helper (R1)`

REPORT FILE: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-4-report.md with: status, commits (short hashes), exact `cargo test` output (before FAIL + after PASS), exact `cargo build` result, the Cargo.toml dep changes, any concerns. Return only: status, commits, one-line test summary, concerns.

If `cargo` is not found, run `source ~/.cargo/env` first. If a test fails for an unexpected reason, debug it — do not claim DONE with failing tests. TDD means RED then GREEN.

## Acceptance Contract
Acceptance level: checked
Completion is not accepted from prose alone. End with a structured acceptance report.

Criteria:
- criterion-1: Implement the requested change without widening scope

Required evidence: changed-files, tests-added, commands-run, residual-risks, no-staged-files

Finish with a fenced JSON block tagged `acceptance-report` in this shape:
Use empty arrays when no items apply; array fields contain strings unless object entries are shown.
```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "specific proof"
    }
  ],
  "changedFiles": [
    "src/file.ts"
  ],
  "testsAddedOrUpdated": [
    "test/file.test.ts"
  ],
  "commandsRun": [
    {
      "command": "command",
      "result": "passed",
      "summary": "short result"
    }
  ],
  "validationOutput": [
    "validation output or concise summary"
  ],
  "residualRisks": [
    "none"
  ],
  "noStagedFiles": true,
  "diffSummary": "short description of the diff",
  "reviewFindings": [
    "blocker: file.ts:12 - issue found, or no blockers"
  ],
  "manualNotes": "anything else the parent should know"
}
```