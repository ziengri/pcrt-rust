//! Private operational adapters and policies for one recorder process.

mod encoder;
mod gate;
mod service;
mod source;

pub(crate) use encoder::FfmpegEncoderFactory;
pub(crate) use gate::DoorGate;
pub(crate) use service::{RecordingService, RecordingServiceStep};
pub(crate) use source::OpenCvVideoSource;
