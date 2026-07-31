#![forbid(unsafe_code)]
//! Door-gated recording lifecycle and video source adapters.

pub mod ffmpeg;
pub mod lifecycle;
pub mod recorder;
#[cfg(feature = "opencv-source")]
pub mod service;
#[cfg(feature = "opencv-source")]
pub mod video;
