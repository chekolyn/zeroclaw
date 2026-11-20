//! Integration test: the provider-layer retry/rate-limit telemetry lands in
//! the OTel meter pipeline with the pinned instrument names, attribute sets,
//! bucket boundaries, and the Prometheus-exported name surface.
//!
//! One shared in-memory exporter backs the whole file (the global meter
//! provider is replaceable but instruments bind at first emit, so the
//! exporter is created once); each test resets the exporter, drives its
//! scripted provider flow under a paused tokio clock, force-flushes, and
//! asserts only the datapoints carrying its own unique provider name.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use opentelemetry::global;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics, Sum};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, PeriodicReader, SdkMeterProvider, Temporality,
};
use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_providers::reliable::ReliableModelProvider;

/// Scripted provider: serves the error script front-first (one error per
/// call), then succeeds — the `TimedRateLimitMockProvider` script shape,
/// without the clock recording.
struct ScriptedProvider {
    calls: Arc<AtomicUsize>,
    errors: Mutex<VecDeque<String>>,
    response: String,
}

impl ScriptedProvider {
    fn scripted(calls: &Arc<AtomicUsize>, errors: &[&str], response: &str) -> Self {
        Self {
            calls: Arc::clone(calls),
            errors: Mutex::new(
                errors
                    .iter()
                    .map(|error| (*error).to_string())
                    .collect::<VecDeque<String>>(),
            ),
            response: response.to_string(),
        }
    }

    fn next_outcome(&self) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let error = self
            .errors
            .lock()
            .expect("test error script mutex")
            .pop_front();
        match error {
            Some(error) => Err(anyhow::Error::msg(error)),
            None => Ok(self.response.clone()),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        self.next_outcome()
    }
}

impl Attributable for ScriptedProvider {
    fn role(&self) -> Role {
        Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
    }
    fn alias(&self) -> &str {
        "ScriptedProvider"
    }
}

struct TelemetryHarness {
    exporter: InMemoryMetricExporter,
    provider: SdkMeterProvider,
    lock: Mutex<()>,
}

/// One shared exporter for the whole test file: instruments bind to the
/// global meter provider at first emit and cannot rebind, so the provider
/// (and its in-memory exporter) is installed exactly once; tests serialize
/// on the file lock and isolate by resetting the exporter.
fn harness() -> &'static TelemetryHarness {
    static HARNESS: OnceLock<TelemetryHarness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        global::set_meter_provider(provider.clone());
        TelemetryHarness {
            exporter,
            provider,
            lock: Mutex::new(()),
        }
    })
}

const RATE_LIMIT_429: &str = "HTTP 429 Too Many Requests, Retry-After: 30";

/// The mandated wait-histogram bucket set (prod shape: base 2000 ms,
/// per-wait ceiling 60 s, per-call budget 300 s).
const WAIT_BUCKETS_MS: [f64; 11] = [
    250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0, 32000.0, 60000.0, 120000.0, 300000.0,
];

fn attrs_of<'a, I: IntoIterator<Item = &'a opentelemetry::KeyValue>>(
    attributes: I,
) -> std::collections::BTreeMap<String, String> {
    attributes
        .into_iter()
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().into_owned()))
        .collect()
}

fn sums<'a>(batches: &'a [ResourceMetrics], name: &str) -> Vec<&'a Sum<u64>> {
    batches
        .iter()
        .flat_map(|batch| batch.scope_metrics())
        .flat_map(|scope| scope.metrics())
        .filter(|metric| metric.name() == name)
        .filter_map(|metric| match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(sum),
            _ => None,
        })
        .collect()
}

fn sum_datapoints<'a>(
    batches: &'a [ResourceMetrics],
    name: &str,
) -> Vec<(std::collections::BTreeMap<String, String>, u64)> {
    sums(batches, name)
        .iter()
        .flat_map(|sum| sum.data_points())
        .map(|point| (attrs_of(point.attributes()), point.value()))
        .collect()
}

/// Datapoints of one instrument scoped to ONE provider: the meter's
/// cumulative series survive exporter resets across the file's tests, so
/// every assertion must scope to the test's own provider name.
fn sum_datapoints_for(
    batches: &[ResourceMetrics],
    name: &str,
    provider: &str,
) -> Vec<(std::collections::BTreeMap<String, String>, u64)> {
    sum_datapoints(batches, name)
        .into_iter()
        .filter(|(attrs, _)| attr(attrs, "gen_ai.provider.name") == provider)
        .collect()
}

fn histogram_datapoints(
    batches: &[ResourceMetrics],
    name: &str,
) -> Vec<(
    std::collections::BTreeMap<String, String>,
    u64,
    f64,
    Vec<f64>,
)> {
    batches
        .iter()
        .flat_map(|batch| batch.scope_metrics())
        .flat_map(|scope| scope.metrics())
        .filter(|metric| metric.name() == name)
        .filter_map(|metric| match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => Some(histogram),
            _ => None,
        })
        .flat_map(|histogram| histogram.data_points())
        .map(|point| {
            (
                attrs_of(point.attributes()),
                point.count(),
                point.sum(),
                point.bounds().collect(),
            )
        })
        .collect()
}

fn histogram_datapoints_for(
    batches: &[ResourceMetrics],
    name: &str,
    provider: &str,
) -> Vec<(
    std::collections::BTreeMap<String, String>,
    u64,
    f64,
    Vec<f64>,
)> {
    histogram_datapoints(batches, name)
        .into_iter()
        .filter(|(attrs, _, _, _)| attr(attrs, "gen_ai.provider.name") == provider)
        .collect()
}

fn metric_unit(batches: &[ResourceMetrics], name: &str) -> Option<String> {
    batches
        .iter()
        .flat_map(|batch| batch.scope_metrics())
        .flat_map(|scope| scope.metrics())
        .find(|metric| metric.name() == name)
        .map(|metric| metric.unit().to_string())
}

fn attr<'a>(map: &'a std::collections::BTreeMap<String, String>, key: &'a str) -> &'a str {
    map.get(key).map(String::as_str).unwrap_or("<missing>")
}

#[tokio::test(start_paused = true)]
async fn rate_limit_wait_emits_waits_counter_and_wait_histogram() {
    let harness = harness();
    let _guard = harness.lock.lock().expect("harness lock");
    harness.exporter.reset();

    let calls = Arc::new(AtomicUsize::new(0));
    let model_provider = ReliableModelProvider::new_pinned_for_test(
        "mock",
        vec![(
            "alpha.mock",
            "mock",
            "alpha-model",
            Arc::new(ScriptedProvider::scripted(
                &calls,
                &[RATE_LIMIT_429],
                "recovered",
            )),
        )],
        1,
        2000,
    );

    let served = model_provider
        .chat_with_system(None, "hello", "ignored", Some(0.0))
        .await
        .expect("the single retry after the in-place wait recovers");
    assert_eq!(served, "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one 429 then the 200");

    harness.provider.force_flush().expect("force flush");
    let batches = harness.exporter.get_finished_metrics().expect("exported");

    let wait_class = "rate_limit";
    let shaped_by = "retry_after";
    let waits = sum_datapoints_for(&batches, "zeroclaw.provider.waits", "alpha.mock");
    assert_eq!(waits.len(), 1, "exactly one wait was emitted: {waits:?}");
    assert_eq!(waits[0].1, 1);
    assert_eq!(attr(&waits[0].0, "gen_ai.provider.name"), "alpha.mock");
    assert_eq!(attr(&waits[0].0, "wait_class"), wait_class);
    assert_eq!(attr(&waits[0].0, "shaped_by"), shaped_by);
    // The `_total` Prometheus suffix is only legitimate for a monotonic
    // cumulative sum — pin the aggregation shape alongside the name.
    let waits_sum = sums(&batches, "zeroclaw.provider.waits");
    assert!(
        waits_sum[0].is_monotonic(),
        "the waits sum must be monotonic"
    );
    assert_eq!(waits_sum[0].temporality(), Temporality::Cumulative);

    let histograms = histogram_datapoints_for(&batches, "zeroclaw.provider.wait", "alpha.mock");
    assert_eq!(
        histograms.len(),
        1,
        "exactly one histogram point: {histograms:?}"
    );
    let (histogram_attrs, count, sum, bounds) = &histograms[0];
    assert_eq!(*count, 1);
    // The 30 s Retry-After dominates the 2 s doubling term; the ±20% jitter
    // band on 30000 ms is [24000, 36000].
    assert!(
        (24000.0..=36000.0).contains(sum),
        "wait sum {sum} outside the jitter band"
    );
    assert_eq!(
        bounds,
        &WAIT_BUCKETS_MS.to_vec(),
        "bucket boundaries pinned"
    );
    assert_eq!(attr(histogram_attrs, "gen_ai.provider.name"), "alpha.mock");
    assert_eq!(attr(histogram_attrs, "wait_class"), wait_class);
    assert_eq!(attr(histogram_attrs, "shaped_by"), shaped_by);
    assert_eq!(
        metric_unit(&batches, "zeroclaw.provider.wait").as_deref(),
        Some("ms"),
        "the ms unit pins the Prometheus milliseconds_* suffix"
    );

    assert!(
        sum_datapoints_for(
            &batches,
            "zeroclaw.provider.budget_exhaustions",
            "alpha.mock"
        )
        .is_empty(),
        "no budget was exhausted in this flow"
    );
    assert!(
        sum_datapoints_for(&batches, "zeroclaw.provider.cooldown_skips", "alpha.mock").is_empty(),
        "no cooldown skip happened in this flow"
    );
}

#[tokio::test(start_paused = true)]
async fn budget_exhaustion_emits_counter_and_sleeps_nothing() {
    let harness = harness();
    let _guard = harness.lock.lock().expect("harness lock");
    harness.exporter.reset();

    let calls = Arc::new(AtomicUsize::new(0));
    let model_provider = ReliableModelProvider::new_pinned_for_test(
        "mock",
        vec![(
            "beta.mock",
            "mock",
            "beta-model",
            Arc::new(ScriptedProvider::scripted(
                &calls,
                &[RATE_LIMIT_429, RATE_LIMIT_429],
                "never reached",
            )),
        )],
        2,
        2000,
    )
    .with_provider_rate_limit_budget_ms(1);

    let started = tokio::time::Instant::now();
    let result = model_provider
        .chat_with_system(None, "hello", "ignored", Some(0.0))
        .await;
    let elapsed = started.elapsed();

    let error = result.expect_err("the spent budget fails fast instead of waiting");
    assert!(
        error
            .to_string()
            .contains("failed after 2 failure event(s)"),
        "both 429s were consumed and the call failed fast: {error}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "fail-fast: no third call");
    // The first wait clamped to the 1 ms budget (one real sleep); the
    // exhausted return slept NOTHING — against a 30 s Retry-After term the
    // whole call stays inside a few paused-milliseconds.
    assert!(
        elapsed <= Duration::from_millis(2),
        "the exhausted return must not sleep: {elapsed:?}"
    );

    harness.provider.force_flush().expect("force flush");
    let batches = harness.exporter.get_finished_metrics().expect("exported");

    let exhaustions = sum_datapoints_for(
        &batches,
        "zeroclaw.provider.budget_exhaustions",
        "beta.mock",
    );
    assert_eq!(
        exhaustions.len(),
        1,
        "the silent return now counts: {exhaustions:?}"
    );
    assert_eq!(exhaustions[0].1, 1);
    assert_eq!(attr(&exhaustions[0].0, "gen_ai.provider.name"), "beta.mock");

    let waits = sum_datapoints_for(&batches, "zeroclaw.provider.waits", "beta.mock");
    assert_eq!(waits.len(), 1, "only the first 1 ms wait: {waits:?}");
    assert_eq!(attr(&waits[0].0, "shaped_by"), "budget_clamp");
    assert_eq!(attr(&waits[0].0, "wait_class"), "rate_limit");

    let histograms = histogram_datapoints_for(&batches, "zeroclaw.provider.wait", "beta.mock");
    assert_eq!(histograms.len(), 1);
    assert_eq!(histograms[0].1, 1, "count");
    assert_eq!(
        histograms[0].2, 1.0,
        "the clamped wait was exactly the budget"
    );
}

#[tokio::test(start_paused = true)]
async fn cooldown_skip_emits_counter_without_a_wait() {
    let harness = harness();
    let _guard = harness.lock.lock().expect("harness lock");
    harness.exporter.reset();

    let limited_calls = Arc::new(AtomicUsize::new(0));
    let skipped_calls = Arc::new(AtomicUsize::new(0));
    let served_calls = Arc::new(AtomicUsize::new(0));
    let model_provider = ReliableModelProvider::new_pinned_for_test(
        "mock",
        vec![
            (
                "gamma.mock",
                "mock",
                "gamma-model",
                Arc::new(ScriptedProvider::scripted(
                    &limited_calls,
                    &[RATE_LIMIT_429],
                    "never reached",
                )),
            ),
            (
                "gamma.mock",
                "mock",
                "gamma-alt-model",
                Arc::new(ScriptedProvider::scripted(
                    &skipped_calls,
                    &[],
                    "must be skipped",
                )),
            ),
            (
                "delta.mock",
                "mock",
                "delta-model",
                Arc::new(ScriptedProvider::scripted(
                    &served_calls,
                    &[],
                    "served-by-delta",
                )),
            ),
        ],
        1,
        2000,
    );

    let served = model_provider
        .chat_with_system(None, "hello", "ignored", Some(0.0))
        .await
        .expect("the heterogeneous rotation reaches the uncooled provider");
    assert_eq!(served, "served-by-delta");
    assert_eq!(limited_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        skipped_calls.load(Ordering::SeqCst),
        0,
        "the same-key sibling was skipped by the cooldown"
    );
    assert_eq!(served_calls.load(Ordering::SeqCst), 1);

    harness.provider.force_flush().expect("force flush");
    let batches = harness.exporter.get_finished_metrics().expect("exported");

    let skips = sum_datapoints_for(&batches, "zeroclaw.provider.cooldown_skips", "gamma.mock");
    assert_eq!(skips.len(), 1, "the skip now counts: {skips:?}");
    assert_eq!(skips[0].1, 1);
    assert_eq!(attr(&skips[0].0, "gen_ai.provider.name"), "gamma.mock");

    assert!(
        sum_datapoints_for(&batches, "zeroclaw.provider.waits", "gamma.mock").is_empty()
            && sum_datapoints_for(&batches, "zeroclaw.provider.waits", "delta.mock").is_empty(),
        "heterogeneous rotation never waits in place"
    );
    assert!(histogram_datapoints_for(&batches, "zeroclaw.provider.wait", "gamma.mock").is_empty());
    assert!(histogram_datapoints_for(&batches, "zeroclaw.provider.wait", "delta.mock").is_empty());
    assert!(
        sum_datapoints_for(
            &batches,
            "zeroclaw.provider.budget_exhaustions",
            "gamma.mock"
        )
        .is_empty()
            && sum_datapoints_for(
                &batches,
                "zeroclaw.provider.budget_exhaustions",
                "delta.mock"
            )
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn ordinary_backoff_emits_wait_class_backoff() {
    let harness = harness();
    let _guard = harness.lock.lock().expect("harness lock");
    harness.exporter.reset();

    let calls = Arc::new(AtomicUsize::new(0));
    let model_provider = ReliableModelProvider::new_pinned_for_test(
        "mock",
        vec![(
            "epsilon.mock",
            "mock",
            "epsilon-model",
            Arc::new(ScriptedProvider::scripted(
                &calls,
                &["HTTP 500 Internal Server Error"],
                "recovered",
            )),
        )],
        1,
        50,
    );

    let served = model_provider
        .chat_with_system(None, "hello", "ignored", Some(0.0))
        .await
        .expect("the ordinary backoff retry recovers");
    assert_eq!(served, "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    harness.provider.force_flush().expect("force flush");
    let batches = harness.exporter.get_finished_metrics().expect("exported");

    let waits = sum_datapoints_for(&batches, "zeroclaw.provider.waits", "epsilon.mock");
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(attr(&waits[0].0, "gen_ai.provider.name"), "epsilon.mock");
    assert_eq!(attr(&waits[0].0, "wait_class"), "backoff");
    assert_eq!(attr(&waits[0].0, "shaped_by"), "doubling");

    let histograms = histogram_datapoints_for(&batches, "zeroclaw.provider.wait", "epsilon.mock");
    assert_eq!(histograms.len(), 1);
    // The ordinary backoff has no jitter: the wait is exactly the base.
    assert_eq!(histograms[0].1, 1, "count");
    assert_eq!(histograms[0].2, 50.0, "wait == base_backoff_ms");
}

/// The Prometheus exported names, pinned so dashboard panels can never
/// silently lose their series. The collector's prometheusremotewrite
/// translation: dots → underscores; a monotonic sum gets `_total`; a
/// histogram's unit is spelled out (`ms` → `milliseconds`) and carries
/// `_bucket` / `_sum` / `_count` series. This matches the live surface the
/// existing instruments already export (`zeroclaw.llm.calls` →
/// `zeroclaw_llm_calls_total`).
fn prometheus_name(otel_name: &str, unit: Option<&str>, series: &str) -> String {
    fn unit_suffix(unit: &str) -> &str {
        match unit {
            "ms" => "milliseconds",
            "s" => "seconds",
            other => other,
        }
    }
    let mut base = otel_name.replace('.', "_");
    if let Some(unit) = unit {
        base.push('_');
        base.push_str(unit_suffix(unit));
    }
    match series {
        "counter" => format!("{base}_total"),
        other => format!("{base}_{other}"),
    }
}

#[test]
fn exported_metric_names_pin_the_prometheus_surface() {
    assert_eq!(
        prometheus_name("zeroclaw.provider.waits", None, "counter"),
        "zeroclaw_provider_waits_total"
    );
    assert_eq!(
        prometheus_name("zeroclaw.provider.budget_exhaustions", None, "counter"),
        "zeroclaw_provider_budget_exhaustions_total"
    );
    assert_eq!(
        prometheus_name("zeroclaw.provider.cooldown_skips", None, "counter"),
        "zeroclaw_provider_cooldown_skips_total"
    );
    assert_eq!(
        prometheus_name("zeroclaw.provider.wait", Some("ms"), "bucket"),
        "zeroclaw_provider_wait_milliseconds_bucket"
    );
    assert_eq!(
        prometheus_name("zeroclaw.provider.wait", Some("ms"), "sum"),
        "zeroclaw_provider_wait_milliseconds_sum"
    );
    assert_eq!(
        prometheus_name("zeroclaw.provider.wait", Some("ms"), "count"),
        "zeroclaw_provider_wait_milliseconds_count"
    );
}
