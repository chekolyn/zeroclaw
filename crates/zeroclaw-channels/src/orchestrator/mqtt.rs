//! MQTT → SOP event fan-in listener.
//! This is NOT a `Channel` trait implementor — it routes MQTT messages
//! to the SOP engine via `dispatch_untrusted_fan_in`, not to the chat loop.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};

use zeroclaw_config::schema::MqttConfig;
use zeroclaw_runtime::sop::audit::SopAuditLogger;
use zeroclaw_runtime::sop::dispatch::SopIngress;
use zeroclaw_runtime::sop::engine::SopEngine;
use zeroclaw_runtime::sop::types::SopTriggerSource;

/// Run the MQTT SOP listener loop.
/// Subscribes to configured topics and dispatches incoming publishes
/// to the SOP engine. Blocks until disconnected or cancelled.
pub async fn run_mqtt_sop_listener(
    config: &MqttConfig,
    engine: Arc<Mutex<SopEngine>>,
    audit: Arc<SopAuditLogger>,
    driver_sink: Option<zeroclaw_runtime::sop::SopDriverSink>,
) -> Result<()> {
    config.validate()?;

    let mut mqtt_options = MqttOptions::new(
        &config.client_id,
        broker_host(&config.broker_url),
        broker_port(&config.broker_url),
    );
    mqtt_options.set_keep_alive(std::time::Duration::from_secs(config.keep_alive_secs));

    if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        mqtt_options.set_credentials(user, pass);
    }

    // Configure TLS transport when mqtts:// scheme is used
    if config.use_tls {
        mqtt_options.set_transport(Transport::tls_with_default_config());
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "MQTT SOP listener: TLS transport enabled"
        );
    }

    let (client, mut eventloop) = AsyncClient::new(mqtt_options, 64);

    let qos = match config.qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::ExactlyOnce,
    };

    // Subscribe to all configured topics
    for topic in &config.topics {
        client.subscribe(topic, qos).await?;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"topic": topic})),
            "MQTT SOP listener: subscribed to ''"
        );
    }

    zeroclaw_runtime::health::mark_component_ok("mqtt");

    loop {
        match eventloop.poll().await {
            Ok(event) => {
                // Every completed poll proves the loop is alive — stamp the
                // health component (see record_poll_alive for the defect-#2
                // rationale). This includes the idle case: the broker's
                // keepalive PingResp polls every keep_alive_secs.
                record_poll_alive();
                match event {
                    Event::Incoming(Packet::Publish(msg)) => {
                let payload_raw = String::from_utf8_lossy(&msg.payload);
                let mut ingress = SopIngress::new(Some(&engine), Some(audit.as_ref()));
                if let Some(sink) = driver_sink.as_ref() {
                    ingress = ingress.with_driver_sink(sink);
                }
                ingress
                    .dispatch(
                        SopTriggerSource::Mqtt,
                        Some(&msg.topic),
                        Some(&payload_raw),
                        None,
                        None,
                    )
                    .await;
            }
                    Event::Incoming(Packet::ConnAck(_)) => {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                            "MQTT SOP listener: connected to broker"
                        );
                    }
                    // Other events (PingResp, SubAck, outgoing) — alive,
                    // already stamped by the per-poll record above.
                    _ => {}
                }
            }
            Err(e) => {
                zeroclaw_runtime::health::mark_component_error("mqtt", e.to_string());
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "MQTT SOP listener: connection error"
                );
                // rumqttc handles auto-reconnect; loop continues
            }
        }
    }
}

/// Extract host from broker URL like "mqtt://host:port"
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

/// Extract port from broker URL, defaulting to 1883 for mqtt:// and 8883 for mqtts://.
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
    fn mqtt_config_validation_rejects_bad_qos() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "mqtt://localhost:1883".into(),
            client_id: "zeroclaw".into(),
            topics: vec!["test".into()],
            qos: 3,
            username: None,
            password: None,
            use_tls: false,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("qos must be 0, 1, or 2"));
    }

    #[test]
    fn mqtt_config_validation_rejects_bad_url() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "http://localhost:1883".into(),
            client_id: "zeroclaw".into(),
            topics: vec!["test".into()],
            qos: 1,
            username: None,
            password: None,
            use_tls: false,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("mqtt://"));
    }

    #[test]
    fn mqtt_config_validation_rejects_empty_topics() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "mqtt://localhost:1883".into(),
            client_id: "zeroclaw".into(),
            topics: vec![],
            qos: 1,
            username: None,
            password: None,
            use_tls: false,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("at least one topic"));
    }

    #[test]
    fn mqtt_config_validation_rejects_empty_client_id() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "mqtt://localhost:1883".into(),
            client_id: String::new(),
            topics: vec!["test".into()],
            qos: 1,
            username: None,
            password: None,
            use_tls: false,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("client_id must not be empty"));
    }

    #[test]
    fn mqtt_config_validation_accepts_valid() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "mqtt://localhost:1883".into(),
            client_id: "zeroclaw".into(),
            topics: vec!["sensors/#".into()],
            qos: 1,
            username: None,
            password: None,
            use_tls: false,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn mqtt_tls_flag_rejects_mqtt_scheme_with_use_tls() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "mqtt://localhost:1883".into(),
            client_id: "zeroclaw".into(),
            topics: vec!["test".into()],
            qos: 1,
            username: None,
            password: None,
            use_tls: true,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("use_tls is true"));
    }

    #[test]
    fn mqtt_tls_flag_rejects_mqtts_scheme_without_use_tls() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "mqtts://localhost:8883".into(),
            client_id: "zeroclaw".into(),
            topics: vec!["test".into()],
            qos: 1,
            username: None,
            password: None,
            use_tls: false,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("mqtts://"));
    }

    #[test]
    fn mqtt_tls_flag_accepts_mqtts_with_use_tls() {
        let config = MqttConfig {
            enabled: true,
            broker_url: "mqtts://localhost:8883".into(),
            client_id: "zeroclaw".into(),
            topics: vec!["test".into()],
            qos: 1,
            username: None,
            password: None,
            use_tls: true,
            keep_alive_secs: 30,
            excluded_tools: vec![],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn broker_host_extracts_host() {
        assert_eq!(broker_host("mqtt://myhost:1883"), "myhost");
        assert_eq!(
            broker_host("mqtts://secure.example.com:8883"),
            "secure.example.com"
        );
    }

    #[test]
    fn broker_port_extracts_port() {
        assert_eq!(broker_port("mqtt://localhost:1883"), 1883);
        assert_eq!(broker_port("mqtts://host:8883"), 8883);
    }

    #[test]
    fn broker_port_defaults_1883_for_mqtt() {
        assert_eq!(broker_port("mqtt://localhost"), 1883);
    }

    #[test]
    fn broker_port_defaults_8883_for_mqtts() {
        assert_eq!(broker_port("mqtts://secure.example.com"), 8883);
    }
}


/// Records that the MQTT event loop completed a poll successfully — the
/// steady-state proof of channel life. Called for EVERY Ok poll outcome:
/// PingResp arrives every `keep_alive_secs` (30s) even when the wire is
/// idle, Publishes and outgoing packets likewise prove the loop is running.
///
/// Defect #2 (the 2026-09-27 flap root cause): the loop previously stamped
/// the health component ONLY on ConnAck — a healthy persistent connection
/// receives ConnAck exactly once at boot, so `mqtt.last_ok` froze at boot
/// and the staleness liveness probe (the 2026-09-23 wire-death interim,
/// 900s threshold) killed every healthy gateway instance at ~18 min uptime
/// (12 kills/4h observed on prod, 14 on canary, `restart_count: 0`
/// throughout). Stamping per poll makes `last_ok` a true liveness-of-loop
/// metric: a healthy-but-quiet wire refreshes every keepalive; a genuinely
/// hung loop (the original 2026-09-23 wire death) goes stale and the probe
/// fires — the interim mitigation restored to its intended semantics.
fn record_poll_alive() {
    zeroclaw_runtime::health::mark_component_ok("mqtt");
}

#[cfg(test)]
mod loop_liveness_stamp_tests {
    use super::*;

    /// Defect #2 (the flap, root-caused 2026-09-27): the mqtt event loop
    /// stamped `last_ok` ONLY on ConnAck — a healthy persistent connection
    /// receives ConnAck exactly once at boot, so `last_ok` froze at boot and
    /// the staleness liveness probe (the 2026-09-23 wire-death interim,
    /// threshold 900s) killed EVERY healthy gateway instance at ~18 min
    /// uptime (12 kills/4h prod, 14 canary; `restart_count: 0` throughout —
    /// the client never died; the probe's commit premise "last_ok updates on
    /// message consumption" was false). The fix: every successful
    /// `eventloop.poll()` stamps `last_ok` — PingResp arrives every
    /// `keep_alive_secs` (30s) even when idle, Publishes and outgoing
    /// packets likewise prove the loop is alive, so a healthy-but-quiet wire
    /// never trips the probe and a genuinely hung loop (the original
    /// 2026-09-23 wire-death) goes stale and is killed — the interim
    /// mitigation restored to its intended semantics.
    #[test]
    fn successful_poll_stamps_mqtt_last_ok() {
        zeroclaw_runtime::health::mark_component_starting("mqtt");
        // A completed poll (any outcome the loop returns Ok for — e.g. the
        // 30-second keepalive PingResp) must stamp the component healthy.
        record_poll_alive();
        let snap = zeroclaw_runtime::health::snapshot_json();
        let m = &snap["components"]["mqtt"];
        assert!(
            m["last_ok"].is_string(),
            "a successful poll must stamp mqtt.last_ok (the staleness probe keys on its age); got {m}"
        );
        assert_eq!(m["status"], "ok", "a successful poll must mark the component ok");
    }

    #[test]
    fn starting_state_has_no_last_ok_to_stale() {
        zeroclaw_runtime::health::mark_component_starting("mqtt");
        let snap = zeroclaw_runtime::health::snapshot_json();
        let m = &snap["components"]["mqtt"];
        assert!(
            m["last_ok"].is_null(),
            "the starting state must clear last_ok so a fresh incarnation cannot inherit stale health; got {m}"
        );
    }
}
