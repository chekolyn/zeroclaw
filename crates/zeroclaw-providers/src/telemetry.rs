//! Provider-layer OTel metric emission for the retry/rate-limit wait path.
//!
//! The provider layer is the only place that holds all four competing wait
//! terms (doubling, Retry-After, cooldown, cap/budget clamps), so the
//! instruments are emitted HERE — never through the runtime's observer event
//! pipeline. Instruments are created LAZILY on first emit: the global meter
//! provider is installed at observer construction during boot, so an
//! eagerly-created instrument would bind to the no-op provider and stay
//! silent forever.
//!
//! Cardinality rule: attributes are bounded enums only — never model,
//! attempt, error text, endpoint, or turn id.
//!
//! With the `observability-otel` feature off, every emission compiles to a
//! no-op.

/// How the final backoff wait was shaped — the single vocabulary both the
/// rate-limit curve and the ordinary retry backoff classify into.
///
/// The rate-limit path's priority order is: the clamp that bound the wait
/// wins first (`BudgetClamp`, then `CapClamp`), then the dominant pre-jitter
/// term (`Cooldown` > `RetryAfter` > `Doubling`). The `Cooldown` arm is
/// EXACTLY the historical `cooldown_shaped` predicate — the equivalence is
/// pinned by
/// `shaped_by_cooldown_arm_equals_the_cooldown_shaped_predicate` in
/// `reliable.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShapedBy {
    /// The remaining per-call budget clamped the wait.
    BudgetClamp,
    /// The per-wait ceiling clamped the wait.
    CapClamp,
    /// The remaining cooldown dominated the pre-jitter terms.
    Cooldown,
    /// A parsed Retry-After dominated the doubling term.
    RetryAfter,
    /// A parsed Retry-After exceeded the ordinary backoff's 30 s cap.
    RetryAfterCapped,
    /// The overload floor raised the wait.
    OverloadFloor,
    /// The doubling curve shaped the wait (the default arm).
    Doubling,
}

impl ShapedBy {
    /// The meter label vocabulary — consumed only by the live emission
    /// path, so the method exists only under the feature.
    #[cfg(feature = "observability-otel")]
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            Self::BudgetClamp => "budget_clamp",
            Self::CapClamp => "cap_clamp",
            Self::Cooldown => "cooldown",
            Self::RetryAfter => "retry_after",
            Self::RetryAfterCapped => "retry_after_capped",
            Self::OverloadFloor => "overload_floor",
            Self::Doubling => "doubling",
        }
    }
}

/// Whether a wait was the rate-limit in-place curve or the ordinary
/// retryable-error backoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitClass {
    /// The ordinary retryable-error backoff (`compute_backoff`).
    Backoff,
    /// The rate-limit in-place wait curve (`rate_limit_wait_ms`).
    RateLimit,
}

impl WaitClass {
    #[cfg(feature = "observability-otel")]
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            Self::Backoff => "backoff",
            Self::RateLimit => "rate_limit",
        }
    }
}

/// Emit one backoff wait: increments the waits counter and records the wait
/// duration in the histogram. One computation site → two emissions, so the
/// log record and the meter can never disagree.
pub(crate) fn emit_wait(
    provider_name: &str,
    wait_class: WaitClass,
    shaped_by: ShapedBy,
    wait_ms: u64,
) {
    #[cfg(feature = "observability-otel")]
    live::emit_wait(provider_name, wait_class, shaped_by, wait_ms);
    #[cfg(not(feature = "observability-otel"))]
    {
        let _ = (provider_name, wait_class, shaped_by, wait_ms);
    }
}

/// Emit the budget-exhausted fail-fast: the per-call rate-limit wait budget
/// is spent and the caller returns without sleeping (previously silent).
pub(crate) fn emit_budget_exhausted(provider_name: &str) {
    #[cfg(feature = "observability-otel")]
    live::emit_budget_exhausted(provider_name);
    #[cfg(not(feature = "observability-otel"))]
    {
        let _ = provider_name;
    }
}

/// Emit a cooldown skip: a candidate was passed over because its shared
/// cooldown window was still active (previously only a DEBUG log).
pub(crate) fn emit_cooldown_skip(provider_name: &str) {
    #[cfg(feature = "observability-otel")]
    live::emit_cooldown_skip(provider_name);
    #[cfg(not(feature = "observability-otel"))]
    {
        let _ = provider_name;
    }
}

#[cfg(feature = "observability-otel")]
mod live {
    use super::{ShapedBy, WaitClass};
    use opentelemetry::KeyValue;
    use opentelemetry::global;
    use opentelemetry::metrics::{Counter, Histogram};
    use std::sync::OnceLock;

    /// Histogram boundaries sized for the prod shape (RL-KNOBS default base
    /// 2000 ms, per-wait ceiling 60 s, per-call budget 300 s): the
    /// 250 ms–300 s band covers short Retry-After honours up to the
    /// budget-ceiling tail.
    const WAIT_BUCKETS_MS: [f64; 11] = [
        250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0, 32000.0, 60000.0, 120000.0, 300000.0,
    ];

    struct ProviderInstruments {
        waits: Counter<u64>,
        wait: Histogram<f64>,
        budget_exhaustions: Counter<u64>,
        cooldown_skips: Counter<u64>,
    }

    /// Lazily created on FIRST emit (module-level `OnceLock`, never a static
    /// initializer): the global meter provider is installed at observer
    /// construction during boot, so instruments created earlier would bind
    /// to the no-op provider and stay silent forever.
    static INSTRUMENTS: OnceLock<ProviderInstruments> = OnceLock::new();

    fn instruments() -> &'static ProviderInstruments {
        INSTRUMENTS.get_or_init(|| {
            let meter = global::meter("zeroclaw");
            ProviderInstruments {
                waits: meter
                    .u64_counter("zeroclaw.provider.waits")
                    .with_description("Provider backoff waits by wait class and shaping term")
                    .build(),
                wait: meter
                    .f64_histogram("zeroclaw.provider.wait")
                    .with_description("Provider backoff wait duration in milliseconds")
                    .with_unit("ms")
                    .with_boundaries(WAIT_BUCKETS_MS.to_vec())
                    .build(),
                budget_exhaustions: meter
                    .u64_counter("zeroclaw.provider.budget_exhaustions")
                    .with_description(
                        "Rate-limit wait budgets exhausted before a wait (the fail-fast return)",
                    )
                    .build(),
                cooldown_skips: meter
                    .u64_counter("zeroclaw.provider.cooldown_skips")
                    .with_description(
                        "Provider candidates skipped while their rate-limit cooldown was active",
                    )
                    .build(),
            }
        })
    }

    fn wait_attrs(
        provider_name: &str,
        wait_class: WaitClass,
        shaped_by: ShapedBy,
    ) -> [KeyValue; 3] {
        [
            KeyValue::new("gen_ai.provider.name", provider_name.to_string()),
            KeyValue::new("wait_class", wait_class.as_label()),
            KeyValue::new("shaped_by", shaped_by.as_label()),
        ]
    }

    pub(super) fn emit_wait(
        provider_name: &str,
        wait_class: WaitClass,
        shaped_by: ShapedBy,
        wait_ms: u64,
    ) {
        let instruments = instruments();
        let attrs = wait_attrs(provider_name, wait_class, shaped_by);
        instruments.waits.add(1, &attrs);
        instruments.wait.record(wait_ms as f64, &attrs);
    }

    pub(super) fn emit_budget_exhausted(provider_name: &str) {
        instruments().budget_exhaustions.add(
            1,
            &[KeyValue::new(
                "gen_ai.provider.name",
                provider_name.to_string(),
            )],
        );
    }

    pub(super) fn emit_cooldown_skip(provider_name: &str) {
        instruments().cooldown_skips.add(
            1,
            &[KeyValue::new(
                "gen_ai.provider.name",
                provider_name.to_string(),
            )],
        );
    }
}

#[cfg(all(test, not(feature = "observability-otel")))]
mod off_gate_pins {
    //! The production image builds with `--no-default-features` and an
    //! explicit feature list (Containerfile) — when `observability-otel` is
    //! absent from that list, every emission MUST compile to nothing. This
    //! target compiles only in that state; the metric-asserting twin lives
    //! in `tests/rate_limit_telemetry.rs`, required-features-gated to the
    //! opposite state. Together they pin BOTH directions of the SINGLE gate
    //! (no second feature name exists that could turn the emission on).
    #[test]
    fn emission_compiles_to_a_noop_when_the_feature_is_absent() {
        super::emit_wait(
            "off-gate-probe",
            super::WaitClass::Backoff,
            super::ShapedBy::Doubling,
            1,
        );
        super::emit_budget_exhausted("off-gate-probe");
        super::emit_cooldown_skip("off-gate-probe");
    }
}

#[cfg(all(test, feature = "observability-otel"))]
mod on_gate_pins {
    //! The single-gate twin: this target compiles only when
    //! `observability-otel` is present, and the calls reach the `live`
    //! module's instruments (via the no-op global meter here — the
    //! datapoint assertions live in the integration test).
    #[test]
    fn emission_compiles_through_the_live_path_when_the_feature_is_present() {
        super::emit_wait(
            "on-gate-probe",
            super::WaitClass::RateLimit,
            super::ShapedBy::RetryAfter,
            1,
        );
        super::emit_budget_exhausted("on-gate-probe");
        super::emit_cooldown_skip("on-gate-probe");
    }
}
