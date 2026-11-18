//! The `[storage.postgres] pool_size` knob (the MEMORY-PG-POOL program).
//!
//! Absent by default: the backend default (4 connections) fires with NO
//! debops config change — the `recall_soft_timeout_ms` precedent. The knob
//! is `Option<u64>` end to end (config schema field, backend parameter,
//! resolver).

use zeroclaw_config::schema::PostgresStorageConfig;

#[test]
fn pool_size_is_absent_by_default() {
    assert_eq!(
        PostgresStorageConfig::default().pool_size,
        None,
        "the Default impl must leave pool_size absent"
    );
    let cfg: PostgresStorageConfig =
        serde_json::from_str("{}").expect("empty config must deserialize");
    assert_eq!(
        cfg.pool_size, None,
        "an absent pool_size key must deserialize to None"
    );
}

#[test]
fn pool_size_resolves_through_the_deserializer() {
    let cfg: PostgresStorageConfig =
        serde_json::from_str(r#"{"pool_size": 2}"#).expect("pool_size must deserialize");
    assert_eq!(cfg.pool_size, Some(2), "2 must resolve to Some(2)");

    let serialized = serde_json::to_string(&cfg).expect("the config must serialize");
    let round: PostgresStorageConfig =
        serde_json::from_str(&serialized).expect("the serialized config must deserialize");
    assert_eq!(
        round.pool_size,
        Some(2),
        "the serialize round-trip must keep pool_size"
    );
}
