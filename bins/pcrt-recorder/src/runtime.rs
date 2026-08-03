//! Blocking recorder composition and operational loop.

use std::{
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use pcrt_door_zmq::AggregateDoorSubscriber;
use pcrt_recording::{
    lifecycle::RecordingLimits,
    recorder::{Recorder, RecorderConfig},
};
use pcrt_service::ShutdownToken;
use pcrt_storage::SessionStorage;

use crate::{
    config::RecorderConfig as RuntimeConfig,
    recorder::{
        DoorGate, FfmpegEncoderFactory, OpenCvVideoSource, RecordingService, RecordingServiceStep,
    },
};

pub(crate) fn run(config: &RuntimeConfig) -> Result<(), String> {
    let shutdown = ShutdownToken::default();
    install_shutdown_handler(shutdown.clone())?;
    let storage = SessionStorage::open(&config.sessions_dir).map_err(|error| error.to_string())?;
    let recovery = storage
        .recover_recording(&config.camera_id, now_ms())
        .map_err(|error| error.to_string())?;
    log_event(
        "storage_recovered",
        &[
            ("failed_sessions", &recovery.failed_sessions.to_string()),
            ("promoted_captures", &recovery.promoted_captures.to_string()),
        ],
    );
    let recorder = Recorder::new(
        storage,
        FfmpegEncoderFactory::new(shutdown.clone()),
        RecorderConfig {
            camera_id: config.camera_id.clone(),
            source_id: config.source.clone(),
            width: config.width,
            height: config.height,
            frames_per_second: config.frames_per_second,
        },
        RecordingLimits::new(
            config.frames_per_second,
            u64::from(config.frames_per_second)
                .checked_mul(config.max_session_seconds)
                .ok_or_else(|| "MAX_SESSION_SECONDS * FPS overflows frame limit".to_owned())?,
        )
        .map_err(|error| error.to_string())?,
    );
    let source = OpenCvVideoSource::open(&config.source).map_err(|error| error.to_string())?;
    let mut service = RecordingService::new(source, recorder, config.width, config.height)
        .map_err(|error| error.to_string())?;
    let mut doors =
        AggregateDoorSubscriber::connect(&config.endpoint).map_err(|error| error.to_string())?;
    let gate = DoorGate::new(config.door_id, config.door_state_ttl);
    let started_at = Instant::now();
    log_event(
        "recorder_started",
        &[
            ("camera_id", &config.camera_id),
            ("door_channel", &config.door_id.get().to_string()),
            ("endpoint", &config.endpoint),
        ],
    );
    while !shutdown.is_shutdown_requested() {
        if config
            .exit_after
            .is_some_and(|duration| started_at.elapsed() >= duration)
        {
            log_event("recorder_exit_after", &[]);
            break;
        }
        let door_open = match doors.drain() {
            Ok(()) => gate.is_open(doors.latest(), Instant::now()),
            Err(error) => {
                log_event("door_subscriber_failed", &[("error", &error.to_string())]);
                false
            }
        };
        let step = match service.step(door_open, now_ms()) {
            Ok(step) => step,
            Err(error) if shutdown.is_shutdown_requested() => {
                log_event(
                    "recorder_frame_write_interrupted",
                    &[("error", &error.to_string())],
                );
                break;
            }
            Err(error) => return Err(error.to_string()),
        };
        match step {
            RecordingServiceStep::FileRestarted => log_event("source_file_restarted", &[]),
            RecordingServiceStep::NoFrame => thread::sleep(config.idle_sleep),
            RecordingServiceStep::FrameHandled | RecordingServiceStep::FrameDiscardedDoorClosed => {
            }
        }
    }
    service
        .shutdown(now_ms())
        .map_err(|error| error.to_string())?;
    log_event("recorder_stopped", &[]);
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
