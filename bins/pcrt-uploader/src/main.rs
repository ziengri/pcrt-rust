#![forbid(unsafe_code)]
//! Native result uploader composition root.

mod config;
mod runtime;

fn main() -> std::process::ExitCode {
    match config::parse_args(std::env::args().skip(1)).and_then(|config| runtime::run(&config)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pcrt-uploader: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
