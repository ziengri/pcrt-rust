//! Blocking processor composition and operational loop.

use std::{
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use pcrt_door_zmq::{DoorSubscriber, DoorSubscription};
use pcrt_processing::{InferenceBackend, ProcessingStep, Processor, ResultEncoder};
use pcrt_result_queue::ResultQueue;
use pcrt_service::ShutdownToken;
use pcrt_storage::SessionStorage;

use crate::{
    config::ProcessorConfig,
    processor::{DoorGate, ProcessorLock},
};

/// Runs one processor instance with concrete private inference and encoding adapters.
///
/// Door state is read only before calling `process_one`. Since `process_one` owns a
/// claimed session until its terminal outcome, opening doors or stale telemetry never
/// cancels inference already in progress.
pub(crate) fn run<B, E>(config: &ProcessorConfig, backend: B, encoder: E) -> Result<(), String>
where
    B: InferenceBackend,
    E: ResultEncoder,
{
    let shutdown = ShutdownToken::default();
    install_shutdown_handler(shutdown.clone())?;
    let _lock = ProcessorLock::acquire(&config.sessions_dir).map_err(|error| error.to_string())?;
    let storage = SessionStorage::open(&config.sessions_dir).map_err(|error| error.to_string())?;
    let queue = ResultQueue::open(&config.queue_path).map_err(|error| error.to_string())?;
    let mut processor = Processor::new(storage, queue, backend, encoder, shutdown.clone());
    let recovery = processor
        .recover(now_ms())
        .map_err(|error| error.to_string())?;
    log_event(
        "processor_recovered",
        &[
            (
                "completed_prepared_results",
                &recovery.completed_prepared_results.to_string(),
            ),
            ("released_claims", &recovery.released_claims.to_string()),
            ("failed_sessions", &recovery.failed_sessions.to_string()),
        ],
    );
    let mut doors = DoorSubscriber::connect(&config.endpoint, DoorSubscription::Aggregate)
        .map_err(|error| error.to_string())?;
    let gate = DoorGate::new(config.door_state_ttl);
    let started_at = Instant::now();
    log_event("processor_started", &[("endpoint", &config.endpoint)]);

    while !shutdown.is_shutdown_requested() {
        if config
            .exit_after
            .is_some_and(|duration| started_at.elapsed() >= duration)
        {
            log_event("processor_exit_after", &[]);
            break;
        }
        let processing_allowed = match doors.drain() {
            Ok(()) => {
                gate.processing_allowed(doors.latest(), doors.latest_received_at(), Instant::now())
            }
            Err(error) => {
                log_event("door_subscriber_failed", &[("error", &error.to_string())]);
                false
            }
        };
        match processor
            .process_one(processing_allowed, now_ms())
            .map_err(|error| error.to_string())?
        {
            ProcessingStep::Paused | ProcessingStep::Idle => thread::sleep(config.idle_sleep),
            ProcessingStep::ShutdownRequested => break,
            ProcessingStep::Completed(session_id) => {
                log_event(
                    "processor_completed",
                    &[("session_id", session_id.as_str())],
                );
            }
            ProcessingStep::Reconciled(session_id) => {
                log_event(
                    "processor_reconciled",
                    &[("session_id", session_id.as_str())],
                );
            }
            ProcessingStep::Failed(session_id) => {
                log_event("processor_failed", &[("session_id", session_id.as_str())]);
            }
        }
    }
    log_event("processor_stopped", &[]);
    Ok(())
}

fn install_shutdown_handler(token: ShutdownToken) -> Result<(), String> {
    ctrlc::set_handler(move || token.request_shutdown())
        .map_err(|error| format!("install shutdown handler: {error}"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn log_event(event: &str, values: &[(&str, &str)]) {
    let mut line = format!("event={event}");
    for (key, value) in values {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    eprintln!("{line}");
}
