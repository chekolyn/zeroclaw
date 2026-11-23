//! The retention ship posture (the native-hygiene patch, cycle 25).
//!
//! Every row-retention knob ships at 0 = keep forever. The hygiene prune is
//! backend-native now, so a non-zero DEFAULT would arm a DELETE on every
//! deployment that does not name the knob explicitly — the opposite of the
//! ratified-window posture (the operator turns each window on deliberately,
//! after deciding what it may delete). The `sop` audit-trail category is
//! never expressible in the arm plan at all (pinned in zeroclaw-memory).

use zeroclaw_config::schema::MemoryConfig;

#[test]
fn retention_defaults_ship_all_windows_closed() {
    let cfg = MemoryConfig::default();
    assert_eq!(
        cfg.conversation_retention_days, 0,
        "conversation retention must ship at 0 = keep forever"
    );
    assert_eq!(
        cfg.daily_retention_days, 0,
        "daily retention must ship at 0 = keep forever"
    );
    assert_eq!(
        cfg.core_retention_days, 0,
        "core retention must ship at 0 = keep forever"
    );
}

#[test]
fn retention_defaults_survive_a_round_trip_of_an_empty_memory_section() {
    let cfg: MemoryConfig =
        serde_json::from_str("{}").expect("an absent retention set must deserialize");
    assert_eq!(cfg.conversation_retention_days, 0);
    assert_eq!(cfg.daily_retention_days, 0);
    assert_eq!(cfg.core_retention_days, 0);
}
