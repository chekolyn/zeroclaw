# Task for worker

You are a delegated subagent running from a fork of the parent session. Treat the inherited conversation as reference-only context, not a live thread to continue. Do not continue or answer prior messages as if they are waiting for a reply. Your sole job is to execute the task below and return a focused result for that task using your tools.

Task:
Review Task 4 (Rust R1 — mqtt_bus publisher helper). Read these three files, then produce the review report:

1. Brief: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-4-brief.md
2. Implementer/controller report: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-4-report.md
3. Diff file: /Users/sa/git/zeroclaw-src/.worktrees/event-driven-swarm-engine/.superpowers/sdd/review-9f47bd44..5a922bfd.diff

Base: 9f47bd44, Head: 5a922bfd. Repo: zeroclaw-src (worktree on branch cheknet-patched-v0.8.2-event-driven).

Global constraints (binding, verbatim):
- TDD: failing test FIRST, then implement, then green. No production code without a failing test.
- Graceful degradation: `publish()` MUST be a no-op (returns Ok) when MQTT unconfigured — never panic, never return Err for "not configured".
- `rumqttc` is an optional dep gated on `channel-mqtt` feature (same version 0.25 as zeroclaw-channels). The module must be `#[cfg(feature = "channel-mqtt")]` gated.
- Do NOT modify daemon/mod.rs or delegate.rs (those are Task 5).
- Match existing code style: `std::sync::LazyLock` for statics, `anyhow::Result`, `zeroclaw_log::record!` for logging (the macro takes a single `$msg:expr`, NOT format args — so format strings must be pre-wrapped in `format!(...)`).
- Interface: `pub async fn init(config: &Config) -> Result<()>`, `pub async fn publish(topic: &str, payload: Vec<u8>, retain: bool) -> Result<()>`, `pub fn topic_for(parts: &[&str]) -> String`.
- topic_for: lowercase each segment, replace illegal chars (/, +, #, space, tab, CR, LF, ASCII control) with _, join with /, prefix "zeroclaw/". Empty parts → "zeroclaw/".

Verify the implementer's claims against the diff. Check: module feature-gated correctly, publish is no-op when unconfigured (test covers it), topic_for sanitization matches spec (test covers it), record! macro used correctly (format! wrapped), no daemon/delegate modifications, no hardcoded secrets, init spawns eventloop poll task. This is a task-scoped gate, read-only — do not mutate the tree or run cargo (the report carries the test evidence).

Produce exactly this format:

### Spec Compliance
- ✅ Spec compliant | ❌ Issues found: [details with file:line]
- ⚠️ Cannot verify from diff: [if any]

### Strengths
[specific]

### Issues
#### Critical (Must Fix)
#### Important (Should Fix)
#### Minor (Nice to Have)

### Assessment
**Task quality:** [Approved | Needs fixes]
**Reasoning:** [1-2 sentences]

Begin directly with the spec-compliance verdict. No preamble.

## Acceptance Contract
Acceptance level: attested
Completion is not accepted from prose alone. End with a structured acceptance report.

Criteria:
- criterion-1: Return concrete findings with file paths and severity when applicable

Required evidence: review-findings, residual-risks

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