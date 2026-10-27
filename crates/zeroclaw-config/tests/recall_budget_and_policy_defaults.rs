//! Tuning cycle 3 (2026-10-27): the recall wall-clock budget knob and the
//! tune-map content-scan posture as FORK DEFAULTS.
//!
//! - `[memory] recall_soft_timeout_ms` — per-call wall-clock budget for
//!   recall; 0 disables. Default 2000ms (the measured prod pathology was
//!   6.3–6.7s per `memory_recall` call; the measurement contract promises
//!   <2s).
//! - `[memory.policy] threat_scan_on_hit` default `block-on-read` (was
//!   upstream `reject`) and `redact_on_write` default `true` (was `false`) —
//!   the proven prod v14 posture from the content-scan tune map: flagged
//!   writes persist (audit archives stay intact) and are withheld at read
//!   time, while redaction cleans future entries. `threat_scan_load_time`
//!   stays `true`: the map explicitly does NOT recommend disabling read-time
//!   scanning.

use zeroclaw_config::schema::{MemoryConfig, MemoryPolicyConfig};

#[test]
fn recall_soft_timeout_defaults_to_2000ms() {
    let cfg = MemoryConfig::default();
    assert_eq!(
        cfg.recall_soft_timeout_ms, 2000,
        "the default budget must promise the <2s recall measurement contract"
    );
}

#[test]
fn recall_soft_timeout_resolves_through_the_config_deserializer() {
    let cfg: MemoryConfig = serde_json::from_str(
        r#"{"recall_soft_timeout_ms": 0}"#,
    )
    .expect("recall_soft_timeout_ms must deserialize");
    assert_eq!(
        cfg.recall_soft_timeout_ms, 0,
        "0 must be representable: it disables the budget"
    );

    let cfg: MemoryConfig =
        serde_json::from_str(r#"{"recall_soft_timeout_ms": 5000}"#).expect("must deserialize");
    assert_eq!(cfg.recall_soft_timeout_ms, 5000);
}

#[test]
fn recall_soft_timeout_defaults_when_absent_from_serialized_config() {
    let cfg: MemoryConfig = serde_json::from_str("{}").expect("empty config must deserialize");
    assert_eq!(cfg.recall_soft_timeout_ms, 2000);
}

#[test]
fn threat_policy_defaults_match_the_live_block_on_read_posture() {
    let policy = MemoryPolicyConfig::default();
    assert_eq!(
        policy.threat_scan_on_hit, "block-on-read",
        "flagged writes must persist by default (the audit-loss class is cured at the fork level)"
    );
    assert!(
        policy.redact_on_write,
        "future entries must persist masked so the corpus cures at the source"
    );
    assert!(
        policy.threat_scan_load_time,
        "read-time scanning stays ON — the tune map explicitly does NOT recommend disabling it"
    );
}
