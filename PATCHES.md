# Patches Log for the `cheknet-patched-v*` Branches

The patched branches carry local custom patches on top of the pinned upstream
ZeroClaw tags for the home-lab deployment. v0.8.4 base: `a56c345d`; the current
branch `cheknet-patched-v0.8.5` pins upstream v0.8.5.

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
- The v0.8.5 realignment was a single squash (`f9b2c9fcf`) carrying every
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
| R1 | f9b2c9fcf | v0.8.4 | feat(runtime): add mqtt_bus publisher helper (`crates/zeroclaw-runtime/src/mqtt_bus.rs`); provenance: v0.8.4-branch `7090d8c28`. | - |
| R2 | f9b2c9fcf | v0.8.4 | delegate publishes the `started` event (retained) to the MQTT bus for the event-driven swarm — the started publish site in `delegate.rs` (`projects/{project}/milestones/{milestone}/tasks/{task_id}/started`); provenance: the v0.8.4-branch `R2–R4` collapse `a0f48c2f8` (one commit carried all three patches — split per patch here). | - |
| R3 | f9b2c9fcf | v0.8.4 | delegate publishes the terminal `completed` event to the MQTT bus — the terminal publish site in `delegate.rs`; provenance: the v0.8.4-branch `R2–R4` collapse `a0f48c2f8`. | - |
| R4 | f9b2c9fcf | v0.8.4 | the event-driven delegate args — `DelegateEventArgs`/`parse_delegate_event_args` (project/milestone/chain/ttl_seconds/session from the tool's JSON args, for the MQTT topic hierarchy and the result struct); provenance: the v0.8.4-branch `R2–R4` collapse `a0f48c2f8`. | - |
| wiring | f9b2c9fcf | v0.8.4 | wire `mqtt_bus::init()` into the daemon entry + forward channel-mqtt to runtime (call site `src/main.rs`); provenance: v0.8.4-branch `1a564c72d`. | - |
| delegate-results_dir | f9b2c9fcf | v0.8.4 | fix(delegate): make `results_dir` config-overridable (upstreamable); provenance: v0.8.4-branch `4bc701acf`. | - |
| R5 | f9b2c9fcf | v0.8.4 | feat(runtime): `mqtt_publish` tool for agent/SOP event publishing; provenance: v0.8.4-branch `b04266372`. | - |
| R6 | f9b2c9fcf | v0.8.4 | feat(tools): `memory_store` append + `memory_recall` prefix; provenance: v0.8.4-branch `57568bc62`. | - |
| gateway | f9b2c9fcf | v0.8.4 | feat(gateway): dynamic webhook route registration from config; provenance: v0.8.4-branch `b4eb7ed10`. | - |
| delegate-ttl_seconds | f9b2c9fcf | v0.8.4 | fix(delegate): `ttl_seconds` parameter + the loop-detector false-positive fix; provenance: v0.8.4-branch `efd448d8f`. | - |
| t6b-task-events | f9b2c9fcf | v0.8.4 | M2 T6b — typed `Task*` events on fixed topics (`zeroclaw/tasks/{started,completed,failed}`), non-retained, IDs in payload; provenance: v0.8.4-branch `3f4dfb5d5`. | - |
| sop-headless-drivers | f9b2c9fcf | v0.8.4 | the SOP headless-driver system (driver ownership/supervision, step scoping, headless runs; `sop/active_scope.rs` + the sop tree); provenance: the v0.8.4-branch headless-driver series `c776c7239..ea6d8713f` (19 commits, merged via `ffec91f64`). | - |
| otel-w3c-bridge | f9b2c9fcf | v0.8.4 | the tracing-opentelemetry bridge + W3C TraceContext/Baggage composite propagator (`zeroclaw-log/src/otel_bridge.rs`); provenance: v0.8.4-branch `bcdac41f0` + `34a99fd1d`. | - |
| migrate-fixes | f9b2c9fcf | v0.8.4 | migration fixes — permissive operator policy on the postgres path + V3 agent metadata preservation; provenance: v0.8.4-branch `17f07cb01` + `135e89839`. | - |
| runs-fixes | f9b2c9fcf | v0.8.4 | file_read directory listing, run pruning, stuck-run reaper, cron-load Option; provenance: v0.8.4-branch `1b74f1f35`. | - |
| finished-runs-cap | f9b2c9fcf | v0.8.4 | finished_runs capped to `max_finished_runs` on restore + the maintenance tick; provenance: v0.8.4-branch `b6bfb8c78`. | - |
| skip-at-dispatch | f9b2c9fcf | v0.8.4 | sidecar SOP skip-at-dispatch — no run created (present at HEAD despite the squash's "deferred" note); provenance: v0.8.4-branch `4cd7afde4`. | - |
| M3AX-1 | d3e195993 | v0.8.5 | memory Phase 0.5 — three-axis recall + namespace/importance store + exclude config + the recall SELECT fix. | - |
| M3AX-2 | 2fd2a3e49 | v0.8.5 | memory Phase 0.5 — tool params (namespace/importance on store; category/namespace/key_prefix/scope on recall) + btree index on key. | - |
| M3AX-3 | 3548143b1 | v0.8.5 | memory Phase 0.5 — three-axis in-memory filtering in `memory_recall` `execute()`. | - |
| GLM53-EFFORT | 7be31cc2e | v0.8.5 | provider — `thinking.budget_tokens` to OpenAI-compatible chat completions + `reasoning_effort`-max; rides: the stray `zeroclaw_runtime/` dir removed. | - |
| M3AX-WIRE | 43d274e2a | v0.8.5 | complete three-axis recall exclusion wiring. | - |
| DELEGATE-EXCL | 8e5690fbb | v0.8.5 | config-sourced recall excludes threaded into bounded delegate memory tools. | - |
| PG-BOUNDS | dea4038bf | v0.8.5 | bound PostgreSQL memory ops — `statement_timeout`, keepalives, op timeout. | - |
| PG-SHARE | 355575edf | v0.8.5 | share one backend instance per config across agent constructions. | - |
| PG-BLOCKPOOL | 27eb2d694 | v0.8.5 | construct backends on the blocking pool, not async workers. | - |
| PG-CACHE-GATE | c311d2b76 | v0.8.5 | gate resolutions — `backend_cache` default to postgres; bound construction await. | - |
| OTEL-ONCE | 7fed6e9a8 | v0.8.5 | construct the OTel pipeline once per process. | - |
| PG-FLOAT8 | 90c554280 | v0.8.5 | type the importance param as FLOAT8 in the postgres store. | - |
| L2B-NOTIFY | 7a3640776 | v0.8.5 | Lever-2b — the engine-side completion-notification directive (quorum PROCEED 3/3, `delegate-bg-notify-injection`). | - |
| MQTT-POLL-ALIVE | 9c65570d3 | v0.8.5 | the per-poll liveness stamp — defect #2, the mqtt flap root cause (the channels crate); the pre-audit prose carried no sha, so the probe is this patch's mandatory re-pin anchor. | mqtt-poll-alive |
| RIDE-WEBHOOK-TEST | 1796a6b85 | v0.8.5 | ride-along — `WebhookConfig.signature_header` lib-test initializer gap (same class/fix as v0.8.4's `109b9a6ca`). | - |
| ADE-1 | 998fb082c | v0.8.5 | arg_deny_exemptions — deny-entry table + `arg_deny_exemptions` field (RED groundwork). | - |
| ADE-2 | 1701c56aa | v0.8.5 | arg_deny_exemptions — schema + fail-loud parse-time validation. | - |
| ADE-3 | db0f20af7 | v0.8.5 | arg_deny_exemptions — config threading + escalation-subset variant. | - |
| RIDE-FMT | 1d21f221f | v0.8.5 | ride-along — pre-existing fmt drift (F3's `EscalationViolation` variant initializer). | - |
| ADE-4 | 694db6a2f | v0.8.5 | arg_deny_exemptions — `is_args_safe` table-driven + exemption consult (python -c unblocked). | - |
| ADE-5 | 264e2c293 | v0.8.5 | arg_deny_exemptions — policy-state-computed denial messages (reason-threading). | - |
| ADE-6 | 118afa241 | v0.8.5 | arg_deny_exemptions — classifier mirrors ReadOnly autonomy (no misdirecting suggestion on read-only profiles). | - |
| ADE-7 | 2f32e99bb | v0.8.5 | arg_deny_exemptions — the complete §1.9 deny matrix (both dialect entry points, map absent + present). | - |
| U1 | 33f948e15 | v0.8.5 | the `allow_scripts` subagent propagation fix (delegated subagents skipped skills containing script files — `skills.allow_scripts` was not propagated from the parent config). Carried as the upstreamed claim "upstream `352672c0`" since the v0.8.3-era table; verified dead 2026-10-02 (the origin patch-id absent from v0.8.4's patch-id set, the tree probe failing on v0.8.4/v0.8.5/the current tree) and resolved by the C.1 remediation: cherry-picked from `cheknet-pached` `352672c02` onto v0.8.5, both `load_skills_from_directory` scan sites reshaped to the v0.8.5 loader (the tuple return) and reading the root config snapshot per site; the origin commit's ws.rs marker cleanup and .dockerignore excludes dropped — never applicable on this lineage (the excludes already ride `c91662850`). | allow_scripts-config-read |
| U2 | 957bee7d6 | v0.8.5 | the cron chat-leak fix (automated cron outputs routed exclusively to the `cron` session, not broadcast to all chat WebSockets). Carried as the upstreamed claim "upstream `69dd83ed`" since the v0.8.3-era table; verified dead 2026-10-02 (the origin patch-id absent from v0.8.4's 1350-commit patch-id set, the fix shape absent from the v0.8.4/v0.8.5 trees) and resolved by the C.1 remediation: cherry-picked from `cheknet-pached` `69dd83ed4` onto v0.8.5 — `event_matches_session`'s no-session arm routes `cron_result` to the `cron` session alone, and v0.8.5's own `is_global_chat_event` whitelist (the leaky arm's old shape) is removed with its only caller replaced. | cron-session-route |
| SOP-POISON-QUARANTINE | 26a255fe8 | v0.8.5 | the SOP ledger poison-row quarantine (2026-10-03..05 restore-wedge incident, root cause of the recurring "execution slots full" outage): `load_active_runs`/`load_terminal_runs` aborted wholesale on one unparseable row (a torn WAL write or legacy-schema row), so boot restore rehydrated nothing — the in-memory stuck-run reaper never saw the orphans and the dead process's stale `sop_claims` blocked the start-gate until lease expiry. Fix: tolerant scans that quarantine poison rows terminally (minimal parseable tombstone, stale claim released, `run_quarantined` forensic event — the tombstone also heals the sidecar's `json_extract` readers on the shared DB), a torn-claim-row release in the expired-claims reaper, and the `reaped_stuck_runs` maintenance-summary reporting fix (hardcoded 0 → the real count; the pre-existing reap test was RED on HEAD). 5 regression tests; the 4 remaining sop:: failures are pre-existing lineage debt (verified red on pristine HEAD). | - |

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

## Checker allowlist

The class-c set — the realignment fixups + tooling + docs, outside the
applied rows, allowlisted with reasons from the audit map's vocabulary
(`realignment-fixup`, `tooling`, `docs`). The bijection with the audit map's
class-c rows is enforced (FAIL_A); an allowlisted sha that also has an
applied row is a misclassification failure.

allowlist: c91662850 — tooling: Docker build context excludes (.worktrees/ + target/; 225GB → ~1GB)
allowlist: e0c654935 — realignment-fixup: restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
allowlist: fd1ef7def — realignment-fixup: missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
allowlist: 60b7f1a91 — realignment-fixup: exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray zeroclaw_runtime/ add)
allowlist: 698fd1131 — realignment-fixup: namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
allowlist: 9eab4b59a — realignment-fixup: reaped_stuck_runs placement — method condition + initializer (not field decl)
allowlist: 968691630 — realignment-fixup: mut on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
allowlist: a78b9816e — realignment-fixup: gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
allowlist: 6465b2c4c — realignment-fixup: webhook_secret_hash added to the AppState initializer (E0063 missing field)
allowlist: ccd4b595e — realignment-fixup: HashMap::new() for generic_webhook fields (setup code lost in realignment)
allowlist: 96334076b — realignment-fixup: #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
allowlist: b4535defa — realignment-fixup: webhook_secret_hash None unconditional in the initializer (field not #[cfg]-gated)
allowlist: f2fda50ad — realignment-fixup: Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
allowlist: a1fed21af — realignment-fixup: Cargo.lock entries completed for the realigned cheknet patch deps
allowlist: edf41c818 — docs: PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
allowlist: a05960c1f — docs: PATCHES.md — the arg_deny_exemptions entry

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

- **Lever-2b: the engine-side completion-notification directive** (`7a3640776`, quorum PROCEED 3/3 — `delegate-bg-notify-injection`, debops-cheknet 2026-09-27): `delegate.rs execute_background` gains `apply_bg_notify_directive` — the dedup-guarded append of the notify instruction to every background child's prompt. Idempotency + explicit-re-target-wins: any `sessions_send` already in the dispatcher's prompt (either form) → the injection yields. `execute_sync` untouched. 4 unit tests; the spec: `debops-cheknet/docs/proposals/2026-09-27-delegate-bg-notify-injection-design.md`.
- **Test-target debt repair** (rides the same commit): the loop_.rs test helper mints its registry via the sanctioned `ScopedToolRegistry::assemble` seam; `rpc/context.rs::minimal_with_cert_audit` gains `sop_driver_handles: None`; the engine test's nonexistent `terminal_run_count()` replaced with the `SopRunStore::load_terminal_runs` trait call. The 5 remaining `sop::engine` test failures are pre-existing drift (they never ran before — the lib-test target did not compile).

- **Defect #2 — the mqtt loop's per-poll liveness stamp (the flap root cause)** (2026-09-27, root-caused via systematic debugging): `orchestrator/mqtt.rs`'s event loop stamped the `mqtt` health component ONLY on ConnAck — a healthy persistent connection receives ConnAck exactly once at boot, so `mqtt.last_ok` froze at boot and the staleness liveness probe (the 2026-09-23 wire-death interim, 900s) killed every healthy gateway instance at ~18 min uptime (12 kills/4h prod, 14 canary; `restart_count: 0` throughout — the client never died; the probe commit's premise "last_ok updates on message consumption" was false — Publishes/PingResp never stamped). The fix: `record_poll_alive()` stamps the component on EVERY successful poll (the keepalive PingResp arrives every 30s even when idle) — `last_ok` becomes a true liveness-of-loop metric: a healthy-but-quiet wire never trips the probe; a genuinely hung loop (the original 2026-09-23 wire death) goes stale and the probe fires — the interim mitigation restored to its intended semantics. 2 unit tests (RED-then-GREEN: the stamp + the starting-state clears); the channels suite 1554/1554 + default 1540/1540. **Provenance: `9c65570d3`** (`cheknet-patched-v0.8.5`, 2026-09-27 — the channels crate: `orchestrator/mqtt.rs` + `orchestrator/mod.rs` + `filesystem.rs`; this prose entry rode in the same commit) — the sha this entry carried nowhere in the pre-audit prose; the anchor the mandatory re-pin probe checks.
- **Test-target debt repair (rides along)**: the channels lib-test target did not compile — 4 mechanical drift fixes (`AgentRouter::multi` calls gained the `sop_driver_sink: None` arg ×2; `ChannelRuntimeContext` initializer gained `sop_driver_sink: None`; `FilesystemChannelConfig` gained `driver_sink: None`).
- **`arg_deny_exemptions` (the python -c unblock)** (2026-09-28; spec: `debops-cheknet/docs/superpowers/specs/2026-09-28-python-c-arg-carveout-design.md`): the argument-deny guard's hardcoded `is_args_safe` arms are reified as a per-arm deny-entry table — `deny_entries_for` (token — predicate — case domain; the table is the single source of truth) — and `is_args_safe` becomes table-driven. `RiskProfileConfig`/`SecurityPolicy` gain `arg_deny_exemptions: HashMap<String, Vec<String>>` (`#[serde(default)]`: absent/empty is byte-for-byte today's behavior, and the currently deployed binary ignores the key, so config-before-image cannot crashloop). Semantics: subtract-only, clause-equality — a value string-equals (case-sensitively, as stored) exactly one deny entry of the keyed arm and removes that entry alone; keys bind to the invoked command's lowercased basename (`python`/`python3` distinct keys). Parse-time validation fails loud: unknown keys AND values matching no deny entry of the keyed arm are config errors (dead-config prevention, the dead-`tee` lesson), plus `*`/empty/whitespace values rejected; the key-set invariant test pins the table so arm drift fails the build. The shell-syntax layer (backticks in any quote state, `$VAR`/`$()`, unsafe redirects, `tee`, unquoted single `&`) is unreachable from the map. Escalation: child entries ⊆ parent (`ArgDenyExemptionExpandedByChild`, the allowlist direction — NOT `forbidden_paths`). Denial messages are computed from live policy state (`CommandRejection` + `suggestion(&policy)`) so a profile without the exemption never claims `-c` is allowed — the misdirect-retry class (the 88-retry `memory-purge` loop) this patch exists to kill. Friction-not-containment framing is binding in every comment/message that touches this: the container is the only hard boundary; the arg layer is friction + auditability.
  Touchpoints: `crates/zeroclaw-config/src/policy.rs` (the deny-entry table + `DenyEntry`/`DenyPredicate`, the `arg_deny_exemptions` field + `Default` impl, `from_profiles` threading, the `ensure_no_escalation_beyond` child⊆parent check, the `classify_args_safety` consult + the `classify_*` reason-threading); `crates/zeroclaw-config/src/schema.rs` (the `RiskProfileConfig.arg_deny_exemptions` field + `validate_arg_deny_exemptions`, wired into the config-parse error path); `crates/zeroclaw-runtime/src/tools/shell.rs` (the `rejection.suggestion(&self.security)` site — the message text itself lives in policy.rs).
  - **RE-PIN (the deny-entry table):** on upstream re-pin, re-derive the deny-entry table from the new `is_args_safe` arms and extend the matrix (the key-set invariant test catches arm-level drift; entry-level drift needs this procedure).
  - **RE-PIN (the sandbox posture):** on re-pin, RE-VERIFY the sandbox posture and pin `sandbox_backend` explicitly if auto-bwrap is unwanted — upstream's post-v0.8.5 sandbox rework moves bubblewrap into the Linux Auto chain, and on our Landlock-less kernel Auto would select it once the binary exists (the image ships it).
  - **schemars `schema-export`:** the exported-config-schema question was CHECKED at plan time (2026-09-28): no committed artifact exists to regenerate (the fork serves the schema dynamically from the compiled binary); re-check on re-pin.
  - **Ride-along repairs made in passing by this series:** the pre-existing `WebhookConfig.signature_header` lib-test compile repair (the webhook roundtrip test literal predates the field — unblocks the `cargo test -p zeroclaw-config --lib` target; same class and fix as `109b9a6ca` on cheknet-patched-v0.8.4) and the pre-existing fmt-drift style fix (F3's `EscalationViolation` variant initializer).

## The 2026-10-02 lineage audit

The discovery enumeration for the fork patch-lineage guarantee (task 0 of the
2026-10-02 patch-lineage work): the complete commit map of the current lineage —
the manifest's input truth. Every commit in
`git rev-list --no-merges v0.8.5..cheknet-patched-v0.8.5` is classified below.
The branch is exactly the `v0.8.5` tag (`cb2b20a9f` — the first commit's parent)
plus these 42 content commits; no content commit is left unclassified (the
mechanism's own surfaces — PATCHES.md / scripts/check_patches.py / Makefile —
ride the checker's reverse-check carve-out, never this map).

lineage: cheknet-patched-v0.8.5 base v0.8.5

reverse-scope: crates/zeroclaw-api, crates/zeroclaw-channels, crates/zeroclaw-config, crates/zeroclaw-gateway, crates/zeroclaw-log, crates/zeroclaw-macros, crates/zeroclaw-memory, crates/zeroclaw-providers, crates/zeroclaw-runtime, crates/zeroclaw-tools, Cargo.toml, Cargo.lock, PATCHES.md, docs/book/src/sop, src/main.rs, tests/component, zeroclaw_runtime

The reverse-scope is the union of every path the class-a/b commits touch
(`git show --stat` over each) — the ten crate trees plus the additional scoped
paths. `zeroclaw_runtime/` is a transient stray zc-engine dir (added in the
realignment window, removed by `7be31cc2e`; not in the HEAD tree but a
recorded on-branch touch).

**Method and the patch-id truth.** `git patch-id --stable` was computed for the
old table's nine shas (R1 through `adapt`) and for all 40 current commits:
**zero matches**. The v0.8.5 realignment was a single squash merge
(`f9b2c9fcf`, "realign cheknet patches onto v0.8.5") carrying every replayed
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
`b` — replayed v0.8.4-era patches (all carried by `f9b2c9fcf`); `c` — realignment
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
  ride-along `1796a6b85` (same class, same fix).
- Correction to the squash's own message: skip-at-dispatch (`4cd7afde4`) was
  noted "deferred (dispatch.rs kept HEAD)" but its content IS present at HEAD
  (`SopRunAction::Skipped` + the deterministic-skip tests, introduced by
  `f9b2c9fcf`) — carried, class-b row `skip-at-dispatch`.

| sha | class | row_id_or_allowlist | description |
|-----|-------|---------------------|-------------|
| f9b2c9fcf | b | R1 | old-table R1 — `feat(runtime): add mqtt_bus publisher helper` (`crates/zeroclaw-runtime/src/mqtt_bus.rs`) |
| f9b2c9fcf | b | R2-R4 | old-table R2–R4 — delegate publishes started/completed events + arg extensions |
| f9b2c9fcf | b | wiring | old-table wiring — mqtt_bus::init() into the daemon entry + channel-mqtt feature unification (call site `src/main.rs`) |
| f9b2c9fcf | b | delegate-results_dir | old-table delegate — results_dir config-overridable (upstreamable) |
| f9b2c9fcf | b | R5 | old-table R5 — the mqtt_publish tool for agent/SOP event publishing |
| f9b2c9fcf | b | R6 | old-table R6 — memory_store append + memory_recall prefix |
| f9b2c9fcf | b | gateway | old-table gateway — dynamic webhook route registration from config |
| f9b2c9fcf | b | delegate-ttl_seconds | old-table delegate — ttl_seconds parameter + loop-detector false-positive fix |
| f9b2c9fcf | b | t6b-task-events | M2 T6b — typed Task* events on fixed topics (`zeroclaw/tasks/{started,completed,failed}`), non-retained, IDs in payload |
| f9b2c9fcf | b | sop-headless-drivers | the SOP headless-driver system (driver ownership/supervision, step scoping, headless runs; `sop/active_scope.rs` + the sop tree) |
| f9b2c9fcf | b | otel-w3c-bridge | the tracing-opentelemetry bridge + W3C TraceContext/Baggage composite propagator (`zeroclaw-log/src/otel_bridge.rs`) |
| f9b2c9fcf | b | migrate-fixes | migration fixes — permissive operator policy on the postgres path + V3 agent metadata preservation |
| f9b2c9fcf | b | runs-fixes | v0.8.4-era `1b74f1f35` — file_read directory listing, run pruning, stuck-run reaper, cron-load Option |
| f9b2c9fcf | b | finished-runs-cap | v0.8.4-era `b6bfb8c78` — finished_runs capped to max_finished_runs on restore + maintenance tick |
| f9b2c9fcf | b | skip-at-dispatch | v0.8.4-era `4cd7afde4` — sidecar SOP skip-at-dispatch (present at HEAD despite the squash's "deferred" note) |
| d3e195993 | a | M3AX-1 | memory Phase 0.5 — three-axis recall + namespace/importance store + exclude config + recall SELECT fix |
| 2fd2a3e49 | a | M3AX-2 | memory Phase 0.5 — tool params (namespace/importance on store; category/namespace/key_prefix/scope on recall) + btree index on key |
| 3548143b1 | a | M3AX-3 | memory Phase 0.5 — three-axis in-memory filtering in memory_recall execute() |
| c91662850 | c | tooling | Docker build context excludes (`.worktrees/` + `target/`; 225GB → ~1GB) |
| e0c654935 | c | realignment-fixup | restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock) |
| fd1ef7def | c | realignment-fixup | missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit) |
| 60b7f1a91 | c | realignment-fixup | exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray `zeroclaw_runtime/` add) |
| 698fd1131 | c | realignment-fixup | namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape) |
| 9eab4b59a | c | realignment-fixup | reaped_stuck_runs placement — method condition + initializer (not field decl) |
| 968691630 | c | realignment-fixup | `mut` on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration) |
| a78b9816e | c | realignment-fixup | gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes) |
| 6465b2c4c | c | realignment-fixup | webhook_secret_hash added to the AppState initializer (E0063 missing field) |
| ccd4b595e | c | realignment-fixup | HashMap::new() for generic_webhook fields (setup code lost in realignment) |
| 96334076b | c | realignment-fixup | #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement) |
| b4535defa | c | realignment-fixup | webhook_secret_hash: None unconditional in the initializer (field not #[cfg]-gated) |
| 7be31cc2e | a | GLM53-EFFORT | provider — thinking.budget_tokens to OpenAI-compatible chat completions + reasoning_effort=max; rides: the stray `zeroclaw_runtime/` dir removed |
| 43d274e2a | a | M3AX-WIRE | complete three-axis recall exclusion wiring |
| 8e5690fbb | a | DELEGATE-EXCL | config-sourced recall excludes threaded into bounded delegate memory tools |
| dea4038bf | a | PG-BOUNDS | bound PostgreSQL memory ops — statement_timeout, keepalives, op timeout |
| 355575edf | a | PG-SHARE | share one backend instance per config across agent constructions |
| 27eb2d694 | a | PG-BLOCKPOOL | construct backends on the blocking pool, not async workers |
| f2fda50ad | c | realignment-fixup | Cargo.lock sync — entries missing from the restored v0.8.5 lockfile |
| c311d2b76 | a | PG-CACHE-GATE | gate resolutions — backend_cache default to postgres; bound construction await |
| 7fed6e9a8 | a | OTEL-ONCE | construct the OTel pipeline once per process |
| 90c554280 | a | PG-FLOAT8 | type the importance param as FLOAT8 in the postgres store |
| a1fed21af | c | realignment-fixup | Cargo.lock entries completed for the realigned cheknet patch deps |
| 7a3640776 | a | L2B-NOTIFY | Lever-2b — the engine-side completion-notification directive (quorum PROCEED 3/3, delegate-bg-notify-injection) |
| edf41c818 | c | docs | PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair) |
| 9c65570d3 | a | MQTT-POLL-ALIVE | the per-poll liveness stamp — defect #2, the mqtt flap root cause (the channels crate) |
| 1796a6b85 | a | RIDE-WEBHOOK-TEST | ride-along — WebhookConfig.signature_header lib-test initializer gap (same class/fix as v0.8.4's `109b9a6ca`) |
| 998fb082c | a | ADE-1 | arg_deny_exemptions — deny-entry table + arg_deny_exemptions field (RED groundwork) |
| 1701c56aa | a | ADE-2 | arg_deny_exemptions — schema + fail-loud parse-time validation |
| db0f20af7 | a | ADE-3 | arg_deny_exemptions — config threading + escalation-subset variant |
| 1d21f221f | a | RIDE-FMT | ride-along — pre-existing fmt drift (F3's `EscalationViolation` variant initializer) |
| 694db6a2f | a | ADE-4 | arg_deny_exemptions — is_args_safe table-driven + exemption consult (python -c unblocked) |
| 264e2c293 | a | ADE-5 | arg_deny_exemptions — policy-state-computed denial messages (reason-threading) |
| 118afa241 | a | ADE-6 | arg_deny_exemptions — classifier mirrors ReadOnly autonomy (no misdirecting suggestion on read-only profiles) |
| 2f32e99bb | a | ADE-7 | arg_deny_exemptions — the complete §1.9 deny matrix (both dialect entry points, map absent + present) |
| a05960c1f | c | docs | PATCHES.md — the arg_deny_exemptions entry |
| 33f948e15 | a | U1 | the allow_scripts subagent propagation fix — cherry-picked from cheknet-pached `352672c02` (the dead upstreamed-by-v0.8.4 claim, resolved by the C.1 remediation) |
| 957bee7d6 | a | U2 | the cron chat-leak fix — cherry-picked from cheknet-pached `69dd83ed4` (the dead upstreamed-by-v0.8.4 claim, resolved by the C.1 remediation) |
| 26a255fe8 | a | SOP-POISON-QUARANTINE | the SOP ledger poison-row quarantine — tolerant restore scans + terminal tombstones + the reaped_stuck_runs reporting fix (the 2026-10-03..05 restore-wedge / slots-full root cause) |

Row count: 57 rows over 43 commits — 15 class-b rows (one commit, the squash
carrier), 26 class-a rows (one per commit), 16 class-c rows (one per commit).
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
  c91662850 — tooling: Docker build context excludes (.worktrees/ + target/; 225GB → ~1GB)
  e0c654935 — realignment-fixup: restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  fd1ef7def — realignment-fixup: missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  60b7f1a91 — realignment-fixup: exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray zeroclaw_runtime/ add)
  698fd1131 — realignment-fixup: namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — realignment-fixup: reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — realignment-fixup: mut on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — realignment-fixup: gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  6465b2c4c — realignment-fixup: webhook_secret_hash added to the AppState initializer (E0063 missing field)
  ccd4b595e — realignment-fixup: HashMap::new() for generic_webhook fields (setup code lost in realignment)
  96334076b — realignment-fixup: #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  b4535defa — realignment-fixup: webhook_secret_hash None unconditional in the initializer (field not #[cfg]-gated)
  f2fda50ad — realignment-fixup: Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — realignment-fixup: Cargo.lock entries completed for the realigned cheknet patch deps
  edf41c818 — docs: PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  a05960c1f — docs: PATCHES.md — the arg_deny_exemptions entry
-- audit-map class-c (16) --
  c91662850 — Docker build context excludes (`.worktrees/` + `target/`; 225GB → ~1GB)
  e0c654935 — restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  fd1ef7def — missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  60b7f1a91 — exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray `zeroclaw_runtime/` add)
  698fd1131 — namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — `mut` on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  6465b2c4c — webhook_secret_hash added to the AppState initializer (E0063 missing field)
  ccd4b595e — HashMap::new() for generic_webhook fields (setup code lost in realignment)
  96334076b — #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  b4535defa — webhook_secret_hash: None unconditional in the initializer (field not #[cfg]-gated)
  f2fda50ad — Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — Cargo.lock entries completed for the realigned cheknet patch deps
  edf41c818 — PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  a05960c1f — PATCHES.md — the arg_deny_exemptions entry
result: RED — 2 failure section(s); every patch must map to a row or the allowlist, and the allowlist + class-c enumeration above must be human-verified
```

### The C.1 remediation — the checker's first green run (2026-10-02)

The honest-red state above is repaired, exactly as its closing note
prescribed. Both dead upstreamed claims resolved by replay: U1
cherry-picked from `cheknet-pached` `352672c02` → `33f948e15` (both
`load_skills_from_directory` scan sites now read the root config
snapshot's `skills.allow_scripts` — the probe 2-of-2), U2 from
`69dd83ed4` → `957bee7d6` (`event_matches_session`'s no-session arm
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
  c91662850 — tooling: Docker build context excludes (.worktrees/ + target/; 225GB → ~1GB)
  e0c654935 — realignment-fixup: restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  fd1ef7def — realignment-fixup: missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  60b7f1a91 — realignment-fixup: exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray zeroclaw_runtime/ add)
  698fd1131 — realignment-fixup: namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — realignment-fixup: reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — realignment-fixup: mut on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — realignment-fixup: gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  6465b2c4c — realignment-fixup: webhook_secret_hash added to the AppState initializer (E0063 missing field)
  ccd4b595e — realignment-fixup: HashMap::new() for generic_webhook fields (setup code lost in realignment)
  96334076b — realignment-fixup: #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  b4535defa — realignment-fixup: webhook_secret_hash None unconditional in the initializer (field not #[cfg]-gated)
  f2fda50ad — realignment-fixup: Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — realignment-fixup: Cargo.lock entries completed for the realigned cheknet patch deps
  edf41c818 — docs: PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  a05960c1f — docs: PATCHES.md — the arg_deny_exemptions entry
-- audit-map class-c (16) --
  c91662850 — Docker build context excludes (`.worktrees/` + `target/`; 225GB → ~1GB)
  e0c654935 — restore the v0.8.5 Cargo.lock (the squash brought v0.8.4's stale lock)
  fd1ef7def — missing `]` on the exclude_namespaces serde attribute (dropped in the realignment edit)
  60b7f1a91 — exclude_namespaces/categories/key_prefixes in the MemoryConfig Default impl (rode in the stray `zeroclaw_runtime/` add)
  698fd1131 — namespace+importance moved to owned before run_on_os_thread (E0521 borrow escape)
  9eab4b59a — reaped_stuck_runs placement — method condition + initializer (not field decl)
  968691630 — `mut` on sub_tools (retain needs &mut against v0.8.5's ScopedToolRegistry declaration)
  a78b9816e — gateway lib.rs conflict markers resolved (v0.8.5 sop_webhook_routes + our dynamic webhook routes)
  6465b2c4c — webhook_secret_hash added to the AppState initializer (E0063 missing field)
  ccd4b595e — HashMap::new() for generic_webhook fields (setup code lost in realignment)
  96334076b — #[cfg(feature=channel-webhook)] restored on the generic_webhook_secrets initializer (lost in sed replacement)
  b4535defa — webhook_secret_hash: None unconditional in the initializer (field not #[cfg]-gated)
  f2fda50ad — Cargo.lock sync — entries missing from the restored v0.8.5 lockfile
  a1fed21af — Cargo.lock entries completed for the realigned cheknet patch deps
  edf41c818 — PATCHES.md — the v0.8.5 patch entry (Lever-2b + the test-target debt repair)
  a05960c1f — PATCHES.md — the arg_deny_exemptions entry
result: GREEN — every patch in the lineage is accounted for; the allowlist + class-c enumeration above is the trust boundary: no drop can pass without appearing on this report
```
