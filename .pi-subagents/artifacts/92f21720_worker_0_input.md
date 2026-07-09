# Task for worker

You are a delegated subagent running from a fork of the parent session. Treat the inherited conversation as reference-only context, not a live thread to continue. Do not continue or answer prior messages as if they are waiting for a reply. Your sole job is to execute the task below and return a focused result for that task using your tools.

Task:
Implement Task 5 (Rust R2+R3+R4 — delegate publish hooks + arg extensions) of an implementation plan. TDD Rust task in zeroclaw-src.

WORKING DIRECTORY (absolute): /Users/sa/git/zeroclaw-src/.worktrees/event-driven-swarm-engine
Branch: cheknet-patched-v0.8.2-event-driven (already checked out).

READ THIS FIRST — your full requirements, with EXACT code blocks for the publish hooks, EXACT line numbers for the seams, and the struct changes: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-5-brief.md

Where this task fits: Task 4 added `crate::mqtt_bus::publish(topic, payload, retain)` + `topic_for(parts)` (gated on `#[cfg(feature = "channel-mqtt")]`). This task wires it into the background-delegate lifecycle: (R4) extend delegate args with project_id/milestone_id/chain_id/ttl_seconds/session_id; (R2) publish a retained `started` event right after the initial `write_result_atomic`; (R3) publish a retained `completed`/`failed`/`cancelled` event at the terminal seam after `update_status`.

CRITICAL — read the brief carefully. It contains:
- The exact `BackgroundDelegateResult` struct fields to add (line 42-51).
- The exact `tokio::spawn(crate::mqtt_bus::publish(...))` blocks to insert at the two seams (line ~1156 for R2, line ~1326 for R3), wrapped in `#[cfg(feature = "channel-mqtt")]`.
- The exact arg-parsing location (line ~858, near `let background = args.get("background")`).
- The parameter_schema location (line ~758-778) to add the 5 new args.

Global constraints (binding):
- TDD: failing test FIRST (arg parsing + topic contract), then implement, then green.
- `cargo` with `source ~/.cargo/env` first.
- `#[cfg(feature = "channel-mqtt")]` on ALL `mqtt_bus::` references so the crate compiles WITHOUT the feature too.
- Do NOT modify `mqtt_bus.rs` (Task 4, done).
- Graceful degradation: delegate must work exactly as before when mqtt is off.
- Default `ttl_seconds` = 300.
- Update EVERY `BackgroundDelegateResult { ... }` construction site (search for `BackgroundDelegateResult {` — there are sites at the initial write ~L1144 and final write ~L1280; explicitly set all new fields, no `..Default::default()`).

Steps (from the brief):
1. R4: extend struct + all construction sites + parameter_schema + arg parsing.
2. Write failing tests (arg parsing helper + topic contract).
3. `source ~/.cargo/env && cargo test -p zeroclaw-runtime --features channel-mqtt delegate` → FAIL.
4. R2+R3: insert the two publish blocks at the seams.
5. `source ~/.cargo/env && cargo test -p zeroclaw-runtime --features channel-mqtt delegate` → PASS.
6. `source ~/.cargo/env && cargo build -p zeroclaw-runtime --features channel-mqtt` → compiles.
7. `source ~/.cargo/env && cargo build -p zeroclaw-runtime` (NO feature) → compiles (cfg gates hide mqtt_bus).
8. Commit: `feat(runtime): delegate publishes started/completed events + arg extensions (R2 R3 R4)`

REPORT FILE: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-5-report.md with: status, commits, exact `cargo test` output (FAIL then PASS), exact `cargo build` output (with AND without feature), the line numbers of the inserted publish blocks, any concerns. Return only: status, commits, one-line test summary, concerns.

IMPORTANT — do NOT stop mid-task. Complete ALL steps through the commit and report. If a test fails unexpectedly, debug it (read the compile error, fix, retry) — do not return DONE with failing tests or a non-compiling crate. If you cannot resolve a compile error after 3 attempts, report BLOCKED with the error text.

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