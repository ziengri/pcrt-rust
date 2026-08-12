//! Processor composition root.

mod config;
mod inference;
mod processor;
mod result_encoder;
mod runtime;

fn main() -> std::process::ExitCode {
    match config::parse_args(std::env::args().skip(1)) {
        Ok(config) => run(&config),
        Err(error) => {
            eprintln!("pcrt-processor: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(config: &config::ProcessorConfig) -> std::process::ExitCode {
    #[cfg(feature = "license")]
    if let Err(error) = pcrt_license::validate_installed(&config.bus_id) {
        eprintln!("pcrt-processor: license denied: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let backend = match inference::backend::NativeInferenceBackend::new(config) {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("pcrt-processor: initialize native inference: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let encoder = result_encoder::TimelineResultEncoder::new(config.bus_id.clone());
    match runtime::run(config, backend, encoder) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pcrt-processor: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
