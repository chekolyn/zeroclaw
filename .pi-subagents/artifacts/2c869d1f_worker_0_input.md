# Task for worker

You are a delegated subagent running from a fork of the parent session. Treat the inherited conversation as reference-only context, not a live thread to continue. Do not continue or answer prior messages as if they are waiting for a reply. Your sole job is to execute the task below and return a focused result for that task using your tools.

Task:
Review Task 5 (Rust R2+R3+R4 — delegate publish hooks + arg extensions). Read these three files, then produce the review report:

1. Brief: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-5-brief.md
2. Implementer report: /Users/sa/git/debops-cheknet/.worktrees/pi-feature-event-driven-swarm-engine/.superpowers/sdd/task-5-report.md
3. Diff file: /Users/sa/git/zeroclaw-src/.worktrees/event-driven-swarm-engine/.superpowers/sdd/review-5a922bfd..ae7b95df.diff

Base: 5a922bfd, Head: ae7b95df. Repo: zeroclaw-src (branch cheknet-patched-v0.8.2-event-driven).

Global constraints (binding, verbatim):
- TDD: failing test FIRST, then implement, then green.
- `#[cfg(feature = "channel-mqtt")]` on ALL `mqtt_bus::` references so the crate compiles WITHOUT the feature too (verified: builds clean both ways).
- Do NOT modify `mqtt_bus.rs` (Task 4, done).
- Graceful degradation: delegate must work exactly as before when mqtt is off (mqtt_bus::publish is a no-op when unconfigured).
- Default `ttl_seconds` = 300.
- 5 new args: project_id, milestone_id, chain_id (all Option<String>), ttl_seconds (u64, default 300), session_id (Option<String>).
- R2: publish retained `started` after the initial write_result_atomic. R3: publish retained `completed`/`failed`/`cancelled` at the terminal seam after update_status.
- Topic hierarchy: `zeroclaw/projects/{proj}/milestones/{ms}/tasks/{task_id}/{state}` with `unassigned` fallback when project_id/milestone_id None.
- Update EVERY BackgroundDelegateResult construction site with the new fields (no ..Default::default()).

Verify the implementer's claims against the diff. Check: all 5 args added to struct + parameter_schema + parsing; all construction sites updated; R2 + R3 publish blocks present, #[cfg(feature="channel-mqtt")] gated, using topic_for + tokio::spawn(mqtt_bus::publish) with retain=true; unassigned fallback; no mqtt_bus.rs changes; no hardcoded secrets; tests are real behavior tests not mocks-that-assert-nothing. Note any construction site missed or any ungated mqtt_bus reference (would break the no-feature build). This is a task-scoped gate, read-only — do not mutate the tree or run cargo (the controller already verified: 101 tests pass, builds clean with AND without feature).

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