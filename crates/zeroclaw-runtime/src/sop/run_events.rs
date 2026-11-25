//! The SOP run-failed MQTT event (the north Stage-2 leg) — the fixed topic
//! constant + the payload builder for the event-driven learning loop.
//!
//! Pure functions mirroring the sidecar
//! `zc_engine.core.schemas.events.sop_run_events.SopRunFailed` pydantic model
//! (the T6a discipline — the `task_events` delegate precedent). Extracted so
//! they are unit-testable without a live MQTT broker (`mqtt_bus::publish` is
//! a free function, not mockable).
//!
//! Contract: non-retained, fixed topic, IDs in the payload. The publish site
//! lives in the engine's terminal-failure arm (`SopEngine::record_run_failed_event`)
//! and is best-effort fire-and-forget — a publish failure NEVER fails the
//! run's terminal path (the C7 boot-time reconcile is the boot-safe backstop).

use crate::sop::types::{SopRun, SopRunStatus};

/// Fixed MQTT topic for SOP run-failed events.
///
/// Published by the gateway runtime when an agent-run SOP terminates in
/// failure — the event-driven feed into the learning loop (the north
/// Stage-2 leg). Non-retained by design: a live-failure signal whose
/// boot-safe backstop is the C7 reconcile.
pub fn sop_run_failed_topic() -> &'static str {
    "zeroclaw/sop/runs/failed"
}

/// Build the `SopRunFailed` payload (sidecar schema: `SopRunFailed`).
///
/// Fields: `run_id`, `sop_name`, `failure_reason`, `started_at`,
/// `completed_at`, `current_step`, `total_steps`, `source` (always
/// `"gateway"` — the publishing plane). The reason degrades to `"unknown"`
/// when absent so the sidecar's registered-topic validation never drops the
/// event on a min-length violation (a dropped event is a silently dropped
/// occurrence).
pub fn build_sop_run_failed_payload(run: &SopRun) -> serde_json::Value {
    serde_json::json!({
        "run_id": run.run_id.clone(),
        "sop_name": run.sop_name.clone(),
        "failure_reason": run.failure_reason.as_deref().unwrap_or("unknown"),
        "started_at": run.started_at.clone(),
        "completed_at": run.completed_at.as_deref().unwrap_or("unknown"),
        "current_step": run.current_step,
        "total_steps": run.total_steps,
        "source": "gateway",
    })
}

/// The emit decision for a terminal run: `Some((topic, payload))` for a
/// `Failed` run, `None` for every other terminal status — completions and
/// cancellations publish nothing (the failed topic stays failed-only).
pub fn run_failed_event_for(run: &SopRun) -> Option<(String, String)> {
    if run.status != SopRunStatus::Failed {
        return None;
    }
    let payload = build_sop_run_failed_payload(run).to_string();
    Some((sop_run_failed_topic().to_string(), payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sop::types::{SopEvent, SopRun, SopRunStatus, SopTriggerSource};

    fn run_with(status: SopRunStatus, failure_reason: Option<&str>) -> SopRun {
        SopRun {
            run_id: "run-1".to_string(),
            sop_name: "harness-deploy-executor".to_string(),
            initiating_agent: None,
            memory_owner: None,
            trigger_event: SopEvent {
                source: SopTriggerSource::Manual,
                topic: None,
                payload: None,
                timestamp: "2026-11-25T00:00:00Z".to_string(),
            },
            frame_marker_id: "m".to_string(),
            status,
            current_step: 2,
            total_steps: 4,
            started_at: "2026-11-25T00:00:00Z".to_string(),
            completed_at: Some("2026-11-25T00:01:00Z".to_string()),
            failure_reason: failure_reason.map(str::to_string),
            step_results: Vec::new(),
            waiting_since: None,
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
            decided_mode: None,
            decisions: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn topic_constant_is_the_fixed_runs_failed_topic() {
        assert_eq!(sop_run_failed_topic(), "zeroclaw/sop/runs/failed");
    }

    #[test]
    fn failed_run_yields_the_topic_and_the_full_payload() {
        let run = run_with(SopRunStatus::Failed, Some("disk quota exceeded"));
        let (topic, payload) =
            run_failed_event_for(&run).expect("a failed run emits the run-failed event");

        assert_eq!(topic, "zeroclaw/sop/runs/failed");
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("the payload is valid JSON");
        assert_eq!(parsed["run_id"], "run-1");
        assert_eq!(parsed["sop_name"], "harness-deploy-executor");
        assert_eq!(parsed["failure_reason"], "disk quota exceeded");
        assert_eq!(parsed["started_at"], "2026-11-25T00:00:00Z");
        assert_eq!(parsed["completed_at"], "2026-11-25T00:01:00Z");
        assert_eq!(parsed["current_step"], 2);
        assert_eq!(parsed["total_steps"], 4);
        assert_eq!(parsed["source"], "gateway");
        // The classifier's input shape is exactly the spec's eight fields —
        // no extras, no omissions. (Both sides sorted for the comparison.)
        let expected_keys = [
            "completed_at",
            "current_step",
            "failure_reason",
            "run_id",
            "sop_name",
            "source",
            "started_at",
            "total_steps",
        ];
        let obj = parsed.as_object().expect("a JSON object payload");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys, expected_keys,
            "the payload carries exactly the spec's fields"
        );
    }

    #[test]
    fn completed_run_publishes_nothing_the_omits_pin() {
        let run = run_with(SopRunStatus::Completed, None);
        assert!(
            run_failed_event_for(&run).is_none(),
            "a completed run must not emit the run-failed event"
        );
    }

    #[test]
    fn cancelled_run_publishes_nothing() {
        let run = run_with(SopRunStatus::Cancelled, None);
        assert!(
            run_failed_event_for(&run).is_none(),
            "a cancelled run must not emit the run-failed event"
        );
    }

    #[test]
    fn missing_failure_reason_falls_back_to_a_non_empty_label() {
        // Every current Failed call site carries a reason, but a reason-less
        // failure must still publish a non-degenerate classifier input (an
        // empty/min-length-violating label would be dropped at the sidecar's
        // registered-topic validation — a silently dropped occurrence).
        let run = run_with(SopRunStatus::Failed, None);
        let (_, payload) =
            run_failed_event_for(&run).expect("a failed run emits even without a reason");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            parsed["failure_reason"], "unknown",
            "a missing reason degrades to a stable non-empty label"
        );
    }
}
