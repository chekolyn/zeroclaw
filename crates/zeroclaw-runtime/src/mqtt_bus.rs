//! Outbound MQTT publish helper for the event-driven swarm engine.
//!
//! This module provides a global publisher client that lets SOPs and
//! delegate hooks publish events to the MQTT bus. When no MQTT channel is
//! configured/enabled, [`publish`] is a graceful no-op (returns `Ok`).
//!
//! See: docs/superpowers/specs/2026-07-08-event-driven-swarm-engine-design.md

use std::sync::LazyLock;

use anyhow::Result;
use rumqttc::{AsyncClient, MqttOptions, QoS, Transport};
use tokio::sync::Mutex;
use zeroclaw_config::schema::Config;

/// Global publisher client. `None` until [`init`] is called with an enabled
/// MQTT channel. Once set, the client is cloned for each publish (rumqttc
/// `AsyncClient` is `Clone`).
static PUBLISHER: LazyLock<Mutex<Option<AsyncClient>>> = LazyLock::new(|| Mutex::new(None));

/// Initialize the global publisher from the first enabled `[channels.mqtt.*]`.
///
/// Call once at daemon startup (alongside `run_mqtt_sop_listener`). Safe to
/// call multiple times — the first successful call wins; subsequent calls
/// are no-ops if a client is already set. No-op (returns `Ok`) if no MQTT
/// channel is enabled.
pub async fn init(config: &Config) -> Result<()> {
    let mut guard = PUBLISHER.lock().await;
    if guard.is_some() {
        // Already initialized; first call wins.
        return Ok(());
    }

    // Find the first enabled MQTT channel.
    let mqtt_cfg = config
        .channels
        .mqtt
        .values()
        .find(|c| c.enabled)
        .cloned();

    let Some(cfg) = mqtt_cfg else {
        // No enabled MQTT channel — publisher stays None; publish() will no-op.
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "mqtt_bus: init no-op (no enabled MQTT channel)"
        );
        return Ok(());
    };

    cfg.validate()?;

    let mut mqtt_options = MqttOptions::new(
        format!("{}-pub", cfg.client_id),
        broker_host(&cfg.broker_url),
        broker_port(&cfg.broker_url),
    );
    mqtt_options.set_keep_alive(std::time::Duration::from_secs(cfg.keep_alive_secs));

    if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
        mqtt_options.set_credentials(user, pass);
    }

    if cfg.use_tls {
        mqtt_options.set_transport(Transport::tls_with_default_config());
    }

    let (client, mut eventloop) = AsyncClient::new(mqtt_options, 64);

    // Spawn the eventloop task so the client can process outgoing publishes.
    // We don't need to handle incoming events (the SOP listener does that);
    // we just need the eventloop alive for the publish path.
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(_) => { /* event processed */ }
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(
                            module_path!(),
                            ::zeroclaw_log::Action::Note
                        ),
                        format!("mqtt_bus: eventloop poll error: {e}")
                    );
                    // rumqttc auto-reconnects; brief backoff to avoid spin.
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });

    *guard = Some(client);

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
        format!("mqtt_bus: publisher initialized for broker {}", cfg.broker_url)
    );

    Ok(())
}

/// Publish a message to the bus at QoS 1 (at-least-once).
///
/// When no MQTT channel is configured (i.e. [`init`] was never called or no
/// enabled channel was found), this is a graceful no-op: it logs at INFO and
/// returns `Ok(())`. Never panics, never returns `Err` for "not configured".
pub async fn publish(topic: &str, payload: Vec<u8>, retain: bool) -> Result<()> {
    let guard = PUBLISHER.lock().await;
    let Some(client) = guard.as_ref() else {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            format!("mqtt_bus: publish no-op (mqtt not configured) topic={topic}")
        );
        return Ok(());
    };

    // AsyncClient is Clone — clone out of the mutex to avoid holding the lock
    // across the await.
    let client = client.clone();
    drop(guard);

    client
        .publish(topic, QoS::AtLeastOnce, retain, payload)
        .await?;

    Ok(())
}

/// Build a topic string from parts, joining `zeroclaw/` + parts with `/`.
///
/// Each segment is sanitized: lowercased, and any char illegal in an MQTT
/// topic segment (`/`, `+`, `#`, space, tab, newline, CR, and other ASCII
/// control chars) is replaced with `_`.
///
/// # Examples
/// ```
/// // ["projects","Foo Bar/Baz"] -> "zeroclaw/projects/foo_bar_baz"
/// ```
pub fn topic_for(parts: &[&str]) -> String {
    let sanitized: Vec<String> = parts
        .iter()
        .map(|p| sanitize_segment(p))
        .collect();
    let joined = sanitized.join("/");
    format!("zeroclaw/{joined}")
}

/// Sanitize a single topic segment: lowercase + replace illegal chars with `_`.
fn sanitize_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if is_illegal_topic_char(c) {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

/// Returns true for chars that are illegal or problematic in an MQTT topic
/// segment: `/` (level separator), `+` (single-level wildcard), `#`
/// (multi-level wildcard), space, tab, newline, CR, and ASCII control chars
/// (0x00–0x1F, 0x7F).
fn is_illegal_topic_char(c: char) -> bool {
    if c == '/' || c == '+' || c == '#' || c == ' ' || c == '\t' || c == '\n' || c == '\r' {
        return true;
    }
    // ASCII control chars
    if c.is_ascii_control() {
        return true;
    }
    false
}

/// Extract the host from a broker URL (`mqtt://host:port` or `mqtts://host:port`).
/// Self-contained parse (does not depend on zeroclaw-channels internals).
fn broker_host(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("mqtt://")
        .or_else(|| url.strip_prefix("mqtts://"))
        .unwrap_or(url);
    without_scheme
        .split(':')
        .next()
        .unwrap_or("localhost")
        .to_string()
}

/// Extract the port from a broker URL, defaulting to 1883 for `mqtt://` and
/// 8883 for `mqtts://`. Self-contained parse.
fn broker_port(url: &str) -> u16 {
    let is_tls = url.starts_with("mqtts://");
    let without_scheme = url
        .strip_prefix("mqtt://")
        .or_else(|| url.strip_prefix("mqtts://"))
        .unwrap_or(url);
    let default_port: u16 = if is_tls { 8883 } else { 1883 };
    without_scheme
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default_port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_for_sanitizes_segments() {
        // Basic: lowercase + illegal chars replaced
        assert_eq!(
            topic_for(&["projects", "Foo Bar/Baz"]),
            "zeroclaw/projects/foo_bar_baz"
        );
        // Empty parts -> just the prefix
        assert_eq!(topic_for(&[]), "zeroclaw/");
        // Wildcards replaced
        assert_eq!(
            topic_for(&["swarm", "test+topic", "deep#path"]),
            "zeroclaw/swarm/test_topic/deep_path"
        );
        // Multiple segments joined
        assert_eq!(
            topic_for(&["projects", "debops", "milestones", "m1", "tasks", "t1"]),
            "zeroclaw/projects/debops/milestones/m1/tasks/t1"
        );
        // Control chars replaced
        assert_eq!(
            topic_for(&["test", "seg\tment"]),
            "zeroclaw/test/seg_ment"
        );
    }

    #[tokio::test]
    async fn publish_noop_when_mqtt_unconfigured() {
        // With no init() call, the global PUBLISHER is None.
        // publish() should return Ok(()) and not panic.
        let result = publish("test/topic", b"payload".to_vec(), false).await;
        assert!(result.is_ok(), "publish() should return Ok when unconfigured");
    }
}
