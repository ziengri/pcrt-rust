#![forbid(unsafe_code)]
//! Native result uploader composition root.

mod config;
mod runtime;

use pcrt_model::SessionId;
use pcrt_result_queue::{ResultQueue, Timestamp};

fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args().skip(1);
    let result = match arguments.next().as_deref() {
        Some("requeue-dead-letter") => requeue_dead_letter(arguments),
        Some(argument) => config::parse_args(std::iter::once(argument.to_owned()).chain(arguments))
            .and_then(|config| runtime::run(&config)),
        None => config::parse_args(std::iter::empty()).and_then(|config| runtime::run(&config)),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pcrt-uploader: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn requeue_dead_letter(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let session_id = arguments
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| usage().to_owned())?;
    let mut queue_path = "/var/lib/pcrt/sessions/outbox/results.sqlite".to_owned();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--result-queue-db" => {
                queue_path = arguments
                    .next()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "--result-queue-db requires a value".to_owned())?;
            }
            _ => return Err(usage().to_owned()),
        }
    }
    let session_id = SessionId::new(session_id).map_err(|error| error.to_string())?;
    ResultQueue::open(queue_path)
        .map_err(|error| error.to_string())?
        .requeue_dead_letter(&session_id, Timestamp::now())
        .map_err(|error| error.to_string())?;
    println!(
        "event=uploader_dead_letter_requeued session_id={}",
        session_id.as_str()
    );
    Ok(())
}

const fn usage() -> &'static str {
    "usage: pcrt-uploader [--config-env-file PATH] [--env-file PATH] [--result-queue-db PATH] [--exit-after-ms MS] | requeue-dead-letter SESSION_ID [--result-queue-db PATH]"
}
