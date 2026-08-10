#![forbid(unsafe_code)]
//! RS-232 door gateway composition entrypoint.

mod application;
mod config;
mod infrastructure;
mod runtime;

use std::{env, process};

fn main() {
    let result = config::parse_args(env::args().skip(1)).and_then(|config| runtime::run(&config));
    if let Err(error) = result {
        eprintln!("event=gateway_fatal error={error}");
        process::exit(1);
    }
}
