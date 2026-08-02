//! Serial-port and feature-gated local byte source.

use std::io::{self, Read};

#[cfg(feature = "test-transport")]
use std::os::unix::net::UnixStream;

use crate::config::GatewayConfig;

pub(crate) enum ByteSource {
    Serial(Box<dyn serialport::SerialPort>),
    #[cfg(feature = "test-transport")]
    TestUnix(UnixStream),
}

impl ByteSource {
    pub(crate) const fn is_test_transport(&self) -> bool {
        match self {
            Self::Serial(_) => false,
            #[cfg(feature = "test-transport")]
            Self::TestUnix(_) => true,
        }
    }
}

impl Read for ByteSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Serial(port) => port.read(buffer),
            #[cfg(feature = "test-transport")]
            Self::TestUnix(stream) => stream.read(buffer),
        }
    }
}

pub(crate) fn open_source(config: &GatewayConfig) -> Result<ByteSource, String> {
    #[cfg(feature = "test-transport")]
    if let Some(path) = &config.test_source_path {
        let stream = UnixStream::connect(path)
            .map_err(|error| format!("connect test byte source {path}: {error}"))?;
        stream
            .set_read_timeout(Some(config.serial_read_timeout))
            .map_err(|error| format!("set test byte source timeout: {error}"))?;
        return Ok(ByteSource::TestUnix(stream));
    }
    if let Some(path) = &config.serial_port {
        if let Ok(source) = open_serial(path, config) {
            return Ok(source);
        }
    }
    let Some(pattern) = &config.serial_port_find else {
        return Err(
            "serial port is unavailable and no serial discovery pattern is configured".to_owned(),
        );
    };
    let paths = glob::glob(pattern)
        .map_err(|error| format!("invalid serial discovery pattern: {error}"))?;
    let mut failures = Vec::new();
    for path in paths.flatten() {
        let path = path.to_string_lossy().into_owned();
        match open_serial(&path, config) {
            Ok(source) => return Ok(source),
            Err(error) => failures.push(error),
        }
    }
    Err(format!(
        "no serial port discovered for {pattern}: {}",
        failures.join("; ")
    ))
}

fn open_serial(path: &str, config: &GatewayConfig) -> Result<ByteSource, String> {
    serialport::new(path, config.serial_baudrate)
        .data_bits(config.serial_data_bits)
        .parity(config.serial_parity)
        .stop_bits(config.serial_stop_bits)
        .timeout(config.serial_read_timeout)
        .open()
        .map(ByteSource::Serial)
        .map_err(|error| format!("open serial port {path}: {error}"))
}
