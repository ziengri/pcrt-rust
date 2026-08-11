//! Blocking uploader composition and operational loop.

use std::{thread, time::Instant};

use pcrt_api_client::{ApiClientConfig, TimelineApiClient};
use pcrt_result_queue::{ResultQueue, Timestamp};
use pcrt_service::ShutdownToken;
use pcrt_uploader_core::{UploadStep, Uploader, UploaderConfig};

use crate::config::UploaderProcessConfig;

pub(super) fn run(config: &UploaderProcessConfig) -> Result<(), String> {
    let shutdown = ShutdownToken::default();
    install_shutdown_handler(shutdown.clone())?;
    let queue = ResultQueue::open(&config.queue_path).map_err(|error| error.to_string())?;
    let api_config = ApiClientConfig::new(
        &config.api_base_url,
        config.api_x_auth.clone(),
        config.api_timeout,
    )
    .map_err(|error| format!("configure timeline API client: {error}"))?;
    let delivery = TimelineApiClient::new(&api_config)
        .map_err(|error| format!("create timeline API client: {error}"))?;
    let mut uploader = Uploader::new(
        queue,
        delivery,
        UploaderConfig {
            poll_interval: config.poll_interval,
            initial_backoff: config.initial_backoff,
            max_backoff: config.max_backoff,
        },
    )
    .map_err(|error| error.to_string())?;
    let started_at = Instant::now();
    log_event(
        "uploader_started",
        &[("queue_path", &config.queue_path.display().to_string())],
    );

    while !shutdown.is_shutdown_requested() {
        if config
            .exit_after
            .is_some_and(|duration| started_at.elapsed() >= duration)
        {
            log_event("uploader_exit_after", &[]);
            break;
        }
        match uploader
            .process_next(Timestamp::now())
            .map_err(|error| error.to_string())?
        {
            UploadStep::Idle => thread::sleep(config.poll_interval),
            UploadStep::Delivered { session_id } => {
                log_event("uploader_delivered", &[("session_id", &session_id)]);
            }
            UploadStep::Rescheduled {
                session_id,
                attempts,
                retry_at,
            } => {
                log_event(
                    "uploader_rescheduled",
                    &[
                        ("session_id", &session_id),
                        ("attempts", &attempts.to_string()),
                        ("retry_at_ms", &retry_at.as_unix_millis().to_string()),
                    ],
                );
            }
            UploadStep::DeadLettered {
                session_id,
                attempts,
            } => {
                log_event(
                    "uploader_dead_lettered",
                    &[
                        ("session_id", &session_id),
                        ("attempts", &attempts.to_string()),
                    ],
                );
            }
        }
    }
    log_event("uploader_stopped", &[]);
    Ok(())
}

fn install_shutdown_handler(token: ShutdownToken) -> Result<(), String> {
    ctrlc::set_handler(move || token.request_shutdown())
        .map_err(|error| format!("install shutdown handler: {error}"))
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
