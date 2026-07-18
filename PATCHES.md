# Patches Log for `cheknet-patched-v0.8.3` Branch

This branch contains local custom patches applied to the ZeroClaw codebase for deployment in the home lab environment.

## Base Version
- **Tag:** `v0.8.3`

## Applied Patches

### 1. Cron Chat Leak Fix
- **Commit:** `07df4e09` (upstream: `69dd83ed`)
- **Description:** Fixes a security leak where automated cron job execution outputs were broadcast to all active chat WebSockets instead of being routed exclusively to the `"cron"` session.

### 2. Allow Scripts Subagent Fix
- **Commit:** `462a2027` (upstream: `352672c0`)
- **Description:** Fixes an issue where delegated subagents spawned by the `delegate` tool would skip skills containing script files and print warnings (`skills.allow_scripts = true` was not properly propagated from the parent agent's configuration).

### 3. Event-Driven Architecture (R1-R6)
- **R1:** `mqtt_bus` publisher helper (commit `5a922bfd`)
- **R2-R4:** Delegate publishes started/completed events + arg extensions (commit `ae7b95df`)
- **Event wiring:** Wire `mqtt_bus::init()` into daemon + forward channel-mqtt to runtime (commit `74e3cb50`)
- **Delegate config:** Make `results_dir` config-overridable (commit `0f35a521`)
- **R5:** `mqtt_publish` tool for agent/SOP event publishing (commit `167bda1a`)
- **R6:** `memory_store` append + `memory_recall` prefix tools (commit `ac772176`)

### 4. Dynamic Webhook Route Registration
- **Commit:** `101d8e1c`
- **Description:** Gateway registers webhook routes dynamically from config, enabling flexible webhook endpoint management without code changes.
