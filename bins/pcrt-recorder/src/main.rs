#![forbid(unsafe_code)]
//! One-camera door-gated recorder service.

mod config;
mod recorder;
mod runtime;

use std::{env, process};

fn main() {
    let result = config::parse_args(env::args().skip(1)).and_then(|config| runtime::run(&config));
    if let Err(error) = result {
        eprintln!("event=recorder_fatal error={error}");
        process::exit(1);
    }
}
