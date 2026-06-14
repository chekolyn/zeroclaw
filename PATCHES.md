# Patches Log for `cheknet-pached` Branch

This branch contains local custom patches applied to the ZeroClaw codebase for deployment in the home lab environment.

## Base Version
- **Tag:** `v0.8.0`

## Applied Patches

### 1. Cron Chat Leak Fix
- **Commit:** `69dd83ed` (originally `792a5f80`)
- **Description:** Fixes a security leak where automated cron job execution outputs were broadcast to all active chat WebSockets instead of being routed exclusively to the `"cron"` session.

### 2. Allow Scripts Subagent Fix
- **Commit:** `40143f5b`
- **Description:** Fixes an issue where delegated subagents spawned by the `delegate` tool would skip skills containing script files and print warnings (`skills.allow_scripts = true` was not properly propagated from the parent agent's configuration).
