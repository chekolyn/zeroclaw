# Patches Log for `cheknet-patched-v0.8.2` Branch

This branch contains local custom patches applied to the ZeroClaw codebase for deployment in the home lab environment.

## Base Version
- **Tag:** `v0.8.2`

## Applied Patches

### 1. Cron Chat Leak Fix
- **Commit:** `07df4e09`
- **Description:** Fixes a security leak where automated cron job execution outputs were broadcast to all active chat WebSockets instead of being routed exclusively to the `"cron"` session.

### 2. Allow Scripts Subagent Fix
- **Commit:** `462a2027`
- **Description:** Fixes an issue where delegated subagents spawned by the `delegate` tool would skip skills containing script files and print warnings (`skills.allow_scripts = true` was not properly propagated from the parent agent's configuration).
