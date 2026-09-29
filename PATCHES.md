# Patches Log for the `cheknet-patched-v*` Branches

The patched branches carry local custom patches on top of the pinned upstream
ZeroClaw tags for the home-lab deployment. v0.8.4 base: `a56c345d`; the current
branch `cheknet-patched-v0.8.5` pins upstream v0.8.5.

## Upstreamed fixes (already in v0.8.4, NOT replayed here)

These were cheknet-origin fixes that landed upstream; they are present in
v0.8.4 and were intentionally dropped from the replay set to avoid conflicts:

- Cron chat-leak fix (upstream `69dd83ed`)
- `allow_scripts` subagent propagation fix (upstream `352672c0`)

## Applied patches (cheknet-specific, replayed onto v0.8.4)

| R | Commit | Description |
|---|--------|-------------|
| R1 | `7090d8c28` | `feat(runtime): add mqtt_bus publisher helper` |
| R2–R4 | `a0f48c2f8` | `feat(runtime): delegate publishes started/completed events + arg extensions` |
| wiring | `1a564c72d` | `fix(event-driven): wire mqtt_bus::init() into daemon + forward channel-mqtt to runtime` |
| delegate | `4bc701acf` | `fix(delegate): make results_dir config-overridable (upstreamable)` |
| R5 | `b04266372` | `feat(runtime): mqtt_publish tool for agent/SOP event publishing` |
| R6 | `57568bc62` | `feat(tools): memory_store append + memory_recall prefix` |
| gateway | `b4eb7ed10` | `feat(gateway): dynamic webhook route registration from config` |
| delegate | `efd448d8f` | `fix(delegate): add ttl_seconds parameter + fix loop detector false positives` |
| adapt | `454f20239` | `fix: adapt cheknet patches to v0.8.4 ToolOutput API` (realignment fixup) |

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

- **Defect #2 — the mqtt loop's per-poll liveness stamp (the flap root cause)** (2026-09-27, root-caused via systematic debugging): `orchestrator/mqtt.rs`'s event loop stamped the `mqtt` health component ONLY on ConnAck — a healthy persistent connection receives ConnAck exactly once at boot, so `mqtt.last_ok` froze at boot and the staleness liveness probe (the 2026-09-23 wire-death interim, 900s) killed every healthy gateway instance at ~18 min uptime (12 kills/4h prod, 14 canary; `restart_count: 0` throughout — the client never died; the probe commit's premise "last_ok updates on message consumption" was false — Publishes/PingResp never stamped). The fix: `record_poll_alive()` stamps the component on EVERY successful poll (the keepalive PingResp arrives every 30s even when idle) — `last_ok` becomes a true liveness-of-loop metric: a healthy-but-quiet wire never trips the probe; a genuinely hung loop (the original 2026-09-23 wire death) goes stale and the probe fires — the interim mitigation restored to its intended semantics. 2 unit tests (RED-then-GREEN: the stamp + the starting-state clears); the channels suite 1554/1554 + default 1540/1540.
- **Test-target debt repair (rides along)**: the channels lib-test target did not compile — 4 mechanical drift fixes (`AgentRouter::multi` calls gained the `sop_driver_sink: None` arg ×2; `ChannelRuntimeContext` initializer gained `sop_driver_sink: None`; `FilesystemChannelConfig` gained `driver_sink: None`).
- **`arg_deny_exemptions` (the python -c unblock)** (2026-09-28; spec: `debops-cheknet/docs/superpowers/specs/2026-09-28-python-c-arg-carveout-design.md`): the argument-deny guard's hardcoded `is_args_safe` arms are reified as a per-arm deny-entry table — `deny_entries_for` (token — predicate — case domain; the table is the single source of truth) — and `is_args_safe` becomes table-driven. `RiskProfileConfig`/`SecurityPolicy` gain `arg_deny_exemptions: HashMap<String, Vec<String>>` (`#[serde(default)]`: absent/empty is byte-for-byte today's behavior, and the currently deployed binary ignores the key, so config-before-image cannot crashloop). Semantics: subtract-only, clause-equality — a value string-equals (case-sensitively, as stored) exactly one deny entry of the keyed arm and removes that entry alone; keys bind to the invoked command's lowercased basename (`python`/`python3` distinct keys). Parse-time validation fails loud: unknown keys AND values matching no deny entry of the keyed arm are config errors (dead-config prevention, the dead-`tee` lesson), plus `*`/empty/whitespace values rejected; the key-set invariant test pins the table so arm drift fails the build. The shell-syntax layer (backticks in any quote state, `$VAR`/`$()`, unsafe redirects, `tee`, unquoted single `&`) is unreachable from the map. Escalation: child entries ⊆ parent (`ArgDenyExemptionExpandedByChild`, the allowlist direction — NOT `forbidden_paths`). Denial messages are computed from live policy state (`CommandRejection` + `suggestion(&policy)`) so a profile without the exemption never claims `-c` is allowed — the misdirect-retry class (the 88-retry `memory-purge` loop) this patch exists to kill. Friction-not-containment framing is binding in every comment/message that touches this: the container is the only hard boundary; the arg layer is friction + auditability.
  Touchpoints: `crates/zeroclaw-config/src/policy.rs` (the deny-entry table + `DenyEntry`/`DenyPredicate`, the `arg_deny_exemptions` field + `Default` impl, `from_profiles` threading, the `ensure_no_escalation_beyond` child⊆parent check, the `classify_args_safety` consult + the `classify_*` reason-threading); `crates/zeroclaw-config/src/schema.rs` (the `RiskProfileConfig.arg_deny_exemptions` field + `validate_arg_deny_exemptions`, wired into the config-parse error path); `crates/zeroclaw-runtime/src/tools/shell.rs` (the `rejection.suggestion(&self.security)` site — the message text itself lives in policy.rs).
  - **RE-PIN (the deny-entry table):** on upstream re-pin, re-derive the deny-entry table from the new `is_args_safe` arms and extend the matrix (the key-set invariant test catches arm-level drift; entry-level drift needs this procedure).
  - **RE-PIN (the sandbox posture):** on re-pin, RE-VERIFY the sandbox posture and pin `sandbox_backend` explicitly if auto-bwrap is unwanted — upstream's post-v0.8.5 sandbox rework moves bubblewrap into the Linux Auto chain, and on our Landlock-less kernel Auto would select it once the binary exists (the image ships it).
  - **schemars `schema-export`:** the exported-config-schema question was CHECKED at plan time (2026-09-28): no committed artifact exists to regenerate (the fork serves the schema dynamically from the compiled binary); re-check on re-pin.
  - **Ride-along repairs made in passing by this series:** the pre-existing `WebhookConfig.signature_header` lib-test compile repair (the webhook roundtrip test literal predates the field — unblocks the `cargo test -p zeroclaw-config --lib` target; same class and fix as `109b9a6ca` on cheknet-patched-v0.8.4) and the pre-existing fmt-drift style fix (F3's `EscalationViolation` variant initializer).
