//! Processor composition root.

mod config;
// The runtime is ready for injection by the native backend slice. It must not be
// started with a placeholder backend because that could fail real sessions.
#[allow(
    dead_code,
    reason = "native inference backend is intentionally not enabled yet"
)]
mod processor;
#[allow(
    dead_code,
    reason = "native inference backend is intentionally not enabled yet"
)]
mod runtime;

fn main() -> std::process::ExitCode {
    match config::parse_args(std::env::args().skip(1)) {
        Ok(_) => {
            eprintln!(
                "pcrt-processor native inference backend is not enabled; deploy the existing processor until fixture and shadow validation are complete"
            );
            std::process::ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("pcrt-processor: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
