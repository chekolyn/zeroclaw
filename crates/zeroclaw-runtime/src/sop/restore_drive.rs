//! The boot-time restore-drive leg.
//!
//! A pod restart orphans every in-flight gateway SOP run: [`SopEngine`]'s
//! `restore_runs` rehydrates the non-terminal run rows at boot — and, before
//! this leg, nothing re-drove a restored `Running` run. The stuck-run reaper
//! failed each restored run ~30 minutes later as `stuck-run timeout`,
//! discarding the in-flight work and wedging the SOP's single exec lane for
//! the whole reap window.
//!
//! This leg closes the gap at daemon boot, FIRST GENERATION WITH AN SOP
//! ENGINE only, wired after the driver supervisor exists and BEFORE
//! `spawn_sop_maintenance` (its interval's first tick fires immediately and
//! runs the reaper) and before the MQTT listener admits new activations —
//! boot order: engine build → restore → **restore-drive** →
//! maintenance/channels.
//!
//! Per restored `Running` run (never `Pending` — maintenance's gated-pend
//! retry owns that class; never a parked run — the parked replay owns those):
//!
//! 1. **Construct** the resume action at the persisted step
//!    ([`SopEngine::resume_action_for_run`]) — fail-closed on an absent SOP
//!    definition, a changed definition, or an exhausted progress-grounded
//!    resume budget.
//! 2. **Pre-check** the headless-driver lease (read-only). A refused
//!    pre-check mints NO `resumed` event and no clock bump — the resume
//!    budget never counts a resume that never happened.
//! 3. **Mark** INLINE ([`SopEngine::mark_resumed_with_event`]): the progress
//!    clock restarts before any spawn latency, so the reaper stays off the
//!    marked run, and the `resumed` audit event lands in the same atomic
//!    store write.
//! 4. **Spawn** the headless driver for the first action, staggered — the
//!    boot-burst bound: a crash night's N restorations do not stampede the
//!    model concurrently, and the leg never amplifies beyond the
//!    pre-restart in-flight set (the restored claims mirror it 1:1).
//!
//! Unresumable classes fail closed at boot (`fast-reap: …`), freeing the
//! lane immediately instead of at +30 minutes — with ONE deferred look for
//! the absent-SOP class (the init-container may still be copying the SOP
//! directory): the first maintenance tick that still cannot find the
//! definition fast-fails the run.

use std::sync::{Arc, Mutex};

use super::audit::SopAuditLogger;
use super::engine::{ResumeActionError, SopEngine};
use super::executor::{SopDriverHandles, spawn_and_register_sop_driver_with_capability};
use super::types::{SopRunAction, SopRunStatus};
use crate::live_config_authority::AgentExecutionCapability;

/// How long the leg waits after a successful spawn before driving the next
/// restored run (the boot-burst stagger). The first spawn is not delayed; the
/// boot cost is bounded by (successful spawns − 1) × this offset, and N — the
/// number of restored runs — mirrors the pre-restart in-flight set, so the
/// leg never amplifies beyond the work that was already running. Pinned at
/// build as the per-spawn sleep offset with sequential (serialized) admission.
const SPAWN_STAGGER: std::time::Duration = std::time::Duration::from_millis(250);

/// Terminal reason for a run whose progress-grounded resume budget is
/// exhausted (the crash-loop guard; the lane frees at boot, not at +1800 s).
pub const FAST_REAP_BUDGET_REASON: &str = "fast-reap: resume budget exhausted";

/// Terminal reason for a run whose loaded SOP definition no longer matches
/// the persisted run at the count level. Fail-closed into discard (the
/// disclosed residual): resuming a semantically-drifted step is the worse
/// failure.
pub const FAST_REAP_DEFINITION_CHANGED_REASON: &str =
    "fast-reap: unresumable: sop definition changed";

/// Terminal reason for a run whose SOP definition was still absent at the
/// first maintenance tick after the restore-drive leg deferred it once.
pub const FAST_REAP_SOP_ABSENT_REASON: &str = "fast-reap: unresumable: sop definition absent";

/// What one pass of the leg did, for the boot log and the tests.
#[derive(Debug, Default, Clone)]
pub struct RestoreDriveOutcome {
    /// Restored `Running` runs the pass examined (0 for a pass over an empty
    /// set).
    pub examined: usize,
    /// Runs the leg marked and spawned a driver for.
    pub resumed: usize,
    /// Runs the leg left alone (lease held, parked/pending class, absent SOP
    /// deferred, or an operational store failure — fail-closed, no mark).
    pub skipped: usize,
    /// Runs the leg terminalized at boot (`fast-reap: …` reasons).
    pub fast_failed: usize,
}

/// What one run's leg pass decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerRun {
    Drove,
    Skipped,
    FastFailed,
}

/// Drive the restored `Running` runs at daemon boot.
///
/// Walks the engine's live `Running` runs (discovery is the store —
/// `restore_runs` already rehydrated them into `active_runs`), constructs each
/// run's resume action, marks the resume, and spawns its driver — staggered.
/// The lease pre-check makes a re-run of the leg a no-op for a run that is
/// already driven. Runs are examined in sorted run-id order so the boot log
/// is deterministic.
pub async fn drive_restored_runs(
    engine: Arc<Mutex<SopEngine>>,
    handles: &SopDriverHandles,
    config: zeroclaw_config::schema::Config,
    audit: Option<Arc<SopAuditLogger>>,
    execution_capability: Option<AgentExecutionCapability>,
) -> RestoreDriveOutcome {
    let run_ids: Vec<String> = {
        let guard = match engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut ids: Vec<String> = guard
            .active_runs()
            .iter()
            .filter(|(_, run)| run.status == SopRunStatus::Running)
            .map(|(run_id, _)| run_id.clone())
            .collect();
        ids.sort();
        ids
    };
    let mut outcome = RestoreDriveOutcome {
        examined: run_ids.len(),
        ..RestoreDriveOutcome::default()
    };
    let mut drove_last = false;
    for run_id in &run_ids {
        // Boot-burst stagger: after a successful spawn, bound the pace at
        // which successive drivers hit the model.
        if drove_last {
            tokio::time::sleep(SPAWN_STAGGER).await;
        }
        let decided = resume_one_run(
            &engine,
            handles,
            &config,
            audit.clone(),
            execution_capability.clone(),
            run_id,
        )
        .await;
        match decided {
            PerRun::Drove => outcome.resumed += 1,
            PerRun::Skipped => outcome.skipped += 1,
            PerRun::FastFailed => outcome.fast_failed += 1,
        }
        drove_last = decided == PerRun::Drove;
    }
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "examined": outcome.examined,
                "resumed": outcome.resumed,
                "skipped": outcome.skipped,
                "fast_failed": outcome.fast_failed,
            })
        ),
        &format!(
            "SOP restore-drive: examined {} restored running run(s): {} resumed, {} skipped, {} \
             fast-failed",
            outcome.examined, outcome.resumed, outcome.skipped, outcome.fast_failed
        )
    );
    outcome
}

/// The per-run leg pass: construct → pre-check → mark → spawn. The engine
/// lock is taken only inside each phase, never held across an await. An
/// absent SOP definition ALWAYS defers here (the boot pass cannot know
/// whether the SOP directory is still mid-copy); the engine's maintenance
/// tick owns the one deferred look — still absent there fast-fails the run.
async fn resume_one_run(
    engine: &Arc<Mutex<SopEngine>>,
    handles: &SopDriverHandles,
    config: &zeroclaw_config::schema::Config,
    audit: Option<Arc<SopAuditLogger>>,
    execution_capability: Option<AgentExecutionCapability>,
    run_id: &str,
) -> PerRun {
    // What the leg does with each classified failure, under the engine lock.
    // The lock is taken only here, never held across an await. The action is
    // boxed: a boxed step action keeps the enum's variants within one
    // size class.
    enum Prepared {
        Drive(Box<SopRunAction>),
        Skip(String),
        FastFailed(String),
    }
    fn fast_fail(guard: &mut SopEngine, run_id: &str, reason: &'static str) -> Prepared {
        match guard.finish_run(run_id, SopRunStatus::Failed, Some(reason.to_string())) {
            Ok(_) => Prepared::FastFailed(reason.to_string()),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "run_id": run_id,
                            "error": e.to_string(),
                        })),
                    "SOP restore-drive: failed to fast-fail the run; it stays active for the \
                     stuck-run reaper"
                );
                Prepared::Skip(format!("{reason} (the terminal write failed)"))
            }
        }
    }

    let (prepared, run_identity) = {
        let mut guard = match engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let identity = guard
            .active_runs()
            .get(run_id)
            .map(|run| (run.sop_name.clone(), run.current_step));
        // The lease pre-check FIRST: a refused pre-check mints no `resumed`
        // event and no clock bump — the resume budget never counts a resume
        // that never happened. NEVER hold a lease across the spawn: a held
        // lease makes the spawn's own acquisition fail and the run is then
        // settled as undriven (a silent boot-only loss).
        if guard.is_headless_driver_leased(run_id) {
            (
                Prepared::Skip("a headless driver already holds the run's lease".to_string()),
                identity,
            )
        } else {
            let decided = match guard.resume_action_for_run(run_id) {
                Ok(action @ SopRunAction::ExecuteStep { .. }) => Prepared::Drive(Box::new(action)),
                // The construction path yields only `ExecuteStep` (a Running
                // run is past its gate) or a schema-failed terminal action.
                Ok(other) => Prepared::Skip(format!(
                    "the resume produced a non-driving action ({other:?})"
                )),
                Err(ResumeActionError::SopAbsent { sop_name }) => {
                    // The absent class ALWAYS defers here: the boot pass does
                    // not know whether the SOP directory is still mid-copy.
                    // The engine's maintenance tick owns the one deferred
                    // look — still absent there fast-fails the run, no loops.
                    guard.defer_resume_absent(run_id);
                    Prepared::Skip(format!(
                        "SOP '{sop_name}' is not loaded; the run is deferred to the first \
                         maintenance tick (the init-container may still be copying)"
                    ))
                }
                Err(ResumeActionError::DefinitionChanged { .. }) => {
                    fast_fail(&mut guard, run_id, FAST_REAP_DEFINITION_CHANGED_REASON)
                }
                Err(ResumeActionError::ResumeBudgetExhausted { .. }) => {
                    fast_fail(&mut guard, run_id, FAST_REAP_BUDGET_REASON)
                }
                Err(ResumeActionError::NotResumableHere { status, .. }) => {
                    Prepared::Skip(match status {
                        Some(status) => format!("the run is {status}, not a Running run"),
                        None => "the run is not active in this engine".to_string(),
                    })
                }
                Err(ResumeActionError::Store { context }) => {
                    // Fail closed: a budget that cannot be read is not a
                    // budget that passed.
                    Prepared::Skip(format!("the store read failed: {context}"))
                }
            };
            (decided, identity)
        }
    };
    let action = match prepared {
        Prepared::Drive(action) => action,
        Prepared::Skip(reason) => {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({ "run_id": run_id })),
                &format!("SOP restore-drive: skipped run {run_id}: {reason}")
            );
            return PerRun::Skipped;
        }
        Prepared::FastFailed(reason) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({ "run_id": run_id })),
                &format!("SOP restore-drive: fast-failed run {run_id}: {reason}")
            );
            return PerRun::FastFailed;
        }
    };
    let (sop_name, current_step) = run_identity.unwrap_or_default();
    // The resume mark, INLINE — the reaper's idle window restarts before any
    // spawn latency. A refused mark persists nothing, so the run is left for
    // the reaper exactly as pre-C6 (fail-closed, never half-marked).
    {
        let mut guard = match engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(e) = guard.mark_resumed_with_event(run_id) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "error": e.to_string(),
                    })),
                "SOP restore-drive: the resume mark did not land; the run is not driven"
            );
            return PerRun::Skipped;
        }
    }
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "run_id": run_id,
                "sop_name": sop_name,
                "current_step": current_step,
                "trigger": "boot-restore",
            })
        ),
        &format!(
            "SOP restore-drive: resuming run {run_id} of '{sop_name}' at step {current_step} \
             (boot-restore)"
        )
    );
    // The staggered background spawn: the driver runs in the background; the
    // caller staggers successive spawns. A refusal here (a drained generation)
    // terminalizes the run through `settle_undriven_run` inside the spawn —
    // the honest residual: the mark stands, but the run is terminal, so the
    // mark is budget-inert.
    let spawned = spawn_and_register_sop_driver_with_capability(
        handles,
        config.clone(),
        Arc::clone(engine),
        audit,
        *action,
        execution_capability,
    );
    if !spawned {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({ "run_id": run_id })),
            "SOP restore-drive: the resume driver was refused at admission; the run was \
             settled undriven"
        );
        return PerRun::Skipped;
    }
    PerRun::Drove
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_config_authority::LiveConfigAuthority;
    use crate::sop::engine::RESUME_BUDGET_LIMIT;
    use crate::sop::store::sqlite::SqliteRunStore;
    use crate::sop::store::{InMemoryRunStore, PersistedRun, SopEventRecord, SopRunStore};
    use crate::sop::types::{
        SopEvent, SopExecutionMode, SopPriority, SopStep, SopStepResult, SopStepStatus,
        SopTriggerSource,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    const STEP_ONE_OUTPUT: &str = "step-one-output-marker";

    fn manual_event() -> SopEvent {
        SopEvent {
            source: SopTriggerSource::Manual,
            topic: None,
            payload: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// A three-step Auto SOP owned by the `stepper` agent — the acceptance
    /// probe SOP's shape (`execution_mode = "auto"`, three `Execute` steps).
    fn three_step_sop(name: &str) -> crate::sop::types::Sop {
        let mut sop = crate::sop::types::Sop {
            name: name.into(),
            description: format!("Test SOP: {name}"),
            version: "1.0.0".into(),
            priority: SopPriority::Normal,
            execution_mode: SopExecutionMode::Auto,
            triggers: vec![crate::sop::types::SopTrigger::Manual],
            steps: vec![
                SopStep {
                    number: 1,
                    title: "Step one".into(),
                    body: "Do step one".into(),
                    ..SopStep::default()
                },
                SopStep {
                    number: 2,
                    title: "Step two".into(),
                    body: "Do step two".into(),
                    ..SopStep::default()
                },
            ],
            cooldown_secs: 0,
            max_concurrent: 1,
            location: None,
            deterministic: false,
            admission_policy: crate::sop::types::SopAdmissionPolicy::Parallel,
            max_pending_approvals: 0,
            agent: Some("stepper".into()),
            decision: None,
        };
        sop.steps.push(SopStep {
            number: 3,
            title: "Step three".into(),
            body: "Do step three".into(),
            ..SopStep::default()
        });
        sop
    }

    fn step_result(step_number: u32, output: &str, completed_at: &str) -> SopStepResult {
        SopStepResult {
            step_number,
            status: SopStepStatus::Completed,
            output: output.into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            completed_at: Some(completed_at.into()),
            effective_agent: None,
            tool_calls: Vec::new(),
        }
    }

    /// The E2E daemon config: `stepper` enabled with a risk profile and bound
    /// to an OpenAI-compatible provider at `uri` on the chat-completions wire.
    fn e2e_config(tmp: &tempfile::TempDir, uri: &str) -> zeroclaw_config::schema::Config {
        use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig, WireApi};
        let mut providers = zeroclaw_config::providers::Providers::default();
        {
            let base = providers
                .models
                .ensure("openai", "mock")
                .expect("the openai slot exists");
            base.api_key = Some("test-key".into());
            base.model = Some("test-model".into());
            base.uri = Some(uri.into());
            base.wire_api = Some(WireApi::ChatCompletions);
        }
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            providers,
            ..Config::default()
        };
        config
            .risk_profiles
            .insert("stepper".into(), RiskProfileConfig::default());
        config.agents.insert(
            "stepper".into(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: "openai.mock".into(),
                risk_profile: "stepper".into(),
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    /// A chat-completions mock whose replies are distinguishable per request
    /// (`mock-output-1`, `mock-output-2`, …). Returns (server, request count).
    async fn mock_chat_server() -> (wiremock::MockServer, Arc<AtomicUsize>) {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        Mock::given(method("POST"))
            .respond_with(move |_request: &wiremock::Request| {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": format!("mock-output-{n}")}}]
                }))
            })
            .mount(&server)
            .await;
        (server, requests)
    }

    /// An engine over `store` with `sops` loaded and an execution capability
    /// minted from the daemon-shaped authority, as production builds engines.
    fn capable_engine(
        store: Arc<dyn SopRunStore>,
        sops: Vec<crate::sop::types::Sop>,
        config: &zeroclaw_config::schema::Config,
    ) -> SopEngine {
        let authority = LiveConfigAuthority::new(config.clone());
        let mut engine = SopEngine::new(config.sop.clone())
            .with_store(store)
            .with_execution_capability(authority.execution_capability());
        engine.set_sops_for_test(sops);
        engine
    }

    fn resumed_event_record(run_id: &str, ts: &str) -> SopEventRecord {
        SopEventRecord {
            run_id: run_id.to_string(),
            seq: 0,
            ts: ts.to_string(),
            kind: "resumed".to_string(),
            actor: Some("boot-restore".to_string()),
            reason: Some("pod-restart".to_string()),
            payload: serde_json::json!({}),
        }
    }

    /// The store's `resumed` rows for `run_id`.
    fn resumed_events(store: &dyn SopRunStore, run_id: &str) -> Vec<SopEventRecord> {
        store
            .list_events(run_id)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "resumed")
            .collect()
    }

    /// Overwrite the stored row for `run_id` with `run` at a bumped revision
    /// and `last_progress_at` (the way a crashed engine would leave it); a row
    /// that does not exist yet is seeded at revision 0.
    fn rewrite_row(
        store: &dyn SopRunStore,
        run: crate::sop::types::SopRun,
        last_progress_at: &str,
    ) {
        let revision = store
            .load_run(&run.run_id)
            .ok()
            .flatten()
            .map(|stored| stored.revision + 1)
            .unwrap_or(0);
        let mut persisted =
            PersistedRun::new(run, last_progress_at.to_string(), SopTriggerSource::Manual);
        persisted.revision = revision;
        store.save_run(&persisted).unwrap();
    }

    /// Wait for every driver this generation owns to settle.
    async fn drain_drivers(handles: &SopDriverHandles) {
        let drivers = handles.lock().unwrap().close_and_take();
        for driver in drivers {
            tokio::time::timeout(std::time::Duration::from_secs(120), driver)
                .await
                .expect("the driver settles")
                .unwrap();
        }
    }

    /// The two-engines-over-one-store resume: engine A starts a 3-step run,
    /// completes step 1, and is DROPPED (the simulated kill); engine B
    /// restores from the same store and the leg drives the run to terminal
    /// `Completed` at the persisted step, with step 1 never re-executed, the
    /// `resumed` audit event in the store, and the resumed steps' turns
    /// carrying the execution witness (they ran: the mock served each turn).
    #[tokio::test]
    async fn restore_drive_resumes_a_killed_run_at_its_persisted_step() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (server, requests) = mock_chat_server().await;
        let config = e2e_config(&tmp, &server.uri());
        let db = tmp.path().join("runs.db");
        let store = Arc::new(SqliteRunStore::open(&db).unwrap());
        let sop = three_step_sop("resume-e2e");

        let mut engine_a = capable_engine(store.clone(), vec![sop.clone()], &config);
        let action = engine_a.start_run("resume-e2e", manual_event()).unwrap();
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        engine_a
            .advance_step(
                &run_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        drop(engine_a);

        let mut engine_b = capable_engine(store.clone(), vec![sop], &config);
        engine_b.restore_runs();
        let engine = Arc::new(Mutex::new(engine_b));
        let handles = SopDriverHandles::default();

        let outcome = drive_restored_runs(
            Arc::clone(&engine),
            &handles,
            config.clone(),
            None,
            Some(LiveConfigAuthority::new(config.clone()).execution_capability()),
        )
        .await;
        assert_eq!(outcome.examined, 1);
        assert_eq!(outcome.resumed, 1, "the restored run is resumed");
        drain_drivers(&handles).await;

        let stored = store.load_run(&run_id).unwrap().expect("the run persists");
        assert_eq!(
            stored.run.status,
            SopRunStatus::Completed,
            "the resumed run reaches its terminal transition"
        );
        let executed: Vec<(u32, &str)> = stored
            .run
            .step_results
            .iter()
            .map(|result| (result.step_number, result.output.as_str()))
            .collect();
        assert_eq!(
            executed,
            vec![
                (1, STEP_ONE_OUTPUT),
                (2, "mock-output-1"),
                (3, "mock-output-2"),
            ],
            "step 1 is never re-executed; steps 2-3 executed on the resumed driver"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "the resumed driver executed exactly the remaining steps"
        );
        let resumed = resumed_events(store.as_ref(), &run_id);
        assert_eq!(resumed.len(), 1, "exactly one resume was marked");
        assert_eq!(resumed[0].actor.as_deref(), Some("boot-restore"));
        assert_eq!(resumed[0].reason.as_deref(), Some("pod-restart"));
    }

    /// A run whose driver already holds the lease is not re-driven, and the
    /// refused pre-check mints NO `resumed` event and no clock bump — the
    /// resume budget never counts a resume that never happened.
    #[tokio::test]
    async fn restore_drive_refuses_an_already_driven_run_without_minting_a_resume() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = e2e_config(&tmp, "http://127.0.0.1:1");
        let store = Arc::new(InMemoryRunStore::new());
        let sop = three_step_sop("leased");

        let mut engine_a = capable_engine(store.clone(), vec![sop.clone()], &config);
        let action = engine_a.start_run("leased", manual_event()).unwrap();
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        engine_a
            .advance_step(
                &run_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        drop(engine_a);

        let mut engine_b = capable_engine(store.clone(), vec![sop], &config);
        engine_b.restore_runs();
        // The first driver owns the run: hold its lease through the pass.
        engine_b.try_lease_headless_driver(&run_id);
        let engine = Arc::new(Mutex::new(engine_b));
        let before = store.load_run(&run_id).unwrap().unwrap();
        let handles = SopDriverHandles::default();
        let outcome =
            drive_restored_runs(Arc::clone(&engine), &handles, config.clone(), None, None).await;
        assert_eq!(outcome.resumed, 0, "the leased run is not re-driven");
        assert_eq!(handles.lock().unwrap().len(), 0, "no driver was spawned");
        let after = store.load_run(&run_id).unwrap().unwrap();
        assert_eq!(
            after.last_progress_at, before.last_progress_at,
            "the refused pre-check must not bump the progress clock"
        );
        assert!(
            resumed_events(store.as_ref(), &run_id).is_empty(),
            "the refused pre-check must not mint a resumed event"
        );
    }

    /// A run whose loaded SOP definition no longer matches the persisted run
    /// is fast-failed AT BOOT with the readable reason — the lane frees
    /// immediately, not at +1800 s — and is never driven.
    #[tokio::test]
    async fn restore_drive_fast_fails_a_definition_changed_run_at_boot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = e2e_config(&tmp, "http://127.0.0.1:1");
        let store = Arc::new(InMemoryRunStore::new());

        let mut engine_a = capable_engine(store.clone(), vec![three_step_sop("drifted")], &config);
        let action = engine_a.start_run("drifted", manual_event()).unwrap();
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        engine_a
            .advance_step(
                &run_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        drop(engine_a);

        // The SOP was promoted with a shortened step list between the roll's
        // clone states: the loaded definition has 2 steps, the persisted run 3.
        let mut drifted = three_step_sop("drifted");
        drifted.steps.pop();
        let mut engine_b = capable_engine(store.clone(), vec![drifted], &config);
        engine_b.restore_runs();
        let engine = Arc::new(Mutex::new(engine_b));
        let handles = SopDriverHandles::default();

        let outcome =
            drive_restored_runs(Arc::clone(&engine), &handles, config.clone(), None, None).await;
        assert_eq!(outcome.fast_failed, 1, "the drifted run is fast-failed");
        assert_eq!(outcome.resumed, 0, "a drifted run is never driven");
        assert_eq!(handles.lock().unwrap().len(), 0);
        let stored = store.load_run(&run_id).unwrap().unwrap();
        assert_eq!(stored.run.status, SopRunStatus::Failed);
        assert_eq!(
            stored.run.failure_reason.as_deref(),
            Some(FAST_REAP_DEFINITION_CHANGED_REASON)
        );
        assert!(resumed_events(store.as_ref(), &run_id).is_empty());
    }

    /// Parked (`WaitingApproval`/`PausedCheckpoint`) and `Pending` runs are
    /// NOT driven by the leg: the parked replay owns the parked classes and
    /// maintenance's gated-pend retry owns `Pending`.
    #[tokio::test]
    async fn restore_drive_leaves_parked_and_pending_runs_undriven() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = e2e_config(&tmp, "http://127.0.0.1:1");
        let store = Arc::new(InMemoryRunStore::new());

        let mut engine_a = capable_engine(store.clone(), vec![three_step_sop("states")], &config);
        let action = engine_a.start_run("states", manual_event()).unwrap();
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        let live = engine_a.active_runs()[&run_id].clone();

        for (suffix, status) in [
            ("waiting", SopRunStatus::WaitingApproval),
            ("checkpoint", SopRunStatus::PausedCheckpoint),
            ("pending", SopRunStatus::Pending),
        ] {
            let mut row = live.clone();
            row.run_id = format!("{run_id}-{suffix}");
            row.status = status;
            rewrite_row(store.as_ref(), row, "2026-01-01T00:00:00Z");
        }
        // The live run itself goes terminal so the leg's enumeration holds
        // only the crafted parked/pending rows.
        engine_a
            .finish_run(&run_id, SopRunStatus::Cancelled, None)
            .unwrap();
        drop(engine_a);

        let mut engine_b = capable_engine(store.clone(), vec![three_step_sop("states")], &config);
        engine_b.restore_runs();
        let engine = Arc::new(Mutex::new(engine_b));
        let handles = SopDriverHandles::default();

        let outcome =
            drive_restored_runs(Arc::clone(&engine), &handles, config.clone(), None, None).await;
        assert_eq!(outcome.examined, 0, "no parked or pending run is examined");
        assert_eq!(handles.lock().unwrap().len(), 0);
        for suffix in ["waiting", "checkpoint", "pending"] {
            let stored = store
                .load_run(&format!("{run_id}-{suffix}"))
                .unwrap()
                .unwrap();
            assert!(
                stored.run.status != SopRunStatus::Failed,
                "a parked/pending run is never fast-failed by the leg"
            );
            assert!(resumed_events(store.as_ref(), &format!("{run_id}-{suffix}")).is_empty());
        }
    }

    /// A run whose progress-grounded resume budget is exhausted is
    /// fast-failed terminally AT BOOT — the lane frees instead of wedging for
    /// the reap window.
    #[tokio::test]
    async fn restore_drive_fast_fails_a_budget_exhausted_run_at_boot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = e2e_config(&tmp, "http://127.0.0.1:1");
        let store = Arc::new(InMemoryRunStore::new());

        let mut engine_a = capable_engine(store.clone(), vec![three_step_sop("looped")], &config);
        let action = engine_a.start_run("looped", manual_event()).unwrap();
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        engine_a
            .advance_step(
                &run_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        drop(engine_a);

        // Three resumes with no landed step result after them: the pod is
        // crash-looping without the run progressing.
        for i in 1..=RESUME_BUDGET_LIMIT {
            store
                .append_event(&resumed_event_record(
                    &run_id,
                    &format!("2026-01-02T00:00:{i:02}Z"),
                ))
                .unwrap();
        }

        let mut engine_b = capable_engine(store.clone(), vec![three_step_sop("looped")], &config);
        engine_b.restore_runs();
        let engine = Arc::new(Mutex::new(engine_b));
        let handles = SopDriverHandles::default();

        let outcome =
            drive_restored_runs(Arc::clone(&engine), &handles, config.clone(), None, None).await;
        assert_eq!(outcome.fast_failed, 1);
        let stored = store.load_run(&run_id).unwrap().unwrap();
        assert_eq!(stored.run.status, SopRunStatus::Failed);
        assert_eq!(
            stored.run.failure_reason.as_deref(),
            Some(FAST_REAP_BUDGET_REASON)
        );
        assert_eq!(handles.lock().unwrap().len(), 0);
    }

    /// The boot pass DEFERS an absent-SOP run (the init-container may still be
    /// copying the SOP directory): no fast-fail, no drive, no mark. The first
    /// maintenance tick that still cannot find the definition fast-fails the
    /// run with the absent reason — one deferred look, no loops.
    #[tokio::test]
    async fn deferred_absent_sop_fast_fails_on_the_maintenance_look() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = e2e_config(&tmp, "http://127.0.0.1:1");
        let store = Arc::new(InMemoryRunStore::new());

        let mut engine_a = capable_engine(store.clone(), vec![three_step_sop("late")], &config);
        let action = engine_a.start_run("late", manual_event()).unwrap();
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        engine_a
            .advance_step(
                &run_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        drop(engine_a);

        // Engine B boots without the SOP loaded (the copy is mid-flight).
        let mut engine_b = capable_engine(store.clone(), vec![three_step_sop("other")], &config);
        engine_b.restore_runs();
        let engine = Arc::new(Mutex::new(engine_b));
        let handles = SopDriverHandles::default();

        let outcome =
            drive_restored_runs(Arc::clone(&engine), &handles, config.clone(), None, None).await;
        assert_eq!(
            outcome.fast_failed, 0,
            "the absent-SOP run is deferred, not failed"
        );
        assert_eq!(outcome.resumed, 0);
        let stored = store.load_run(&run_id).unwrap().unwrap();
        assert_eq!(stored.run.status, SopRunStatus::Running, "still restorable");
        assert!(resumed_events(store.as_ref(), &run_id).is_empty());

        // The one deferred look: still absent → the unresumable class
        // fast-fails at the first maintenance tick.
        let summary = { engine.lock().unwrap().run_maintenance_tick() };
        assert_eq!(summary.fast_failed_unresumable, 1);
        let stored = store.load_run(&run_id).unwrap().unwrap();
        assert_eq!(stored.run.status, SopRunStatus::Failed);
        assert_eq!(
            stored.run.failure_reason.as_deref(),
            Some(FAST_REAP_SOP_ABSENT_REASON)
        );
    }

    /// A deferred absent-SOP run whose definition IS loaded by the time of the
    /// deferred look clears the deferral and stays alive (the no-loop exit;
    /// the next boot's restore drive resumes it).
    #[tokio::test]
    async fn deferred_absent_sop_clears_when_the_definition_arrives() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = e2e_config(&tmp, "http://127.0.0.1:1");
        let store = Arc::new(InMemoryRunStore::new());

        let mut engine_a = capable_engine(store.clone(), vec![three_step_sop("late")], &config);
        let action = engine_a.start_run("late", manual_event()).unwrap();
        let run_id = match &action {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        engine_a
            .advance_step(
                &run_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        drop(engine_a);

        let mut engine_b = capable_engine(store.clone(), vec![three_step_sop("other")], &config);
        engine_b.restore_runs();
        let engine = Arc::new(Mutex::new(engine_b));
        let handles = SopDriverHandles::default();
        drive_restored_runs(Arc::clone(&engine), &handles, config.clone(), None, None).await;

        // The definition arrives before the deferred look.
        engine
            .lock()
            .unwrap()
            .set_sops_for_test(vec![three_step_sop("late")]);
        let summary = { engine.lock().unwrap().run_maintenance_tick() };
        assert_eq!(summary.fast_failed_unresumable, 0);
        let stored = store.load_run(&run_id).unwrap().unwrap();
        assert_eq!(stored.run.status, SopRunStatus::Running);
        // The deferral cleared: a second tick does not fast-fail.
        let summary = { engine.lock().unwrap().run_maintenance_tick() };
        assert_eq!(summary.fast_failed_unresumable, 0);
    }

    /// The leg walks EVERY restored Running run, staggered.
    #[tokio::test]
    async fn restore_drive_walks_every_restored_running_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (server, requests) = mock_chat_server().await;
        let config = e2e_config(&tmp, &server.uri());
        let store = Arc::new(InMemoryRunStore::new());

        let mut engine_a = capable_engine(
            store.clone(),
            vec![three_step_sop("walk-a"), three_step_sop("walk-b")],
            &config,
        );
        let first = engine_a.start_run("walk-a", manual_event()).unwrap();
        let second = engine_a.start_run("walk-b", manual_event()).unwrap();
        let first_id = match &first {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        let second_id = match &second {
            SopRunAction::ExecuteStep { run_id, .. } => run_id.clone(),
            other => panic!("an auto SOP starts on an ExecuteStep, got {other:?}"),
        };
        engine_a
            .advance_step(
                &first_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        engine_a
            .advance_step(
                &second_id,
                step_result(1, STEP_ONE_OUTPUT, "2026-01-01T00:00:01Z"),
            )
            .unwrap();
        drop(engine_a);

        let mut engine_b = capable_engine(
            store.clone(),
            vec![three_step_sop("walk-a"), three_step_sop("walk-b")],
            &config,
        );
        engine_b.restore_runs();
        let engine = Arc::new(Mutex::new(engine_b));
        let handles = SopDriverHandles::default();

        let outcome = drive_restored_runs(
            Arc::clone(&engine),
            &handles,
            config.clone(),
            None,
            Some(LiveConfigAuthority::new(config.clone()).execution_capability()),
        )
        .await;
        assert_eq!(outcome.examined, 2);
        assert_eq!(outcome.resumed, 2);
        drain_drivers(&handles).await;

        for run_id in [&first_id, &second_id] {
            let stored = store.load_run(run_id).unwrap().unwrap();
            assert_eq!(stored.run.status, SopRunStatus::Completed);
            assert_eq!(resumed_events(store.as_ref(), run_id).len(), 1);
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            4,
            "two remaining steps per run"
        );
    }
}
