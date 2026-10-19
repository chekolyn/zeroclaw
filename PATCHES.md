# Patches Log for the `cheknet-patched-v*` Branches

The patched branches carry local custom patches on top of the pinned upstream
ZeroClaw tags for the home-lab deployment. v0.8.4 base: `a56c345d`; the current
branch `cheknet-patched-v0.8.6` pins upstream v0.8.6.

## The structured manifest

The machine-verifiable manifest for the current lineage — the parse surface
the fork's patch checker (`scripts/check_patches.py`) reads. The audit map
(below, "The 2026-10-02 lineage audit") is the discovery truth; these rows
are the lineage contract. The rules:

- Every patch has a row with its CURRENT on-branch sha (`current_sha`). The
  v0.8.4-branch shas are NOT anchors — each lives only in its description's
  provenance note.
- `origin_base` is the upstream tag whose tree the patch content was first
  authored against: `v0.8.4` for the replayed generation, `v0.8.5` for the
  native generation.
- The v0.8.5 realignment was a single squash (`902108a1d`) carrying every
  replayed v0.8.4-era patch as one adapted diff — moved-site replays whose
  per-patch content was verified at HEAD (see the audit map). All 17
  v0.8.4-era rows therefore share the squash's sha as their current anchor.
  The old table's `R2–R4` collapse (`a0f48c2f8`, one commit for all three
  patches) is split into per-patch rows here: R2 the `started` publish site,
  R3 the terminal `completed` publish site, R4 the `DelegateEventArgs`
  parsing — the split follows the old commit's own R2/R3/R4 labels.
- The probe cell names a probe registry key EXACTLY (the checker's `PROBES`
  dict); `-` means no probe. Probes are mandatory for any patch ever
  implicated in a silent drop: the mqtt liveness fix (this row carries
  `mqtt-poll-alive`), U1 (carries `allow_scripts-config-read`), and U2
  (carries `cron-session-route`).
- **Superseded (no row):** the old `adapt` patch (`454f20239`, "adapt cheknet
  patches to v0.8.4 ToolOutput API") is superseded, not dropped — its purpose
  was the v0.8.4 `ToolOutput` adaptation, and the v0.8.5 sites were
  re-adapted inside the realignment squash's conflict resolutions. It has no
  row and no allowlist entry; see the audit map's not-on-this-branch list.

### Applied rows

| id | current_sha | origin_base | description | probe |
|----|-------------|-------------|-------------|-------|
| R1 | 902108a1d | v0.8.4 | feat(runtime): add mqtt_bus publisher helper (`crates/zeroclaw-runtime/src/mqtt_bus.rs`); provenance: v0.8.4-branch `7090d8c28`. | - |
| R2 | 902108a1d | v0.8.4 | delegate publishes the `started` event (retained) to the MQTT bus for the event-driven swarm — the started publish site in `delegate.rs` (`projects/{project}/milestones/{milestone}/tasks/{task_id}/started`); provenance: the v0.8.4-branch `R2–R4` collapse `a0f48c2f8` (one commit carried all three patches — split per patch here). | - |
| R3 | 902108a1d | v0.8.4 | delegate publishes the terminal `completed` event to the MQTT bus — the terminal publish site in `delegate.rs`; provenance: the v0.8.4-branch `R2–R4` collapse `a0f48c2f8`. | - |
| R4 | 902108a1d | v0.8.4 | the event-driven delegate args — `DelegateEventArgs`/`parse_delegate_event_args` (project/milestone/chain/ttl_seconds/session from the tool's JSON args, for the MQTT topic hierarchy and the result struct); provenance: the v0.8.4-branch `R2–R4` collapse `a0f48c2f8`. | - |
| wiring | 902108a1d | v0.8.4 | wire `mqtt_bus::init()` into the daemon entry + forward channel-mqtt to runtime (call site `src/main.rs`); provenance: v0.8.4-branch `1a564c72d`. | - |
| delegate-results_dir | 902108a1d | v0.8.4 | fix(delegate): make `results_dir` config-overridable (upstreamable); provenance: v0.8.4-branch `4bc701acf`. | - |
| R5 | 902108a1d | v0.8.4 | feat(runtime): `mqtt_publish` tool for agent/SOP event publishing; provenance: v0.8.4-branch `b04266372`. | - |
| R6 | 902108a1d | v0.8.4 | feat(tools): `memory_store` append + `memory_recall` prefix; provenance: v0.8.4-branch `57568bc62`. | - |
| gateway | 902108a1d | v0.8.4 | feat(gateway): dynamic webhook route registration from config; provenance: v0.8.4-branch `b4eb7ed10`. | - |
| delegate-ttl_seconds | 902108a1d | v0.8.4 | fix(delegate): `ttl_seconds` parameter + the loop-detector false-positive fix; provenance: v0.8.4-branch `efd448d8f`. | - |
| t6b-task-events | 902108a1d | v0.8.4 | M2 T6b — typed `Task*` events on fixed topics (`zeroclaw/tasks/{started,completed,failed}`), non-retained, IDs in payload; provenance: v0.8.4-branch `3f4dfb5d5`. | - |
| otel-w3c-bridge | 902108a1d | v0.8.4 | the tracing-opentelemetry bridge + W3C TraceContext/Baggage composite propagator (`zeroclaw-log/src/otel_bridge.rs`); provenance: v0.8.4-branch `bcdac41f0` + `34a99fd1d`. | - |
| migrate-fixes | 902108a1d | v0.8.4 | migration fixes — permissive operator policy on the postgres path + V3 agent metadata preservation; provenance: v0.8.4-branch `17f07cb01` + `135e89839`. 2026-10-18 realignment note: the lib.rs policy half was SUPERSEDED by upstream v0.8.6's own migration rework (`build_memory_with_storage` + the permissive operator config + audit suppression) — the realignment adopted upstream's implementation there; the `migration.rs` V3-agent-metadata half still carries. | - |
| runs-fixes | 902108a1d | v0.8.4 | file_read directory listing, run pruning, stuck-run reaper, cron-load Option; provenance: v0.8.4-branch `1b74f1f35`. | - |
| finished-runs-cap | 902108a1d | v0.8.4 | finished_runs capped to `max_finished_runs` on restore + the maintenance tick; provenance: v0.8.4-branch `b6bfb8c78`. | - |
| skip-at-dispatch | 902108a1d | v0.8.4 | sidecar SOP skip-at-dispatch — no run created (present at HEAD despite the squash's "deferred" note); provenance: v0.8.4-branch `4cd7afde4`. 2026-10-18 note: upstream v0.8.6 adopted the `SopRunAction::Skipped { sop_name, reason }` variant into types.rs; the fork's dispatch-side skip logic (`det_*_sop` builders + the skip tests) still carries on top. | - |
| M3AX-1 | 3b6b7dcd0 | v0.8.5 | memory Phase 0.5 — three-axis recall + namespace/importance store + exclude config + the recall SELECT fix. | - |
| M3AX-2 | a99dfb251 | v0.8.5 | memory Phase 0.5 — tool params (namespace/importance on store; category/namespace/key_prefix/scope on recall) + btree index on key. | - |
| M3AX-3 | f136f7373 | v0.8.5 | memory Phase 0.5 — three-axis in-memory filtering in `memory_recall` `execute()`. | - |
| GLM53-EFFORT | 440213153 | v0.8.5 | provider — `thinking.budget_tokens` to OpenAI-compatible chat completions + `reasoning_effort`-max; rides: the stray `zeroclaw_runtime/` dir removed. 2026-10-19 ADJUDICATED — the patch STAYS and is load-bearing on v0.8.6: pristine v0.8.6's openai chat_completions provider NEVER serializes `request.thinking` (every construction site is `thinking: None`; #10605's passthrough covers the Anthropic/Responses paths, not the chat_completions wire), so this patch is the only bridge between upstream's config-side thinking system ([runtime_profiles.*.thinking] level=max + budget_tokens → request.thinking) and the Crusoe GLM wire format. The config already rides upstream's system; this patch is its wire half. | - |
| M3AX-WIRE | 2aabeaef2 | v0.8.5 | complete three-axis recall exclusion wiring. | - |
| DELEGATE-EXCL | e399f82e3 | v0.8.5 | config-sourced recall excludes threaded into bounded delegate memory tools. | - |
| PG-BOUNDS | 43fc2195c | v0.8.5 | bound PostgreSQL memory ops — `statement_timeout`, keepalives, op timeout. | - |
| PG-SHARE | caab7d8bd | v0.8.5 | share one backend instance per config across agent constructions. | - |
| PG-BLOCKPOOL | 2c1471274 | v0.8.5 | construct backends on the blocking pool, not async workers. | - |
| PG-CACHE-GATE | ff17b61be | v0.8.5 | gate resolutions — `backend_cache` default to postgres; bound construction await. | - |
| OTEL-ONCE | 5d26cbb02 | v0.8.5 | construct the OTel pipeline once per process. | - |
| PG-FLOAT8 | 1e4733cdd | v0.8.5 | type the importance param as FLOAT8 in the postgres store. | - |
| L2B-NOTIFY | 97dbe3eaa | v0.8.5 | Lever-2b — the engine-side completion-notification directive (quorum PROCEED 3/3, `delegate-bg-notify-injection`). | - |
| MQTT-POLL-ALIVE | f9ccd8374 | v0.8.5 | the per-poll liveness stamp — defect #2, the mqtt flap root cause (the channels crate); the pre-audit prose carried no sha, so the probe is this patch's mandatory re-pin anchor. | mqtt-poll-alive |
| RIDE-WEBHOOK-TEST | 927c73a65 | v0.8.5 | ride-along — `WebhookConfig.signature_header` lib-test initializer gap (same class/fix as v0.8.4's `109b9a6ca`). | - |
| ADE-1 | fdaa98278 | v0.8.5 | arg_deny_exemptions — deny-entry table + `arg_deny_exemptions` field (RED groundwork). | - |
| ADE-2 | 66f7197eb | v0.8.5 | arg_deny_exemptions — schema + fail-loud parse-time validation. | - |
| ADE-3 | 0577169f0 | v0.8.5 | arg_deny_exemptions — config threading + escalation-subset variant. | - |
| RIDE-FMT | 150b4f73a | v0.8.5 | ride-along — pre-existing fmt drift (F3's `EscalationViolation` variant initializer). | - |
| ADE-4 | c464f6c24 | v0.8.5 | arg_deny_exemptions — `is_args_safe` table-driven + exemption consult (python -c unblocked). | - |
| ADE-5 | 7c184b8e9 | v0.8.5 | arg_deny_exemptions — policy-state-computed denial messages (reason-threading). | - |
| ADE-6 | 503b59893 | v0.8.5 | arg_deny_exemptions — classifier mirrors ReadOnly autonomy (no misdirecting suggestion on read-only profiles). | - |
| ADE-7 | 7ddef6c19 | v0.8.5 | arg_deny_exemptions — the complete §1.9 deny matrix (both dialect entry points, map absent + present). | - |
| U1 | bd538814a | v0.8.5 | the `allow_scripts` subagent propagation fix (delegated subagents skipped skills containing script files — `skills.allow_scripts` was not propagated from the parent config). Carried as the upstreamed claim "upstream `352672c0`" since the v0.8.3-era table; verified dead 2026-10-02 (the origin patch-id absent from v0.8.4's patch-id set, the tree probe failing on v0.8.4/v0.8.5/the current tree) and resolved by the C.1 remediation: cherry-picked from `cheknet-pached` `352672c02` onto v0.8.5, both `load_skills_from_directory` scan sites reshaped to the v0.8.5 loader (the tuple return) and reading the root config snapshot per site; the origin commit's ws.rs marker cleanup and .dockerignore excludes dropped — never applicable on this lineage (the excludes already ride `9d17119a8`). | allow_scripts-config-read |
| U2 | 8fa54cdec | v0.8.5 | the cron chat-leak fix (automated cron outputs routed exclusively to the `cron` session, not broadcast to all chat WebSockets). Carried as the upstreamed claim "upstream `69dd83ed`" since the v0.8.3-era table; verified dead 2026-10-02 (the origin patch-id absent from v0.8.4's 1350-commit patch-id set, the fix shape absent from the v0.8.4/v0.8.5 trees) and resolved by the C.1 remediation: cherry-picked from `cheknet-pached` `69dd83ed4` onto v0.8.5 — `event_matches_session`'s no-session arm routes `cron_result` to the `cron` session alone, and v0.8.5's own `is_global_chat_event` whitelist (the leaky arm's old shape) is removed with its only caller replaced. | cron-session-route |
| SOP-POISON-QUARANTINE | 57f410566 | v0.8.5 | the SOP ledger poison-row quarantine (2026-10-03..05 restore-wedge incident, root cause of the recurring "execution slots full" outage): `load_active_runs`/`load_terminal_runs` aborted wholesale on one unparseable row (a torn WAL write or legacy-schema row), so boot restore rehydrated nothing — the in-memory stuck-run reaper never saw the orphans and the dead process's stale `sop_claims` blocked the start-gate until lease expiry. Fix: tolerant scans that quarantine poison rows terminally (minimal parseable tombstone, stale claim released, `run_quarantined` forensic event — the tombstone also heals the sidecar's `json_extract` readers on the shared DB), a torn-claim-row release in the expired-claims reaper, and the `reaped_stuck_runs` maintenance-summary reporting fix (hardcoded 0 → the real count; the pre-existing reap test was RED on HEAD). 5 regression tests; the 4 remaining sop:: failures are pre-existing lineage debt (verified red on pristine HEAD). | - |
| TUNE-APPLY-LANE | a04aaddd2 | v0.8.5 | the apply-lane guard tunes (2026-10-17 prod evidence: the apply lane dies mid-run to the gateway's own guards — the loop-detector CB and the wall-clock stuck-run reaper): (1) the stuck-run reaper ages runs by the store's `last_progress_at` (idle semantics per the PersistedRun field's own doc) instead of wall-clock `started_at` — prod run `…503289556-0001` was reaped 86s after a healthy step promotion at run-start+60.75min (`stuck_run_timeout_secs=3600`); fallback to `started_at` for runs with no store row yet; (2) `SopIngressOutcome::describe()` + the MQTT listener logs the previously-discarded dispatch outcome (WARN on Unavailable, INFO otherwise) — a dedup'd `Skipped` was indistinguishable from a dead activation and manufactured a false outage hunt; (3) the loop-detector ping-pong kill threshold becomes config (`loop_detection_kill_cycles` on PacingConfig, default 6 preserved: kill at N, block at N−1, warn below, clamped to the 4-cycle detection floor) — a legitimate 6-cycle edit→test coder loop (`file_write`/`shell` alternation) was aborted mid-run. TDD red→green on all three (`reap_stuck_running_runs_spares_progressing_run_past_wall_clock`, `describe_*`, `ping_pong_kill_threshold_is_configurable` + the default-preservation pin); zeroclaw-channels + zeroclaw-config compile. | - |
| C2-RECURSIVE-GREP-STEERING | 2f109b4e5 | v0.8.5 | the recursive-grep steering guard (spec: docs/superpowers/specs/2026-10-18-c2-unscoped-grep-steering-design.md, quorum-approved R3-clean): one boolean rule — any grep-family invocation with a recursive flag (-r/-R incl. combos) is refused PRE-SPAWN and redirected to the native content_search with the extracted pattern as a ready-to-call suggestion (flag parity: -i→case_sensitive:false, -l→files_with_matches). The detector reuses policy.rs's QuoteState segmentation (the DRY ruling); the refusal sits in shell.rs's pre-execution path gated by [shell_tool].steer_recursive_grep (read DIRECTLY from root_config — the anti-shadow decision). 16 unit tests; RED-first (the quoted-pattern split caught the exact quote-awareness class §3.1 specified). | - |
| CRON-SHELL-TIMEOUT | 409b506a4 | v0.8.5 | the cron shell-job wall-clock bound 120 -> 900 (2026-10-18 prod evidence: the coder-test-health-sweep's coverage suite died at the hardcoded 120s every 6h, 5+ consecutive runs; the suite legitimately runs minutes on the in-pod ARM hardware; the fleet's shell crons are otherwise sub-second probes — the bound only matters for hung jobs, now 15 min to clear instead of 2). The per-job config knob was built and reverted — the operator's one-line call (the spine's abstraction-on-the-3rd-repeat doctrine). Gates: cargo test -p zeroclaw-runtime --lib cron 266/0. | - |
| READ-SCAN-WARN-DEDUP | 89690ae73 | v0.8.5 | the read-side scan WARN dedup (2026-10-18 prod evidence: one daily-synthesis recall emitted 17 identical "memory entry withheld from read results" WARN records in a single second — pure log noise): `filter_recalled` aggregates per-recall into ONE record carrying `count`, the unique `kinds`, and up to three `sample_keys` (`summarize_withheld`, pinned by 2 unit tests); the single-entry `get` path keeps its per-entry record. The message text moves to plural ("memory entries withheld…"). | - |
| LOOP-DETECTOR-STEERING | 23fff82f1 | v0.8.5 | the loop-detector's benign-repeat CB class (RCA fix #4 — the 2026-10 verify-improvement SOP runs reaped mid-work by circuit breakers, their real work done): `benign_repeat_tools` (`[pacing]` config, default sop_status/cron_list/memory_recall) — an identical-results trip on a benign tool (the exact-repeat arm OR the no-progress arm) becomes a non-escalating Warning carrying the steering hint ("the tool's result is unchanged; the step's work may be complete — end the turn"), injected into the agent's next prompt via the existing `[Loop Detection]` system-message path, instead of Block/Break; the run-abort ladder retained untouched for write-path tools (file_write/shell/mqtt_publish, the non-benign default — mixed sequences pinned: benign steering never disarms the write-path kill). Rides: the dead `no_progress_exempt_tools` consult wired (declared since v0.8.4, never consulted — its exemption test red on pristine HEAD) and memory_recall moved from the silent-exempt default to the benign class (steering is strictly stronger: no abort in both detectors + the hint; the exempt default pinned against silent return). 12 detector/config unit tests + 1 end-to-end collect test; the 4 remaining sop:: failures are pre-existing lineage debt (verified red without this change). 2026-10-18 v0.8.6 realignment reconciliation: upstream v0.8.6 pinned the no-progress ladder to cap at Block ("never terminates the turn") — the fork's v0.8.5-era Break arm at min_calls+2 is dropped in the realignment (upstream's no-progress tests pin the cap); the kill cases stay owned by the idle-aware stuck-run reaper (TUNE-APPLY-LANE) and the config-gated ping-pong kill_cycles. The benign-repeat steering class + the no_progress_exempt_tools consult carry unchanged. | - |
| DELEGATE-SUPERSET-SETTLEMENT | bb2e2c4cc | v0.8.6 | the background-delegate settlement artifact stays the sidecar-facing superset: `complete/settle_background_task` serialize the fork's `BackgroundDelegateResult` (agent/status + the R4 event fields as top-level keys — the `delegate_results/{task_id}.json` contract the zc-engine sidecar's follow-up loop reads), the worker's terminal path settles through the tool's INJECTED task store (upstream's control-plane-first lifecycle), and the reaper's terminal-settlement identity check parses task_id/output minimally (a deny-unknown-fields lean parse rejected every fork-settled artifact at recovery). Upstream's reader consumes the superset via its legacy branch — back-compat holds both ways. | - |

### Upstreamed rows

(None. The two historical claims were resolved 2026-10-02 by the C.1
remediation — U1 and U2 moved U→R, the applied rows above: both were
carried as "upstreamed by v0.8.4" since the v0.8.3-era table with their
origin commits on `cheknet-pached`; both origin patch-ids were verified
absent from v0.8.4's patch-id set and both fix shapes absent from the
v0.8.4/v0.8.5 trees — the checker's `check_upstreamed` machinery — so
the content was replayed onto this lineage instead.)

`TBD-RESOLVE` marks an unresolved upstreamed claim — a checker failure
(FAIL_U, exit 1), never a pass and never an error exit; the checker never
edits anything. Proof obligation per row: the origin commit's patch-id in
the asserted tag's patch-id set OR the row's probe against the tag's tree.

The 2026-10-18 v0.8.6 realignment recorded the first genuine upstream
landing:

| id | origin_commit | asserted_in | upstream_sha | probe | description |
|----|---------------|-------------|--------------|-------|-------------|
| sop-headless-drivers | ea6d8713f | v0.8.6 | 2f5e85041 | sop-active-scope | the SOP headless-driver system (driver ownership/supervision, step scoping, headless runs; `sop/active_scope.rs` + the sop tree). The v0.8.4-branch series (`c776c7239..ea6d8713f`, 19 commits, merged via `ffec91f64`) went upstream via #9494 and landed as #9841 ("drive headless SOP runs, and close the five defects found reviewing #9494") — `active_scope.rs` is byte-identical in the v0.8.6 tree. The v0.8.6 realignment adopts upstream's evolved implementation (`RegisteredSopDriver`, generation-owned teardown, orphan-run settlement) and drops the fork's v0.8.5-era variant. Proof: the `sop-active-scope` probe against the v0.8.6 tree. |

## Checker allowlist

The class-c set — the realignment fixups + tooling + docs, outside the
applied rows, allowlisted with reasons from the audit map's vocabulary
(`realignment-fixup`, `tooling`, `docs`). The bijection with the audit map's
class-c rows is enforced (FAIL_A); an allowlisted sha that also has an
applied row is a misclassification failure.

allowlist: 9d17119a8 — tooling: Docker build context excludes (.worktrees/ + target/; 225GB → ~1GB)
allowlist: 7f6cfef38 — realignment-fixup: missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
allowlist: cce19e948 — realignment-fixup: exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray zeroclaw_runtime/ add)
allowlist: 39edf51f5 — realignment-fixup: namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
allowlist: b1a1c9fa7 — realignment-fixup: webhook_secret_hash added to the AppState initializer (E0063 missing field)
allowlist: 9eeed5e26 — realignment-fixup: HashMap::new() for generic_webhook fields (setup code lost in realignment)
allowlist: 4bc947057 — realignment-fixup: #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
allowlist: a646c7da6 — realignment-fixup: webhook_secret_hash None unconditional in the initializer (field not #[cfg]-gated)
allowlist: 85228ece8 — realignment-fixup: Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
allowlist: 7c09b8026 — docs: PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
allowlist: 7f574e28a — docs: PATCHES.md — the arg_deny_exemptions entry
allowlist: 61e2163d4 — realignment-fixup: the v0.8.6 conflict resolutions compiled coherent (upstreamed headless/migration/steering surfaces adopted; the fork carries merged; AppState/LoopDetectorConfig/memory-entry test fixtures threaded; the loop-detector knobs both-threaded)
allowlist: 6a3530e48 — realignment-fixup: the v0.8.6 test-suite reconciliation (the no-progress ladder adopts upstream's Block cap — the fork's Break arm dropped, 3 fork tests re-pinned; the ADE exemption validation rehomed per-profile with the alias path prefix; the git delegation check skips table-modeled subcommands; mqtt_publish in OUTSIDE_INVENTORY)

## Realignment notes (v0.8.3 → v0.8.4)

- Strategy A (binding rule `debops-cheknet-rules.md`): new branch
  `cheknet-patched-v0.8.4` created off upstream `v0.8.4`, cheknet patches
  replayed via cherry-pick.
- v0.8.4 changed `ToolResult.output` from `String` → `ToolOutput`; the R5/R6
  tool patches were adapted with `.into()` (commit `454f20239`).
- Build: image tag `v0.8.4.cheknet-patched-v1`, repo
  `registry.home.chekolyn.com/zeroclaw-cheknet-patched-gateway`.
- NOTE: built with thin-LTO / 16 codegen-units (mirrors upstream `[profile.ci]`)
  to fit a low-memory build host; the binary is functionally identical to a
  fat-LTO release. A size-optimal fat-LTO rebuild needs a ≥8GiB build VM.

## Applied patches — cheknet-patched-v0.8.5

- **Lever-2b: the engine-side completion-notification directive** (`97dbe3eaa`, quorum PROCEED 3/3 — `delegate-bg-notify-injection`, debops-cheknet 2026-09-27): `delegate.rs execute_background` gains `apply_bg_notify_directive` — the dedup-guarded append of the notify instruction to every background child's prompt. Idempotency + explicit-re-target-wins: any `sessions_send` already in the dispatcher's prompt (either form) → the injection yields. `execute_sync` untouched. 4 unit tests; the spec: `debops-cheknet/docs/proposals/2026-09-27-delegate-bg-notify-injection-design.md`.
- **Test-target debt repair** (rides the same commit): the loop_.rs test helper mints its registry via the sanctioned `ScopedToolRegistry::assemble` seam; `rpc/context.rs::minimal_with_cert_audit` gains `sop_driver_handles: None`; the engine test's nonexistent `terminal_run_count()` replaced with the `SopRunStore::load_terminal_runs` trait call. The 5 remaining `sop::engine` test failures are pre-existing drift (they never ran before — the lib-test target did not compile).

- **Defect #2 — the mqtt loop's per-poll liveness stamp (the flap root cause)** (2026-09-27, root-caused via systematic debugging): `orchestrator/mqtt.rs`'s event loop stamped the `mqtt` health component ONLY on ConnAck — a healthy persistent connection receives ConnAck exactly once at boot, so `mqtt.last_ok` froze at boot and the staleness liveness probe (the 2026-09-23 wire-death interim, 900s) killed every healthy gateway instance at ~18 min uptime (12 kills/4h prod, 14 canary; `restart_count: 0` throughout — the client never died; the probe commit's premise "last_ok updates on message consumption" was false — Publishes/PingResp never stamped). The fix: `record_poll_alive()` stamps the component on EVERY successful poll (the keepalive PingResp arrives every 30s even when idle) — `last_ok` becomes a true liveness-of-loop metric: a healthy-but-quiet wire never trips the probe; a genuinely hung loop (the original 2026-09-23 wire death) goes stale and the probe fires — the interim mitigation restored to its intended semantics. 2 unit tests (RED-then-GREEN: the stamp + the starting-state clears); the channels suite 1554/1554 + default 1540/1540. **Provenance: `f9ccd8374`** (`cheknet-patched-v0.8.5`, 2026-09-27 — the channels crate: `orchestrator/mqtt.rs` + `orchestrator/mod.rs` + `filesystem.rs`; this prose entry rode in the same commit) — the sha this entry carried nowhere in the pre-audit prose; the anchor the mandatory re-pin probe checks.
- **Test-target debt repair (rides along)**: the channels lib-test target did not compile — 4 mechanical drift fixes (`AgentRouter::multi` calls gained the `sop_driver_sink: None` arg ×2; `ChannelRuntimeContext` initializer gained `sop_driver_sink: None`; `FilesystemChannelConfig` gained `driver_sink: None`).
- **`arg_deny_exemptions` (the python -c unblock)** (2026-09-28; spec: `debops-cheknet/docs/superpowers/specs/2026-09-28-python-c-arg-carveout-design.md`): the argument-deny guard's hardcoded `is_args_safe` arms are reified as a per-arm deny-entry table — `deny_entries_for` (token — predicate — case domain; the table is the single source of truth) — and `is_args_safe` becomes table-driven. `RiskProfileConfig`/`SecurityPolicy` gain `arg_deny_exemptions: HashMap<String, Vec<String>>` (`#[serde(default)]`: absent/empty is byte-for-byte today's behavior, and the currently deployed binary ignores the key, so config-before-image cannot crashloop). Semantics: subtract-only, clause-equality — a value string-equals (case-sensitively, as stored) exactly one deny entry of the keyed arm and removes that entry alone; keys bind to the invoked command's lowercased basename (`python`/`python3` distinct keys). Parse-time validation fails loud: unknown keys AND values matching no deny entry of the keyed arm are config errors (dead-config prevention, the dead-`tee` lesson), plus `*`/empty/whitespace values rejected; the key-set invariant test pins the table so arm drift fails the build. The shell-syntax layer (backticks in any quote state, `$VAR`/`$()`, unsafe redirects, `tee`, unquoted single `&`) is unreachable from the map. Escalation: child entries ⊆ parent (`ArgDenyExemptionExpandedByChild`, the allowlist direction — NOT `forbidden_paths`). Denial messages are computed from live policy state (`CommandRejection` + `suggestion(&policy)`) so a profile without the exemption never claims `-c` is allowed — the misdirect-retry class (the 88-retry `memory-purge` loop) this patch exists to kill. Friction-not-containment framing is binding in every comment/message that touches this: the container is the only hard boundary; the arg layer is friction + auditability.
  Touchpoints: `crates/zeroclaw-config/src/policy.rs` (the deny-entry table + `DenyEntry`/`DenyPredicate`, the `arg_deny_exemptions` field + `Default` impl, `from_profiles` threading, the `ensure_no_escalation_beyond` child⊆parent check, the `classify_args_safety` consult + the `classify_*` reason-threading); `crates/zeroclaw-config/src/schema.rs` (the `RiskProfileConfig.arg_deny_exemptions` field + `validate_arg_deny_exemptions`, wired into the config-parse error path); `crates/zeroclaw-runtime/src/tools/shell.rs` (the `rejection.suggestion(&self.security)` site — the message text itself lives in policy.rs).
  - **RE-PIN (the deny-entry table):** on upstream re-pin, re-derive the deny-entry table from the new `is_args_safe` arms and extend the matrix (the key-set invariant test catches arm-level drift; entry-level drift needs this procedure).
  - **RE-PIN (the sandbox posture):** on re-pin, RE-VERIFY the sandbox posture and pin `sandbox_backend` explicitly if auto-bwrap is unwanted — upstream's post-v0.8.5 sandbox rework moves bubblewrap into the Linux Auto chain, and on our Landlock-less kernel Auto would select it once the binary exists (the image ships it).
  - **schemars `schema-export`:** the exported-config-schema question was CHECKED at plan time (2026-09-28): no committed artifact exists to regenerate (the fork serves the schema dynamically from the compiled binary); re-check on re-pin.
  - **Ride-along repairs made in passing by this series:** the pre-existing `WebhookConfig.signature_header` lib-test compile repair (the webhook roundtrip test literal predates the field — unblocks the `cargo test -p zeroclaw-config --lib` target; same class and fix as `109b9a6ca` on cheknet-patched-v0.8.4) and the pre-existing fmt-drift style fix (F3's `EscalationViolation` variant initializer).

- **The benign-repeat CB class (RCA fix #4)** (`78a030d63`, debops-cheknet 2026-10-07): the 2026-10 verify-improvement SOP runs died 3× to circuit breakers AFTER completing their real work — "tool 'sop_status' called 5 times consecutively with identical arguments" (exact-repeat Break), "tool 'mqtt_publish' called 7 times with different arguments but identical results — no progress" (no-progress Break), "tools 'shell' and 'file_write' have been alternating …" (ping-pong Break). The CB treated the agent's confused-retry flailing on idempotent status/reads (the SOP's own tool-contract bugs, since fixed) as a run-killer; the residual risk — any future SOP-instruction drift re-arms the same death class — is what this patch disarms for the benign class. `LoopDetectorConfig`/`PacingConfig` gain `benign_repeat_tools` (`[pacing]`, `#[serde(default)]`, default `sop_status`/`cron_list`/`memory_recall`): an identical-results trip on one of these tools — the exact-repeat arm OR the no-progress arm — becomes a non-escalating `Warning` carrying the steering hint ("the tool's result is unchanged; the step's work may be complete — end the turn"), riding the existing Warning → `[Loop Detection]` system-message path (`collect_tool_results`) into the agent's next prompt — never Block (the call is never refused), never Break (the run is never aborted). The write-path kill is retained verbatim (file_write/shell/mqtt_publish, the non-benign default): identical results from writes remain a real no-progress loop; mixed sequences are pinned (benign steering never disarms the write-path ladder) and the ruling's scope is binding — the incident's mqtt_publish/shell/file_write death classes stay kills by design, and ping-pong (an alternation trip, not an identical-results trip) is untouched.
  Rides (same file, same death class): (1) the dead `no_progress_exempt_tools` consult wired — declared since v0.8.4, never consulted in `detect_no_progress` (its exemption test was RED on pristine HEAD), so search tools (`content_search`/`web_search_tool`/`glob_search`) returning "no results" for different queries were exposed to the no-progress breaker; (2) `memory_recall` moved from the silent-exempt default to the benign class — steering is strictly stronger (no abort in BOTH detectors plus the hint) — with both defaults pinned by test so the dead-config return cannot come back silently.
  Touchpoints: `crates/zeroclaw-runtime/src/agent/loop_detector.rs` (the class, the two consults, 12 unit tests), `crates/zeroclaw-runtime/src/agent/turn/results_collect.rs` (the end-to-end steering test — no bail + the hint in history; the production Warning path unchanged), `crates/zeroclaw-runtime/src/agent/turn/mod.rs` (the config threading), `crates/zeroclaw-config/src/schema.rs` (`PacingConfig.benign_repeat_tools` + the serde/Default pins).
  - **RE-PIN (the benign class + the consults):** on upstream re-pin, re-derive the benign/exempt lists from the ruling and VERIFY BOTH CONSULTS SURVIVE the realignment — this file has already once silently lost declared detector wiring (the no_progress exemption consult was dead from v0.8.4 through v0.8.5 while its config field and test both shipped); the class pins (`benign_default_class_pins_the_ruling_tools` + the config serde-default test) fail the build if the wiring drops again.

## The 2026-10-02 lineage audit

The discovery enumeration for the fork patch-lineage guarantee (task 0 of the
2026-10-02 patch-lineage work): the complete commit map of the current lineage —
the manifest's input truth. Every commit in
`git rev-list --no-merges v0.8.5..cheknet-patched-v0.8.5` is classified below.
The branch is exactly the `v0.8.5` tag (`cb2b20a9f` — the first commit's parent)
plus these 42 content commits; no content commit is left unclassified (the
mechanism's own surfaces — PATCHES.md / scripts/check_patches.py / Makefile —
ride the checker's reverse-check carve-out, never this map).

lineage: cheknet-patched-v0.8.6 base v0.8.6

reverse-scope: crates/zeroclaw-api, crates/zeroclaw-channels, crates/zeroclaw-config, crates/zeroclaw-gateway, crates/zeroclaw-log, crates/zeroclaw-macros, crates/zeroclaw-memory, crates/zeroclaw-providers, crates/zeroclaw-runtime, crates/zeroclaw-tools, Cargo.toml, Cargo.lock, PATCHES.md, docs/book/src/sop, src/main.rs, tests/component, zeroclaw_runtime

The reverse-scope is the union of every path the class-a/b commits touch
(`git show --stat` over each) — the ten crate trees plus the additional scoped
paths. `zeroclaw_runtime/` is a transient stray zc-engine dir (added in the
realignment window, removed by `440213153`; not in the HEAD tree but a
recorded on-branch touch).

**Method and the patch-id truth.** `git patch-id --stable` was computed for the
old table's nine shas (R1 through `adapt`) and for all 40 current commits:
**zero matches**. The v0.8.5 realignment was a single squash merge
(`902108a1d`, "realign cheknet patches onto v0.8.5") carrying every replayed
v0.8.4-era patch as one adapted diff — conflict resolutions against v0.8.5's
`ScopedToolRegistry`, `sop_webhook_routes`, and the daemon registry rework, so
these are moved-site replays whose patch-ids legitimately differ. The class-b
rows therefore repeat the squash's sha: it is the current on-branch anchor for
each old patch (the description-based fallback; no replayed v0.8.4-era content
exists in any other current commit — the post-squash commits that repaired the
carrier's conflicts are class-c). Content survival was verified at HEAD per
row (e.g. `mqtt_bus.rs`/`mqtt_publish.rs`, the R2/R3 publish sites in
`delegate.rs`, `zeroclaw/tasks/*` topics, `otel_bridge.rs`,
`directory_listing` in `file_read.rs`, `truncate_finished_runs_to_max`,
`SopRunAction::Skipped`).

**Classes.** `a` — v0.8.5-native patches (authored on this lineage, post-squash);
`b` — replayed v0.8.4-era patches (all carried by `902108a1d`); `c` — realignment
fixups + tooling/docs (allowlisted by the reason in the id column, from the
vocabulary `realignment-fixup`, `tooling`, `docs`).

**Row ids.** The old table's ids are reused where they map (`R1`, `R2-R4`,
`wiring`, `gateway`; the old table's two `delegate` rows are disambiguated as
`delegate-results_dir` and `delegate-ttl_seconds`). New stable ids are minted
for the native generation (class-a) and for the v0.8.4-era series that never had
old-table rows (the class-b rows below `gateway`/`delegate-*`).

**Not on this branch — v0.8.4-era patches with no current counterpart
(each verified at HEAD, none a silent drop):**

- `c7c0dd2c7` (pgvector init-thread fix) — upstream-absorbed: upstream
  `dc2a37ba9` (#10209) is native in v0.8.5.
- `c4768dd66` (resolve SOPs by declared `name` field) — upstream-absorbed:
  v0.8.5 carries #9765's native `load_sop_by_name`.
- `454f20239` (`adapt`) — superseded by this realignment itself: its purpose was
  the v0.8.4 `ToolOutput` adaptation; the v0.8.5 sites were re-adapted inside
  the squash's conflict resolutions.
- `109b9a6ca` (v0.8.4 test-initializer gaps) — superseded by the v0.8.5-native
  ride-along `927c73a65` (same class, same fix).
- Correction to the squash's own message: skip-at-dispatch (`4cd7afde4`) was
  noted "deferred (dispatch.rs kept HEAD)" but its content IS present at HEAD
  (`SopRunAction::Skipped` + the deterministic-skip tests, introduced by
  `902108a1d`) — carried, class-b row `skip-at-dispatch`.

| sha | class | row_id_or_allowlist | description |
|-----|-------|---------------------|-------------|
| 902108a1d | b | R1 | old-table R1 — `feat(runtime): add mqtt_bus publisher helper` (`crates/zeroclaw-runtime/src/mqtt_bus.rs`) |
| 902108a1d | b | R2-R4 | old-table R2–R4 — delegate publishes started/completed events + arg extensions |
| 902108a1d | b | wiring | old-table wiring — mqtt_bus::init() into the daemon entry + channel-mqtt feature unification (call site `src/main.rs`) |
| 902108a1d | b | delegate-results_dir | old-table delegate — results_dir config-overridable (upstreamable) |
| 902108a1d | b | R5 | old-table R5 — the mqtt_publish tool for agent/SOP event publishing |
| 902108a1d | b | R6 | old-table R6 — memory_store append + memory_recall prefix |
| 902108a1d | b | gateway | old-table gateway — dynamic webhook route registration from config |
| 902108a1d | b | delegate-ttl_seconds | old-table delegate — ttl_seconds parameter + loop-detector false-positive fix |
| 902108a1d | b | t6b-task-events | M2 T6b — typed Task* events on fixed topics (`zeroclaw/tasks/{started,completed,failed}`), non-retained, IDs in payload |
| 902108a1d | b | otel-w3c-bridge | the tracing-opentelemetry bridge + W3C TraceContext/Baggage composite propagator (`zeroclaw-log/src/otel_bridge.rs`) |
| 902108a1d | b | migrate-fixes | migration fixes — permissive operator policy on the postgres path + V3 agent metadata preservation |
| 902108a1d | b | runs-fixes | v0.8.4-era `1b74f1f35` — file_read directory listing, run pruning, stuck-run reaper, cron-load Option |
| 902108a1d | b | finished-runs-cap | v0.8.4-era `b6bfb8c78` — finished_runs capped to max_finished_runs on restore + maintenance tick |
| 902108a1d | b | skip-at-dispatch | v0.8.4-era `4cd7afde4` — sidecar SOP skip-at-dispatch (present at HEAD despite the squash's "deferred" note) |
| 3b6b7dcd0 | a | M3AX-1 | memory Phase 0.5 — three-axis recall + namespace/importance store + exclude config + recall SELECT fix |
| a99dfb251 | a | M3AX-2 | memory Phase 0.5 — tool params (namespace/importance on store; category/namespace/key_prefix/scope on recall) + btree index on key |
| f136f7373 | a | M3AX-3 | memory Phase 0.5 — three-axis in-memory filtering in memory_recall execute() |
| 9d17119a8 | c | tooling | Docker build context excludes (`.worktrees/` + `target/`; 225GB → ~1GB) |
| 7f6cfef38 | c | realignment-fixup | missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit) |
| cce19e948 | c | realignment-fixup | exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray `zeroclaw_runtime/` add) |
| 39edf51f5 | c | realignment-fixup | namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape) |
| b1a1c9fa7 | c | realignment-fixup | webhook_secret_hash added to the AppState initializer (E0063 missing field) |
| 9eeed5e26 | c | realignment-fixup | HashMap::new() for generic_webhook fields (setup code lost in realignment) |
| 4bc947057 | c | realignment-fixup | #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement) |
| a646c7da6 | c | realignment-fixup | webhook_secret_hash: None unconditional in the initializer (field not #[cfg]-gated) |
| 440213153 | a | GLM53-EFFORT | provider — thinking.budget_tokens to OpenAI-compatible chat completions + reasoning_effort=max; rides: the stray `zeroclaw_runtime/` dir removed |
| 2aabeaef2 | a | M3AX-WIRE | complete three-axis recall exclusion wiring |
| e399f82e3 | a | DELEGATE-EXCL | config-sourced recall excludes threaded into bounded delegate memory tools |
| 43fc2195c | a | PG-BOUNDS | bound PostgreSQL memory ops — statement_timeout, keepalives, op timeout |
| caab7d8bd | a | PG-SHARE | share one backend instance per config across agent constructions |
| 2c1471274 | a | PG-BLOCKPOOL | construct backends on the blocking pool, not async workers |
| 85228ece8 | c | realignment-fixup | Cargo.lock sync — entries missing from the restored v0.8.5 lockfile |
| ff17b61be | a | PG-CACHE-GATE | gate resolutions — backend_cache default to postgres; bound construction await |
| 5d26cbb02 | a | OTEL-ONCE | construct the OTel pipeline once per process |
| 1e4733cdd | a | PG-FLOAT8 | type the importance param as FLOAT8 in the postgres store |
| 97dbe3eaa | a | L2B-NOTIFY | Lever-2b — the engine-side completion-notification directive (quorum PROCEED 3/3, delegate-bg-notify-injection) |
| 7c09b8026 | c | docs | PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair) |
| f9ccd8374 | a | MQTT-POLL-ALIVE | the per-poll liveness stamp — defect #2, the mqtt flap root cause (the channels crate) |
| 927c73a65 | a | RIDE-WEBHOOK-TEST | ride-along — WebhookConfig.signature_header lib-test initializer gap (same class/fix as v0.8.4's `109b9a6ca`) |
| fdaa98278 | a | ADE-1 | arg_deny_exemptions — deny-entry table + arg_deny_exemptions field (RED groundwork) |
| 66f7197eb | a | ADE-2 | arg_deny_exemptions — schema + fail-loud parse-time validation |
| 0577169f0 | a | ADE-3 | arg_deny_exemptions — config threading + escalation-subset variant |
| 150b4f73a | a | RIDE-FMT | ride-along — pre-existing fmt drift (F3's `EscalationViolation` variant initializer) |
| c464f6c24 | a | ADE-4 | arg_deny_exemptions — is_args_safe table-driven + exemption consult (python -c unblocked) |
| 7c184b8e9 | a | ADE-5 | arg_deny_exemptions — policy-state-computed denial messages (reason-threading) |
| 503b59893 | a | ADE-6 | arg_deny_exemptions — classifier mirrors ReadOnly autonomy (no misdirecting suggestion on read-only profiles) |
| 7ddef6c19 | a | ADE-7 | arg_deny_exemptions — the complete §1.9 deny matrix (both dialect entry points, map absent + present) |
| 7f574e28a | c | docs | PATCHES.md — the arg_deny_exemptions entry |
| 61e2163d4 | c | 61e2163d4 | the v0.8.6 conflict resolutions compiled coherent (upstreamed surfaces adopted, fork carries merged, test fixtures threaded) |
| 6a3530e48 | c | 6a3530e48 | the v0.8.6 test-suite reconciliation — the no-progress ladder adopts upstream's Block cap; ADE validation rehomed per-profile; git delegation check skips table-modeled subcommands |
| bd538814a | a | U1 | the allow_scripts subagent propagation fix — cherry-picked from cheknet-pached `352672c02` (the dead upstreamed-by-v0.8.4 claim, resolved by the C.1 remediation) |
| 8fa54cdec | a | U2 | the cron chat-leak fix — cherry-picked from cheknet-pached `69dd83ed4` (the dead upstreamed-by-v0.8.4 claim, resolved by the C.1 remediation) |
| 57f410566 | a | SOP-POISON-QUARANTINE | the SOP ledger poison-row quarantine — tolerant restore scans + terminal tombstones + the reaped_stuck_runs reporting fix (the 2026-10-03..05 restore-wedge / slots-full root cause) |
| 23fff82f1 | a | LOOP-DETECTOR-STEERING | the loop-detector benign-repeat steering class — identical-results trips on idempotent status/reads steer (Warning + hint) instead of aborting; the write-path kill retained; the dead no_progress_exempt consult wired |
| a04aaddd2 | a | TUNE-APPLY-LANE | the apply-lane guard tunes (2026-10-17 prod evidence: the apply lane dies mid-run to its own guards; sha re-anchored to the PR #3 squash a04aaddd2) — (1) the stuck-run reaper ages runs by the store's `last_progress_at` (idle semantics; the field's own doc) instead of wall-clock `started_at`: the proposer run was killed 86s after a healthy step promotion at run-start+60.75min; (2) the MQTT listener logs the previously-discarded `SopIngressOutcome` (`describe()`) — a dedup'd `Skipped` was indistinguishable from a dead activation and manufactured a false outage hunt; (3) the loop-detector ping-pong kill threshold becomes config (`loop_detection_kill_cycles`, default 6 preserved) — a legitimate 6-cycle edit→test coder loop was aborted mid-run |
| 89690ae73 | a | READ-SCAN-WARN-DEDUP | the read-side scan WARN dedup — filter_recalled aggregates per-recall into ONE record (count + unique kinds + ≤3 sample keys; summarize_withheld pinned by 2 unit tests); the single-entry get path keeps its per-entry record. 2026-10-18 prod evidence: one daily-synthesis recall emitted 17 identical WARNs in a second. |
| 2f109b4e5 | a | C2-RECURSIVE-GREP-STEERING | the recursive-grep steering guard — one boolean rule (grep + recursive flag → refuse + redirect to content_search with the extracted pattern + flag parity); quote-aware detection reusing policy.rs's segmentation; the [shell_tool] off-switch read directly from root_config (the anti-shadow decision) |
| 409b506a4 | a | CRON-SHELL-TIMEOUT | the cron shell-job bound 120 -> 900 — the coverage-sweep class (the coder-test-health-sweep's minutes-long suite died at the hardcoded 120s every 6h; sub-second probes unaffected; the per-job knob reverted for the one-liner) |
| bb2e2c4cc | a | DELEGATE-SUPERSET-SETTLEMENT | the settlement artifact stays the sidecar-facing superset; the recovery identity check parses both shapes; the terminal path settles through the injected store |

Row count: 58 rows over 44 commits — 15 class-b rows (one commit, the squash
carrier), 27 class-a rows (one per commit), 16 class-c rows (one per commit).
The sha column is the 9-char short form (unique; `git cat-file -e` resolves
each); the checker re-verifies every row against the branch.

### The honest starting state — the checker's first recorded real-repo run (2026-10-02)

`python3 scripts/check_patches.py --repo /Users/sa/git/zeroclaw-src` →
**exit 1** — the expected honest red, exactly the predicted three facets and
NOTHING else: U1's dead-link claim + U2's dead-link claim (both FAIL_U,
`TBD-RESOLVE` — the origin patch-ids are absent from v0.8.4's patch-id set,
per the verified rows above) + U1's current-tree probe (FAIL_B: the worktree's
`delegate.rs` still hardcodes `false` at both `load_skills_from_directory`
call sites — 0-of-2, the honest pre-fix red). The liveness probe
(`mqtt-poll-alive`) PASSES on its real path; the mechanism's own commits ride
the carve-out (4 mechanism-exempt); the map-consistency arm is green with the
base-tag arm consulted (the class-c↔allowlist bijection 16=16, no
upstream-era/prior-lineage replay among the allowlisted). This red is the
state the C.1 remediation repairs (U1: cherry-pick `352672c02`, row U→R,
probe retained; U2: resolve the claim or cherry-pick). The verbatim report —
the trust boundary's surface, status lines + the allowlist + the class-c
enumeration (the RED report carries it too):

```
== fork patch-lineage checker ==
repo: /Users/sa/git/zeroclaw-src
manifest: /Users/sa/git/zeroclaw-src/PATCHES.md
lineage: cheknet-patched-v0.8.5 (base v0.8.5) — reverse-scope: 17 path(s)
manifest: OK — 40 applied rows, 2 upstreamed rows, 16 allowlist entries, 54 audit-map rows
branch: OK — HEAD is on 'cheknet-patched-v0.8.5' (the declared lineage branch)
presence: OK — 40/40 applied rows anchored in v0.8.5..HEAD (R1, R2, R3, R4, wiring, delegate-results_dir, R5, R6, gateway, delegate-ttl_seconds, t6b-task-events, sop-headless-drivers, otel-w3c-bridge, migrate-fixes, runs-fixes, finished-runs-cap, skip-at-dispatch, M3AX-1, M3AX-2, M3AX-3, GLM53-EFFORT, M3AX-WIRE, DELEGATE-EXCL, PG-BOUNDS, PG-SHARE, PG-BLOCKPOOL, PG-CACHE-GATE, OTEL-ONCE, PG-FLOAT8, L2B-NOTIFY, MQTT-POLL-ALIVE, RIDE-WEBHOOK-TEST, ADE-1, ADE-2, ADE-3, RIDE-FMT, ADE-4, ADE-5, ADE-6, ADE-7)
reverse: OK — 41 scoped commits: 24 rowed, 13 allowlisted, 4 mechanism-exempt; 0 unmapped
drift: OK — every applied row's touched paths lie within the reverse-scope (17 path(s))
map-consistency: OK — 54 audit-map rows (a/b 24, c 16); rows↔a/b 24=24; class-c↔allowlist bijection 16=16; overlap ∅; no upstream-era/prior-lineage replay among allowlisted (asserted tags + the base tag)
upstreamed: FAIL_U — 2 unproven claim(s):
    row 'U1': TBD-RESOLVE — the upstreamed claim is unresolved (exit 1, never a pass and never exit 2)
    row 'U2': TBD-RESOLVE — the upstreamed claim is unresolved (exit 1, never a pass and never exit 2)
probes: FAIL_B — 1 probe(s) below the floor:
    probe 'allow_scripts-config-read': expected >= 2 match(es) of 'root_config.*allow_scripts' in 'crates/zeroclaw-runtime/src/tools/delegate.rs', found 0
-- allowlist (16) --
  9d17119a8 — tooling: Docker build context excludes (.worktrees/ + target/; 225GB → ~1GB)
  e0c654935 — realignment-fixup: restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  7f6cfef38 — realignment-fixup: missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  cce19e948 — realignment-fixup: exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray zeroclaw_runtime/ add)
  39edf51f5 — realignment-fixup: namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — realignment-fixup: reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — realignment-fixup: mut on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — realignment-fixup: gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  b1a1c9fa7 — realignment-fixup: webhook_secret_hash added to the AppState initializer (E0063 missing field)
  9eeed5e26 — realignment-fixup: HashMap::new() for generic_webhook fields (setup code lost in realignment)
  4bc947057 — realignment-fixup: #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  a646c7da6 — realignment-fixup: webhook_secret_hash None unconditional in the initializer (field not #[cfg]-gated)
  85228ece8 — realignment-fixup: Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — realignment-fixup: Cargo.lock entries completed for the realigned cheknet patch deps
  7c09b8026 — docs: PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  7f574e28a — docs: PATCHES.md — the arg_deny_exemptions entry
-- audit-map class-c (16) --
  9d17119a8 — Docker build context excludes (`.worktrees/` + `target/`; 225GB → ~1GB)
  e0c654935 — restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  7f6cfef38 — missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  cce19e948 — exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray `zeroclaw_runtime/` add)
  39edf51f5 — namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — `mut` on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  b1a1c9fa7 — webhook_secret_hash added to the AppState initializer (E0063 missing field)
  9eeed5e26 — HashMap::new() for generic_webhook fields (setup code lost in realignment)
  4bc947057 — #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  a646c7da6 — webhook_secret_hash: None unconditional in the initializer (field not #[cfg]-gated)
  85228ece8 — Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — Cargo.lock entries completed for the realigned cheknet patch deps
  7c09b8026 — PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  7f574e28a — PATCHES.md — the arg_deny_exemptions entry
result: RED — 2 failure section(s); every patch must map to a row or the allowlist, and the allowlist + class-c enumeration above must be human-verified
```

### The C.1 remediation — the checker's first green run (2026-10-02)

The honest-red state above is repaired, exactly as its closing note
prescribed. Both dead upstreamed claims resolved by replay: U1
cherry-picked from `cheknet-pached` `352672c02` → `bd538814a` (both
`load_skills_from_directory` scan sites now read the root config
snapshot's `skills.allow_scripts` — the probe 2-of-2), U2 from
`69dd83ed4` → `8fa54cdec` (`event_matches_session`'s no-session arm
routes `cron_result` exclusively to the `cron` session; v0.8.5's own
`is_global_chat_event` whitelist — the leaky arm's old shape — removed
with its only caller replaced; the derived probe `cron-session-route`
pinned into the checker's registry). Both rows moved U→R (the U-table is
empty; no TBD-RESOLVE remains), both picks landed as class-a map rows,
and the picks carry the origin commit's third hunk dropped where it was
old-lineage cruft (U1: ws.rs conflict-marker cleanup + .dockerignore
excludes; U2: the marker block the auto-merge re-injected into an
unrelated test — repaired before commit). The verbatim green report:

```
== fork patch-lineage checker ==
repo: /Users/sa/git/zeroclaw-src
manifest: /Users/sa/git/zeroclaw-src/PATCHES.md
lineage: cheknet-patched-v0.8.5 (base v0.8.5) — reverse-scope: 17 path(s)
manifest: OK — 42 applied rows, 0 upstreamed rows, 16 allowlist entries, 56 audit-map rows
branch: OK — HEAD is on 'cheknet-patched-v0.8.5' (the declared lineage branch)
presence: OK — 42/42 applied rows anchored in v0.8.5..HEAD (R1, R2, R3, R4, wiring, delegate-results_dir, R5, R6, gateway, delegate-ttl_seconds, t6b-task-events, sop-headless-drivers, otel-w3c-bridge, migrate-fixes, runs-fixes, finished-runs-cap, skip-at-dispatch, M3AX-1, M3AX-2, M3AX-3, GLM53-EFFORT, M3AX-WIRE, DELEGATE-EXCL, PG-BOUNDS, PG-SHARE, PG-BLOCKPOOL, PG-CACHE-GATE, OTEL-ONCE, PG-FLOAT8, L2B-NOTIFY, MQTT-POLL-ALIVE, RIDE-WEBHOOK-TEST, ADE-1, ADE-2, ADE-3, RIDE-FMT, ADE-4, ADE-5, ADE-6, ADE-7, U1, U2)
reverse: OK — 45 scoped commits: 26 rowed, 13 allowlisted, 6 mechanism-exempt; 0 unmapped
drift: OK — every applied row's touched paths lie within the reverse-scope (17 path(s))
map-consistency: OK — 56 audit-map rows (a/b 26, c 16); rows↔a/b 26=26; class-c↔allowlist bijection 16=16; overlap ∅; no upstream-era/prior-lineage replay among allowlisted (asserted tags + the base tag)
upstreamed: OK — no upstreamed claims declared
probes: OK — allow_scripts-config-read 2/2; mqtt-poll-alive 4/1; cron-session-route 1/1
-- allowlist (16) --
  9d17119a8 — tooling: Docker build context excludes (.worktrees/ + target/; 225GB → ~1GB)
  e0c654935 — realignment-fixup: restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  7f6cfef38 — realignment-fixup: missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  cce19e948 — realignment-fixup: exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray zeroclaw_runtime/ add)
  39edf51f5 — realignment-fixup: namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — realignment-fixup: reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — realignment-fixup: mut on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — realignment-fixup: gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  b1a1c9fa7 — realignment-fixup: webhook_secret_hash added to the AppState initializer (E0063 missing field)
  9eeed5e26 — realignment-fixup: HashMap::new() for generic_webhook fields (setup code lost in realignment)
  4bc947057 — realignment-fixup: #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  a646c7da6 — realignment-fixup: webhook_secret_hash None unconditional in the initializer (field not #[cfg]-gated)
  85228ece8 — realignment-fixup: Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — realignment-fixup: Cargo.lock entries completed for the realigned cheknet patch deps
  7c09b8026 — docs: PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  7f574e28a — docs: PATCHES.md — the arg_deny_exemptions entry
-- audit-map class-c (16) --
  9d17119a8 — Docker build context excludes (`.worktrees/` + `target/`; 225GB → ~1GB)
  e0c654935 — restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  7f6cfef38 — missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  cce19e948 — exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray `zeroclaw_runtime/` add)
  39edf51f5 — namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — `mut` on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  b1a1c9fa7 — webhook_secret_hash added to the AppState initializer (E0063 missing field)
  9eeed5e26 — HashMap::new() for generic_webhook fields (setup code lost in realignment)
  4bc947057 — #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  a646c7da6 — webhook_secret_hash: None unconditional in the initializer (field not #[cfg]-gated)
  85228ece8 — Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — Cargo.lock entries completed for the realigned cheknet patch deps
  7c09b8026 — PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  7f574e28a — PATCHES.md — the arg_deny_exemptions entry
result: GREEN — every patch in the lineage is accounted for; the allowlist + class-c enumeration above is the trust boundary: no drop can pass without appearing on this report
```
