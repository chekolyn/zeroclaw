I have all three files. Let me verify the key claims against the diff:

1. **Module feature-gated**: `lib.rs` line — `#[cfg(feature = "channel-mqtt")] pub mod mqtt_bus;` ✅ correct.
2. **publish is no-op when unconfigured**: `mqtt_bus.rs:113-121` — `guard.as_ref()` is `None` → logs + returns `Ok(())`. ✅ Test `publish_noop_when_mqtt_unconfigured` covers it (line 234). The test asserts `result.is_ok()` and doesn't panic. ✅
3. **topic_for sanitization**: `mqtt_bus.rs:147-153` — `sanitize_segment` lowercases + replaces illegal chars. `is_illegal_topic_char` checks `/`, `+`, `#`, space, tab, CR, LF, ASCII control. Test `topic_for_sanitizes_segments` verifies `Foo Bar/Baz` → `foo_bar_baz`, `+`/`#` replaced, control char replaced, empty → `zeroclaw/`. ✅
4. **record! macro used correctly**: Three format-arg calls were fixed to `format!(...)` — line 83 (`format!("mqtt_bus: eventloop poll error: {e}")`), line 99 (`format!("mqtt_bus: publisher initialized for broker {}", cfg.broker_url)`), line 116 (`format!("mqtt_bus: publish no-op (mqtt not configured) topic={topic}")`). The two non-format string literals (lines 47, "mqtt_bus: init no-op...") pass a bare `&str` which is a valid `$msg:expr`. ✅
5. **No daemon/delegate modifications**: Diff only touches `lib.rs` (5 lines) + `mqtt_bus.rs` (new file). No `daemon/mod.rs` or `delegate.rs`. ✅
6. **No hardcoded secrets**: Password flows from `cfg.password` (config), no literal secrets. ✅
7. **init spawns eventloop poll task**: `mqtt_bus.rs:75-93` — `tokio::spawn(async move { loop { match eventloop.poll().await ... }})`. ✅
8. **Interface signatures**: `pub async fn init(config: &Config) -> Result<()>` (line 30), `pub async fn publish(topic: &str, payload: Vec<u8>, retain: bool) -> Result<()>` (line 110), `pub fn topic_for(parts: &[&str]) -> String` (line 145). ✅ All match spec.
9. **LazyLock + tokio::sync::Mutex**: `static PUBLISHER: LazyLock<Mutex<Option<AsyncClient>>>` (line 17). ✅ Matches existing style.
10. **Unique client_id**: `format!("{}-pub", cfg.client_id)` (line 60). ✅
11. **Self-contained broker_host/port**: `broker_host`/`broker_port` parse `mqtt://`/`mqtts://` locally (lines 192-218). No cross-crate dependency. ✅

Now the concerns:
- **TDD RED step**: The report admits tests + impl were written together; RED was the pre-`pub mod` compile-error state. The two tests genuinely assert behavior (not mocks): `topic_for_sanitizes_segments` checks 5 cases including the spec's exact example; `publish_noop_when_mqtt_unconfigured` verifies the no-op contract. For a new module with pure functions + a no-op-by-default async path, this is a reasonable test surface. The RED evidence is weaker than ideal (a removed `pub mod` declaration isn't the same as a failing assertion on the sanitization logic), but the GREEN tests are real behavior verification. This is a Minor TDD-hygiene note, not a blocker — the tests assert real behavior and pass.
- **init() eventloop task untested**: The configured path (`init` → `publish` → real broker) is untested. The report acknowledges this; it's deferred to Task 5 integration + Task 18 e2e. Acceptable for R1 (unit scope); a test with a mock broker would be ideal but is non-trivial to set up for rumqttc and the plan scopes this to integration.

### Spec Compliance
- ✅ Spec compliant — all interface signatures, feature gating, no-op contract, sanitization rules, record! macro usage, and file scope match the brief. No extra features; no daemon/delegate modifications; no hardcoded secrets.
- ⚠️ Cannot verify from diff: the `cargo test` / `cargo build` output (the report carries that evidence; the diff alone can't confirm the binary compiled+passed). The report's test output is internally consistent (2 tests named in the diff match the 2 tests reported passing).

### Strengths
- Clean separation: `init`, `publish`, `topic_for` are each single-responsibility; `broker_host`/`broker_port`/`sanitize_segment`/`is_illegal_topic_char` are well-factored helpers, each testable independently.
- Graceful degradation is exactly right: `publish()` locks the mutex, checks `None`, logs, returns `Ok` — never panics, never `Err` for "not configured" (mqtt_bus.rs:113-121).
- `publish()` clones the `AsyncClient` out of the mutex and `drop(guard)` before the `.await` (mqtt_bus.rs:124-127) — correctly avoids holding the lock across an await, preventing publish serialization.
- Eventloop poll task (mqtt_bus.rs:75-93) handles errors with a 1s backoff rather than panicking; rumqttc auto-reconnects. Correctly scoped to the configured path only.
- Self-contained URL parsing avoids a runtime→channels cross-crate dependency (brief explicitly preferred this).
- Tests cover the spec's exact sanitization example (`Foo Bar/Baz` → `foo_bar_baz`), wildcards, control chars, empty parts, and the no-op contract.

### Issues
#### Critical (Must Fix)
*(none)*

#### Important (Should Fix)
*(none)*

#### Minor (Nice to Have)
- TDD RED evidence is weak: tests + implementation were written together, so the "failing test first" step was the compile-error state before `pub mod mqtt_bus;` was added, not a failing assertion on the sanitization/no-op logic. Acceptable for a new module with pure functions, but not textbook RED→GREEN. (task-4-report.md, "Concerns" section — acknowledged.)
- `init()`'s configured path (eventloop poll + real publish) is untested at the unit level. A mock-broker test would strengthen R1 but is non-trivial for rumqttc; deferred to Task 5 integration + Task 18 e2e. (mqtt_bus.rs:75-93, 137-141)

### Assessment
**Task quality:** Approved

**Reasoning:** The module is well-built, spec-compliant, and the two unit tests genuinely verify behavior (sanitization rules + no-op contract). The TDD-hygiene gap (tests written alongside impl, not strictly RED-first) and the untested configured-path are both acknowledged in the report and appropriately deferred to integration — neither is a correctness or maintainability blocker for this unit-scoped task.