//! OTel bridge: export `tracing` spans to OpenTelemetry via a
//! `tracing-opentelemetry` `OpenTelemetryLayer`, wired through a
//! `tracing_subscriber::reload` slot.
//!
//! ## Why a reload slot
//!
//! `install_global_subscriber` runs early in boot, before the config is
//! loaded and before the global tracer provider is set (the `OtelObserver`
//! sets it later, once config is available). The bridge layer needs a real
//! tracer, but the subscriber — once installed globally — cannot be replaced.
//!
//! The `reload::Layer` slot solves this: the subscriber is installed early
//! (so boot logs land) with a no-op bridge layer, then the real bridge is
//! swapped in once the provider is set. No boot-log regression, no implicit
//! ordering invariant, no config/env coupling.
//!
//! ## Why the global tracer is safe to call twice
//!
//! The bridge layer binds to `global::tracer("zeroclaw")`. In
//! `opentelemetry 0.32` the global provider is a `RwLock` (not a per-name
//! cache): `set_tracer_provider` replaces the inner provider, and
//! `global::tracer` delegates to the *current* provider on each call. So the
//! layer built at install time holds a no-op `BoxedTracer` (provider not yet
//! set), and the layer built at activation time holds the real `BoxedTracer`.
//! Both are `OpenTelemetryLayer<Registry, BoxedTracer>` — the same type — so
//! `reload::Handle::modify` can swap them in place.

#![cfg(feature = "otel-bridge")]

use std::sync::OnceLock;

use opentelemetry::global::{self, BoxedTracer};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::reload::{self, Handle};

/// A `tracing-subscriber` layer that exports `tracing` spans to OTel via the
/// global tracer. The subscriber (`S`) + tracer (`T`) are fixed to
/// `Registry` + `BoxedTracer` so the pre- and post-provider-set layers are
/// the *same* type — a requirement for `reload::Handle::modify` to swap them.
pub type OtelBridgeLayer = OpenTelemetryLayer<Registry, BoxedTracer>;

/// The reload slot wrapping the bridge layer. Added to the subscriber at
/// install time; the inner layer is swapped at activation time.
pub type OtelBridgeReloadLayer = reload::Layer<OtelBridgeLayer, Registry>;

/// Handle used to swap the bridge layer from no-op to real after the global
/// tracer provider has been set.
pub type OtelBridgeHandle = Handle<OtelBridgeLayer, Registry>;

/// Build a bridge layer bound to the *current* global tracer.
///
/// Before `global::set_tracer_provider` is called, the global tracer is a
/// no-op, so this layer silently drops spans. After the provider is set
/// (e.g. by `OtelObserver::new`), a fresh layer built here exports to the
/// real backend.
fn bridge_layer() -> OtelBridgeLayer {
    OpenTelemetryLayer::new(global::tracer("zeroclaw"))
}

/// Global stash for the bridge reload handle, so `install_global_subscriber`
/// (which owns the subscriber assembly) can park the handle and the observer
/// init (a separate crate, later in boot) can activate the bridge without
/// threading the handle through every call site.
static OTEL_BRIDGE_HANDLE: OnceLock<OtelBridgeHandle> = OnceLock::new();

/// Build the reload slot for the OTel bridge.
///
/// Returns the layer to add to the subscriber (the slot, initially holding a
/// no-op bridge) and the handle used to swap the real bridge in later. Call
/// this from `install_global_subscriber` when assembling the subscriber.
pub(crate) fn build_otel_bridge_slot() -> (OtelBridgeReloadLayer, OtelBridgeHandle) {
    reload::Layer::new(bridge_layer())
}

/// Stash the bridge handle in the process-global slot. Called once from
/// `install_global_subscriber` so `activate_otel_bridge` can reach it later.
pub(crate) fn stash_otel_bridge_handle(handle: OtelBridgeHandle) {
    // `set` fails (silently) if called twice; the subscriber is installed once
    // per process, so this never loses a handle in practice.
    let _ = OTEL_BRIDGE_HANDLE.set(handle);
}

/// Activate the OTel bridge via an explicit handle: swap the no-op
/// (pre-provider) layer for the real one, bound to the now-set global tracer.
///
/// Idempotent + fail-safe: a no-op if the handle is stale or the subscriber
/// has been dropped. Tests use this directly; production uses
/// [`activate_otel_bridge`] (which reads the stashed handle).
pub fn activate_otel_bridge_with(handle: &OtelBridgeHandle) {
    // `modify` replaces the inner layer in place; the new layer rebinds to the
    // now-real global tracer (the provider was set since the slot was built).
    let _ = handle.modify(|layer| *layer = bridge_layer());
}

/// Activate the OTel bridge using the process-global stashed handle.
///
/// Call this exactly once, after the global tracer provider has been set
/// (e.g. after `OtelObserver::new`). No-op if no handle was stashed (e.g. the
/// `otel-bridge` feature is off or the subscriber wasn't installed).
pub fn activate_otel_bridge() {
    if let Some(handle) = OTEL_BRIDGE_HANDLE.get() {
        activate_otel_bridge_with(handle);
    }
}

#[cfg(all(test, feature = "otel-bridge"))]
mod tests {
    use super::*;
    use opentelemetry::global;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::layer::SubscriberExt;

    /// A `tracing` span emitted under a subscriber carrying the OTel bridge
    /// layer must be exported to OTel (the in-memory exporter captures it).
    /// This proves the bridge layer is constructed correctly against the
    /// `opentelemetry 0.32` + `tracing-opentelemetry 0.33` API and that spans
    /// flow end-to-end once the global tracer provider is set.
    #[test]
    fn tracing_span_exports_to_otel_via_bridge() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_tracer_provider(provider.clone());

        // The bridge layer binds to the now-real global tracer.
        let layer = bridge_layer();
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("sop_engine", sop_name = "dashboard_updater");
            let _guard = span.enter();
            tracing::info!(target: "zeroclaw_sop", "sop executing");
        });

        let _ = provider.force_flush();
        let spans = exporter
            .get_finished_spans()
            .expect("in-memory exporter should return spans");
        assert!(
            spans.iter().any(|s| s.name == "sop_engine"),
            "the tracing `sop_engine` span should be exported to OTel: {spans:?}"
        );
    }

    /// The reload slot lets the bridge swap from no-op to real. Before
    /// activation (provider not set), spans are NOT exported; after
    /// `activate_otel_bridge`, they are. This validates the swap mechanism
    /// that `install_global_subscriber` relies on.
    #[test]
    fn reload_slot_swaps_noop_to_real() {
        // Phase 1: slot built with no-op (no provider set yet). Emit a span
        // → the no-op bridge drops it (no provider to export to). This phase
        // asserts the no-op path doesn't panic; it does not read an exporter
        // (none is wired yet).
        let exporter = InMemorySpanExporter::default();

        let (slot, handle) = build_otel_bridge_slot();
        let subscriber = tracing_subscriber::registry().with(slot);

        // Wrap in `Dispatch` (Arc-backed, `Clone`): `with_default` moves its
        // argument, but cloning a `Dispatch` only bumps the refcount, so the
        // original `dispatch` keeps the subscriber (and the reload slot's
        // `Weak<RwLock<L>>`) alive across both phases — the phase-2 swap must
        // affect THIS subscriber.
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);

        {
            tracing::dispatcher::with_default(&dispatch, || {
                let span = tracing::info_span!("cron_job", job = "self_health_check");
                let _g = span.enter();
                tracing::info!(target: "zeroclaw_cron", "cron tick");
            });
        }

        // Phase 2: set the provider + activate the bridge (swap no-op → real).
        // Emit a span → the exporter captures it.
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_tracer_provider(provider.clone());
        activate_otel_bridge_with(&handle);

        {
            tracing::dispatcher::with_default(&dispatch, || {
                let span = tracing::info_span!("cron_job", job = "dashboard_updater");
                let _g = span.enter();
                tracing::info!(target: "zeroclaw_cron", "cron tick");
            });
        }

        let _ = provider.force_flush();
        let spans = exporter
            .get_finished_spans()
            .expect("spans after activation");
        assert!(
            spans.iter().any(|s| s.name == "cron_job"),
            "after activation the tracing span should be exported: {spans:?}"
        );
    }
}
