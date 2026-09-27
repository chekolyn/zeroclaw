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
