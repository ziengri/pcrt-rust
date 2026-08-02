//! Synchronous composition root for concrete gateway adapters.

use std::{
    io::{self, Read},
    time::Instant,
};

use pcrt_door_zmq::{DoorPublisher, DoorsState};
use pcrt_service::ShutdownToken;

use crate::{
    config::GatewayConfig,
    door::{ByteSource, DoorProtocol, DoorService, GatewayEffect, GatewayHealth, open_source},
};

const READ_BUFFER_SIZE: usize = 4096;

/// Runs the gateway until shutdown, test-source completion or configured exit.
///
/// # Errors
///
/// Returns configuration-independent adapter setup or runtime failures.
pub(crate) fn run(config: &GatewayConfig) -> Result<(), String> {
    let protocol = DoorProtocol::new(config.door_count).map_err(|error| error.to_string())?;
    let shutdown = ShutdownToken::default();
    install_shutdown_handler(shutdown.clone())?;
    let publisher = DoorPublisher::bind(&config.endpoint).map_err(|error| error.to_string())?;
    log_event("publisher_bound", &[("endpoint", &config.endpoint)]);
    let started_at = Instant::now();
    let mut engine = DoorService::new(
        protocol,
        config.stale_timeout,
        config.serial_liveness_timeout,
        config.reconnect_delay,
        started_at,
    );
    let mut source = None;
    let mut buffer = [0_u8; READ_BUFFER_SIZE];

    let initial_state = engine.snapshot().clone();
    publish_state(&publisher, &initial_state, &mut engine);
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
            if matches!(output, GatewayEffect::DisconnectForLiveness) {
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

fn publish_state(publisher: &DoorPublisher, state: &DoorsState, engine: &mut DoorService) {
    if let Err(error) = publisher.publish(state) {
        engine.record_publish_failure();
        log_event("publish_failed", &[("error", &error.to_string())]);
    }
}

fn disconnect_source(
    source: &mut Option<ByteSource>,
    engine: &mut DoorService,
    now: Instant,
    reason: &str,
) {
    source.take();
    engine.source_disconnected(now);
    log_event("source_disconnected", &[("reason", reason)]);
}

fn handle_gateway_output(
    publisher: &DoorPublisher,
    output: GatewayEffect,
    engine: &mut DoorService,
    now: Instant,
) {
    match output {
        GatewayEffect::Publish(state) => publish_state(publisher, &state, engine),
        GatewayEffect::PacketRejected(error) => {
            log_event("packet_rejected", &[("error", &error.to_string())]);
        }
        GatewayEffect::PacketTruncated => log_event("packet_truncated", &[]),
        GatewayEffect::DecoderOverflow => log_event("decoder_overflow", &[]),
        GatewayEffect::DisconnectForLiveness => {
            log_event("source_disconnected", &[("reason", "liveness_timeout")]);
        }
        GatewayEffect::Heartbeat(state) => {
            publish_state(publisher, &state, engine);
            log_health(engine.health(), &state, now);
        }
    }
}

fn log_health(health: &GatewayHealth, state: &DoorsState, now: Instant) {
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
            ("stale", if state.stale() { "true" } else { "false" }),
            ("seq", &state.sequence().to_string()),
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

fn log_event(event: &str, fields: &[(&str, &str)]) {
    eprint!("event={event}");
    for (key, value) in fields {
        eprint!(" {key}={}", value.replace([' ', '\n', '\r'], "_"));
    }
    eprintln!();
}
