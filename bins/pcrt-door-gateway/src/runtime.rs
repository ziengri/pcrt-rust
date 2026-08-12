//! Synchronous composition root for concrete gateway adapters.

use std::{
    io::{self, Read},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use pcrt_door::{DoorProtocol, DoorSnapshot, encode_snapshot};
use pcrt_door_zmq::DoorPublisher;
#[cfg(feature = "license")]
use pcrt_license::validate_installed;
use pcrt_service::ShutdownToken;

use crate::{
    application::{GatewayEngine, GatewayHealth, GatewayOutput},
    config::GatewayConfig,
    infrastructure::serial::{ByteSource, open_source},
};

const READ_BUFFER_SIZE: usize = 4096;

/// Runs the gateway until shutdown, test-source completion or configured exit.
///
/// # Errors
///
/// Returns configuration-independent adapter setup or runtime failures.
pub(crate) fn run(config: &GatewayConfig) -> Result<(), String> {
    #[cfg(feature = "license")]
    validate_license(&config.bus_id)?;
    let protocol = DoorProtocol::new(config.door_count).map_err(|error| error.to_string())?;
    let shutdown = ShutdownToken::default();
    install_shutdown_handler(shutdown.clone())?;
    let publisher = DoorPublisher::bind(&config.endpoint).map_err(|error| error.to_string())?;
    log_event("publisher_bound", &[("endpoint", &config.endpoint)]);
    let started_at = Instant::now();
    let mut engine = GatewayEngine::new(
        protocol,
        config.stale_timeout,
        config.serial_liveness_timeout,
        config.reconnect_delay,
        started_at,
    );
    let mut source = None;
    let mut buffer = [0_u8; READ_BUFFER_SIZE];

    let initial_snapshot = engine.snapshot().clone();
    publish_snapshot(&publisher, &initial_snapshot, &mut engine);
    while !shutdown.is_shutdown_requested() {
        let now = Instant::now();
        if config
            .exit_after
            .is_some_and(|duration| now.duration_since(started_at) >= duration)
        {
            log_event("gateway_exit_after", &[]);
            break;
        }
        if source.is_none() && engine.reconnect_due(now) {
            #[cfg(feature = "license")]
            if let Err(error) = validate_license(&config.bus_id) {
                log_event("license_denied", &[("reason", &error)]);
                return Err(error);
            }
            engine.begin_connect_attempt();
            match open_source(config) {
                Ok(opened) => {
                    engine.source_connected(now);
                    log_event("source_connected", &[]);
                    source = Some(opened);
                }
                Err(error) => {
                    engine.source_connect_failed(now);
                    log_event("source_connect_failed", &[("error", &error)]);
                }
            }
        }
        if let Some(reader) = source.as_mut() {
            #[cfg(feature = "license")]
            if let Err(error) = validate_license(&config.bus_id) {
                log_event("license_denied", &[("reason", &error)]);
                source.take();
                return Err(error);
            }
            match reader.read(&mut buffer) {
                Ok(0) if reader.is_test_transport() => {
                    log_event("test_source_complete", &[]);
                    break;
                }
                Ok(0) => disconnect_source(&mut source, &mut engine, now, "eof"),
                Ok(count) => {
                    let received_at = Instant::now();
                    for output in engine.on_bytes(&buffer[..count], received_at) {
                        handle_gateway_output(&publisher, output, &mut engine, received_at);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => disconnect_source(&mut source, &mut engine, now, &error.to_string()),
            }
        }
        let tick_at = Instant::now();
        for output in engine.tick(tick_at, config.heartbeat_interval) {
            if matches!(output, GatewayOutput::DisconnectForLiveness) {
                source.take();
            }
            handle_gateway_output(&publisher, output, &mut engine, tick_at);
        }
    }
    log_event("gateway_stopping", &[]);
    drop(source);
    if let Err(error) = publisher.close() {
        log_event("ipc_cleanup_failed", &[("error", &error.to_string())]);
    }
    Ok(())
}

#[cfg(feature = "license")]
fn validate_license(bus_id: &str) -> Result<(), String> {
    validate_installed(bus_id)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn publish_snapshot(
    publisher: &DoorPublisher,
    snapshot: &DoorSnapshot,
    engine: &mut GatewayEngine,
) {
    match encode_snapshot(snapshot, epoch_seconds()) {
        Ok(messages) => {
            for message in &messages {
                if let Err(error) = publisher.publish(message) {
                    engine.record_publish_failure();
                    log_event("publish_failed", &[("error", &error.to_string())]);
                }
            }
        }
        Err(error) => {
            engine.record_publish_failure();
            log_event("encode_failed", &[("error", &error.to_string())]);
        }
    }
}

fn disconnect_source(
    source: &mut Option<ByteSource>,
    engine: &mut GatewayEngine,
    now: Instant,
    reason: &str,
) {
    source.take();
    engine.source_disconnected(now);
    log_event("source_disconnected", &[("reason", reason)]);
}

fn handle_gateway_output(
    publisher: &DoorPublisher,
    output: GatewayOutput,
    engine: &mut GatewayEngine,
    now: Instant,
) {
    match output {
        GatewayOutput::Publish(snapshot) => publish_snapshot(publisher, &snapshot, engine),
        GatewayOutput::PacketRejected(error) => {
            log_event("packet_rejected", &[("error", &error.to_string())]);
        }
        GatewayOutput::PacketTruncated => log_event("packet_truncated", &[]),
        GatewayOutput::DecoderOverflow => log_event("decoder_overflow", &[]),
        GatewayOutput::DisconnectForLiveness => {
            log_event("source_disconnected", &[("reason", "liveness_timeout")]);
        }
        GatewayOutput::Heartbeat(snapshot) => {
            publish_snapshot(publisher, &snapshot, engine);
            log_health(engine.health(), &snapshot, now);
        }
    }
}

fn log_health(health: &GatewayHealth, snapshot: &DoorSnapshot, now: Instant) {
    let packet_age_ms = health.last_valid_packet.map_or_else(
        || "unknown".to_owned(),
        |instant| now.duration_since(instant).as_millis().to_string(),
    );
    log_event(
        "health",
        &[
            (
                "serial_connected",
                if health.connected { "true" } else { "false" },
            ),
            ("stale", if snapshot.is_stale() { "true" } else { "false" }),
            ("seq", &snapshot.sequence().to_string()),
            ("last_valid_age_ms", &packet_age_ms),
            ("valid_packets", &health.valid_packets.to_string()),
            ("rejected_packets", &health.rejected_packets.to_string()),
            ("truncated_packets", &health.truncated_packets.to_string()),
            ("overflow_events", &health.overflow_events.to_string()),
            ("reconnect_attempts", &health.reconnect_attempts.to_string()),
            ("publish_failures", &health.publish_failures.to_string()),
        ],
    );
}

fn install_shutdown_handler(token: ShutdownToken) -> Result<(), String> {
    ctrlc::set_handler(move || token.request_shutdown())
        .map_err(|error| format!("install shutdown handler: {error}"))
}

fn epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn log_event(event: &str, fields: &[(&str, &str)]) {
    eprint!("event={event}");
    for (key, value) in fields {
        eprint!(" {key}={}", value.replace([' ', '\n', '\r'], "_"));
    }
    eprintln!();
}
