# Task for reviewer

Review Task 20 (Rust R6: memory append + prefix) for spec compliance + code quality. READ-ONLY review — do not edit code.

Read these three files in full:
1. Brief (requirements): /Users/sa/git/debops-cheknet/.worktrees/pi-feature-phase2-event-engine/.superpowers/sdd/task-20-brief.md
2. Implementer report: /Users/sa/git/debops-cheknet/.superpowers/sdd/task-20-report.md
3. Review package (the diff + commit): /Users/sa/git/debops-cheknet/.worktrees/pi-feature-phase2-event-engine/.superpowers/sdd/task-20-review-package.md

Worktree to inspect source directly if needed: /Users/sa/git/zeroclaw-src/.worktrees/event-driven-swarm-engine (branch cheknet-patched-v0.8.2-event-driven, R6 commit ac772176, parent 167bda1a). cargo is at /Users/sa/.cargo/bin/cargo if you need to re-run tests (read-only; do NOT modify files).

Review against:
- **Spec compliance:** Does the diff match the brief's scope? (memory_store append param + Memory::store_append default impl; memory_recall prefix param + list+starts_with filter). Does it avoid the plan's two errors (wrong path; unnecessary index table)?
- **TDD discipline:** Were tests written first (RED before GREEN)? The report claims RED was runtime assertion failure (param silently accepted), not compile failure — is that acceptable given the brief asked for compile failure? Judge whether the RED evidence is valid.
- **Correctness:** store_append default impl — JSON-array push, non-array text concat, missing-key→store. Is the JSON edge-case handling sound (non-array JSON, non-JSON content pushed into array, etc.)? prefix handler — list(None,None)+starts_with+take(limit); empty prefix returns "No memories found"; ignores query/since/until/search_mode when prefix set.
- **Regression safety:** Existing memory_store (8) + memory_recall (10) tests still pass — report claims 12+13 green. Any behavior change to existing params when append/prefix unset?
- **No-scope-creep:** Did it touch files outside the 3 expected (memory_traits.rs, memory_store.rs, memory_recall.rs)? Did it touch zeroclaw-gateway (forbidden)? Any image push or debops-cheknet commit (forbidden for this task)?

Return a verdict in this exact shape:
- **Spec compliance:** PASS / FAIL (+ one-line reason)
- **TDD discipline:** PASS / FAIL (+ one-line reason)
- **Correctness:** PASS / FAIL (+ one-line reason with any edge-case concerns)
- **Regression safety:** PASS / FAIL
- **Scope:** PASS / FAIL (list any out-of-scope files touched)
- **Issues:** list each as Critical / Important / Minor with file:line + suggested fix, or "None"
- **Overall:** APPROVE / REQUEST CHANGES

Be rigorous. If the RED-not-compile concern is material, call it out as Important. If the JSON append edge cases have a gap, call it out. This is the gate before Task 21.

## Acceptance Contract
Acceptance level: reviewed
Completion is not accepted from prose alone. End with a structured acceptance report.

Criteria:
- criterion-1: Implement the requested change without widening scope
- criterion-2: Return evidence sufficient for an independent acceptance review

Required evidence: changed-files, tests-added, commands-run, validation-output, residual-risks, no-staged-files

Review gate: required by reviewer.

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