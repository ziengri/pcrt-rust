#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "OpenCV pixels and the fixed 256x256 model coordinate space are converted to f32 for the model contract."
)]

use std::path::Path;

use opencv::{
    core::Mat,
    imgproc,
    prelude::{MatTraitConst, MatTraitConstManual, VideoCaptureTrait, VideoCaptureTraitConst},
    videoio,
};
use openvino::{
    CompiledModel, Core, DeviceType, ElementType, InferRequest, InferenceErrorKind, RwPropertyKey,
    Shape, Tensor,
};
use pcrt_model::PassengerCounts;
use pcrt_processing::{InferenceBackend, InferenceError};
use pcrt_service::ShutdownToken;
use pcrt_storage::ClaimedSession;

use super::{
    crossing::LineCounter,
    tracking::{Detection, Tracker, TrackerConfig, TrackerUpdate},
};
use crate::config::{InferenceConfig, ProcessorConfig};

const REQUEST_WAIT_MS: i64 = 50;

/// Native production inference adapter using `OpenCV` `FFmpeg`, `OpenVINO` and `ByteTrack`.
pub(crate) struct NativeInferenceBackend {
    config: InferenceConfig,
    // The compiled model uses OpenVINO runtime state owned by this core.
    _core: Core,
    compiled_model: CompiledModel,
}

impl NativeInferenceBackend {
    pub(crate) fn new(config: &ProcessorConfig) -> Result<Self, String> {
        let model_path = &config.inference.model_path;
        let weights_path = model_path.with_extension("bin");
        if !model_path.is_file() || !weights_path.is_file() {
            return Err(format!(
                "expected OpenVINO model files: {} and {}",
                model_path.display(),
                weights_path.display()
            ));
        }
        let model_path = path_as_utf8(model_path)?;
        let weights_path = path_as_utf8(&weights_path)?;
        let mut core = Core::new().map_err(|error| format!("initialize OpenVINO: {error}"))?;
        core.set_property(
            &DeviceType::CPU,
            &RwPropertyKey::NumStreams,
            &config.inference.streams.to_string(),
        )
        .map_err(|error| format!("configure OpenVINO CPU streams: {error}"))?;
        let model = core
            .read_model_from_file(model_path, weights_path)
            .map_err(|error| format!("read OpenVINO model: {error}"))?;
        let compiled_model = core
            .compile_model(&model, DeviceType::CPU)
            .map_err(|error| format!("compile OpenVINO model: {error}"))?;
        Ok(Self {
            config: config.inference.clone(),
            _core: core,
            compiled_model,
        })
    }
}

impl InferenceBackend for NativeInferenceBackend {
    fn analyze(
        &mut self,
        session: &ClaimedSession,
        shutdown: &ShutdownToken,
    ) -> Result<PassengerCounts, InferenceError> {
        let [video] = session.manifest().videos.as_slice() else {
            return Err(InferenceError::terminal(
                "native inference requires exactly one video per session",
            ));
        };
        let video_path = session.directory().join(&video.path);
        let mut decoder = VideoDecoder::open(&video_path, self.config.skip_frames)?;
        let mut requests = (0..self.config.streams)
            .map(|_| {
                self.compiled_model.create_infer_request().map_err(|error| {
                    InferenceError::terminal(format!("create OpenVINO request: {error}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut pending = (0..self.config.streams).map(|_| None).collect::<Vec<_>>();
        let mut tracker = Tracker::new(TrackerConfig {
            track_threshold: self.config.track_threshold,
            track_buffer: self.config.track_buffer,
            match_threshold: self.config.track_match_threshold,
            init_threshold: self.config.track_init_threshold,
        });
        let mut counter = None;
        let mut last_observed_frame = None;
        let mut submitted = 0_usize;

        while let Some(frame) = decoder.read_sampled_frame(shutdown)? {
            if shutdown.is_shutdown_requested() {
                cancel_pending_requests(&mut requests, &pending);
                return Err(InferenceError::Cancelled);
            }
            let slot = submitted % requests.len();
            if pending[slot].is_some() {
                let completed = match finish_inference(
                    &mut requests[slot],
                    &mut pending[slot],
                    self.config.confidence,
                    self.config.target_size,
                    shutdown,
                ) {
                    Ok(completed) => completed,
                    Err(error) => {
                        cancel_pending_requests(&mut requests, &pending);
                        return Err(error);
                    }
                };
                observe_completed_frame(
                    &completed,
                    &mut tracker,
                    &mut counter,
                    &mut last_observed_frame,
                    self.config.line_y_ratio,
                    self.config.track_buffer,
                );
            }

            let input = create_input_tensor(&frame, self.config.target_size)?;
            requests[slot].set_input_tensor(&input).map_err(|error| {
                InferenceError::terminal(format!("set OpenVINO input: {error}"))
            })?;
            requests[slot].infer_async().map_err(|error| {
                InferenceError::terminal(format!("start OpenVINO inference: {error}"))
            })?;
            pending[slot] = Some(PendingInference {
                _input: input,
                frame,
            });
            submitted += 1;
        }

        while let Some(slot) = earliest_pending_slot(&pending) {
            let completed = match finish_inference(
                &mut requests[slot],
                &mut pending[slot],
                self.config.confidence,
                self.config.target_size,
                shutdown,
            ) {
                Ok(completed) => completed,
                Err(error) => {
                    cancel_pending_requests(&mut requests, &pending);
                    return Err(error);
                }
            };
            observe_completed_frame(
                &completed,
                &mut tracker,
                &mut counter,
                &mut last_observed_frame,
                self.config.line_y_ratio,
                self.config.track_buffer,
            );
        }

        let counter =
            counter.ok_or_else(|| InferenceError::terminal("video contains no frames"))?;
        Ok(PassengerCounts {
            entered: counter.entered(),
            exited: counter.exited(),
        })
    }
}

struct VideoDecoder {
    capture: videoio::VideoCapture,
    source_frames_read: usize,
    frame_stride: usize,
}

impl VideoDecoder {
    fn open(path: &Path, skip_frames: usize) -> Result<Self, InferenceError> {
        let path = path_as_utf8(path).map_err(InferenceError::terminal)?;
        let capture = videoio::VideoCapture::from_file(path, videoio::CAP_FFMPEG)
            .map_err(|error| InferenceError::terminal(format!("open video: {error}")))?;
        if !capture
            .is_opened()
            .map_err(|error| InferenceError::terminal(format!("check video decoder: {error}")))?
        {
            return Err(InferenceError::terminal(
                "OpenCV could not open video with its FFmpeg backend",
            ));
        }
        let backend = capture.get_backend_name().map_err(|error| {
            InferenceError::terminal(format!("inspect video decoder backend: {error}"))
        })?;
        if backend != "FFMPEG" {
            return Err(InferenceError::terminal(format!(
                "OpenCV selected {backend} instead of required FFMPEG backend"
            )));
        }
        let frame_stride = skip_frames
            .checked_add(1)
            .ok_or_else(|| InferenceError::terminal("AI_SKIP_FRAMES is too large"))?;
        Ok(Self {
            capture,
            source_frames_read: 0,
            frame_stride,
        })
    }

    fn read_sampled_frame(
        &mut self,
        shutdown: &ShutdownToken,
    ) -> Result<Option<Frame>, InferenceError> {
        loop {
            if shutdown.is_shutdown_requested() {
                return Err(InferenceError::Cancelled);
            }
            let mut bgr = Mat::default();
            if !self
                .capture
                .read(&mut bgr)
                .map_err(|error| InferenceError::terminal(format!("decode video frame: {error}")))?
                || bgr.empty()
            {
                return Ok(None);
            }
            let source_frame_index = self.source_frames_read;
            self.source_frames_read = self
                .source_frames_read
                .checked_add(1)
                .ok_or_else(|| InferenceError::terminal("video frame index overflow"))?;
            if !source_frame_index.is_multiple_of(self.frame_stride) {
                continue;
            }
            let mut rgb = Mat::default();
            imgproc::cvt_color_def(&bgr, &mut rgb, imgproc::COLOR_BGR2RGB).map_err(|error| {
                InferenceError::terminal(format!("convert video frame to RGB: {error}"))
            })?;
            let width = usize::try_from(rgb.cols())
                .map_err(|_| InferenceError::terminal("OpenCV returned an invalid frame width"))?;
            let height = usize::try_from(rgb.rows())
                .map_err(|_| InferenceError::terminal("OpenCV returned an invalid frame height"))?;
            if width == 0 || height == 0 {
                return Err(InferenceError::terminal(
                    "OpenCV returned an empty video frame",
                ));
            }
            let rgb = rgb
                .data_bytes()
                .map_err(|error| {
                    InferenceError::terminal(format!("read RGB frame bytes: {error}"))
                })?
                .to_vec();
            return Ok(Some(Frame {
                width,
                height,
                rgb,
                source_frame_index,
            }));
        }
    }
}

struct Frame {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
    source_frame_index: usize,
}

struct PendingInference {
    // The request borrows the tensor data until its asynchronous inference is complete.
    _input: Tensor,
    frame: Frame,
}

struct CompletedInference {
    detections: Vec<Detection>,
    frame: Frame,
}

fn create_input_tensor(frame: &Frame, target_size: usize) -> Result<Tensor, InferenceError> {
    let dimension = i64::try_from(target_size)
        .map_err(|_| InferenceError::terminal("AI_TARGET_SIZE is too large"))?;
    let shape = Shape::new(&[1, 3, dimension, dimension]).map_err(|error| {
        InferenceError::terminal(format!("create OpenVINO input shape: {error}"))
    })?;
    let mut input = Tensor::new(ElementType::F32, &shape)
        .map_err(|error| InferenceError::terminal(format!("allocate OpenVINO input: {error}")))?;
    let destination = input
        .get_data_mut::<f32>()
        .map_err(|error| InferenceError::terminal(format!("access OpenVINO input: {error}")))?;
    letterbox_rgb_to_nchw(frame, target_size, destination);
    Ok(input)
}

fn letterbox_rgb_to_nchw(frame: &Frame, target_size: usize, destination: &mut [f32]) {
    let scale =
        (target_size as f32 / frame.width as f32).min(target_size as f32 / frame.height as f32);
    let resized_width = (frame.width as f32 * scale).round() as usize;
    let resized_height = (frame.height as f32 * scale).round() as usize;
    let offset_x = (target_size - resized_width) / 2;
    let offset_y = (target_size - resized_height) / 2;
    let plane_size = target_size * target_size;

    destination.fill(114.0 / 255.0);
    for output_y in 0..resized_height {
        let source_y = ((output_y as f32 + 0.5) * frame.height as f32 / resized_height as f32
            - 0.5)
            .clamp(0.0, (frame.height - 1) as f32);
        let y0 = source_y.floor() as usize;
        let y1 = (y0 + 1).min(frame.height - 1);
        let y_weight = source_y - y0 as f32;
        for output_x in 0..resized_width {
            let source_x = ((output_x as f32 + 0.5) * frame.width as f32 / resized_width as f32
                - 0.5)
                .clamp(0.0, (frame.width - 1) as f32);
            let x0 = source_x.floor() as usize;
            let x1 = (x0 + 1).min(frame.width - 1);
            let x_weight = source_x - x0 as f32;
            let destination_index = (output_y + offset_y) * target_size + output_x + offset_x;
            for channel in 0..3 {
                let top_left = frame.rgb[(y0 * frame.width + x0) * 3 + channel] as f32;
                let top_right = frame.rgb[(y0 * frame.width + x1) * 3 + channel] as f32;
                let bottom_left = frame.rgb[(y1 * frame.width + x0) * 3 + channel] as f32;
                let bottom_right = frame.rgb[(y1 * frame.width + x1) * 3 + channel] as f32;
                let top = top_left + (top_right - top_left) * x_weight;
                let bottom = bottom_left + (bottom_right - bottom_left) * x_weight;
                destination[channel * plane_size + destination_index] =
                    (top + (bottom - top) * y_weight) / 255.0;
            }
        }
    }
}

fn finish_inference(
    request: &mut InferRequest,
    pending: &mut Option<PendingInference>,
    confidence: f32,
    target_size: usize,
    shutdown: &ShutdownToken,
) -> Result<CompletedInference, InferenceError> {
    wait_for_inference(request, shutdown)?;
    let pending = pending
        .take()
        .ok_or_else(|| InferenceError::terminal("no inference is pending"))?;
    let output = request
        .get_output_tensor()
        .map_err(|error| InferenceError::terminal(format!("read OpenVINO output: {error}")))?;
    let values = output
        .get_data::<f32>()
        .map_err(|error| InferenceError::terminal(format!("access OpenVINO output: {error}")))?;
    let detections = detections_from_output(values, confidence, &pending.frame, target_size)?;
    Ok(CompletedInference {
        detections,
        frame: pending.frame,
    })
}

fn wait_for_inference(
    request: &mut InferRequest,
    shutdown: &ShutdownToken,
) -> Result<(), InferenceError> {
    loop {
        if shutdown.is_shutdown_requested() {
            let _ = request.cancel();
            return Err(InferenceError::Cancelled);
        }
        match request.wait(REQUEST_WAIT_MS) {
            Ok(()) => return Ok(()),
            Err(error) if matches!(error.kind, InferenceErrorKind::ResultNotReady) => {}
            Err(error) => {
                return Err(InferenceError::terminal(format!(
                    "wait for OpenVINO inference: {error}"
                )));
            }
        }
    }
}

fn detections_from_output(
    output: &[f32],
    confidence: f32,
    frame: &Frame,
    target_size: usize,
) -> Result<Vec<Detection>, InferenceError> {
    if !output.len().is_multiple_of(6) {
        return Err(InferenceError::terminal(format!(
            "unexpected OpenVINO output length {}; expected rows of six values",
            output.len()
        )));
    }
    let geometry = LetterboxGeometry::new(frame.width, frame.height, target_size);
    Ok(output
        .chunks_exact(6)
        .filter(|detection| detection[4] >= confidence && detection[5] >= 0.0)
        .filter_map(|detection| {
            let x1 = geometry.to_source_x(detection[0]);
            let y1 = geometry.to_source_y(detection[1]);
            let x2 = geometry.to_source_x(detection[2]);
            let y2 = geometry.to_source_y(detection[3]);
            (x2 > x1 && y2 > y1).then_some(Detection {
                x1,
                y1,
                x2,
                y2,
                confidence: detection[4],
                class_id: detection[5] as i64,
            })
        })
        .collect())
}

struct LetterboxGeometry {
    scale: f32,
    offset_x: usize,
    offset_y: usize,
    frame_width: usize,
    frame_height: usize,
}

impl LetterboxGeometry {
    fn new(frame_width: usize, frame_height: usize, target_size: usize) -> Self {
        let scale =
            (target_size as f32 / frame_width as f32).min(target_size as f32 / frame_height as f32);
        let resized_width = (frame_width as f32 * scale).round() as usize;
        let resized_height = (frame_height as f32 * scale).round() as usize;
        Self {
            scale,
            offset_x: (target_size - resized_width) / 2,
            offset_y: (target_size - resized_height) / 2,
            frame_width,
            frame_height,
        }
    }

    fn to_source_x(&self, x: f32) -> f32 {
        ((x - self.offset_x as f32) / self.scale).clamp(0.0, self.frame_width as f32)
    }

    fn to_source_y(&self, y: f32) -> f32 {
        ((y - self.offset_y as f32) / self.scale).clamp(0.0, self.frame_height as f32)
    }
}

fn observe_completed_frame(
    completed: &CompletedInference,
    tracker: &mut Tracker,
    counter: &mut Option<LineCounter>,
    last_observed_frame: &mut Option<usize>,
    line_y_ratio: f32,
    track_buffer: usize,
) {
    let start = last_observed_frame.map_or(0, |frame| frame + 1);
    for frame_index in start..completed.frame.source_frame_index {
        observe_tracker_update(
            &tracker.update(&[]),
            completed.frame.height,
            frame_index,
            counter,
            line_y_ratio,
            track_buffer,
        );
    }
    let frame_index = completed.frame.source_frame_index;
    observe_tracker_update(
        &tracker.update(&completed.detections),
        completed.frame.height,
        frame_index,
        counter,
        line_y_ratio,
        track_buffer,
    );
    *last_observed_frame = Some(frame_index);
}

fn observe_tracker_update(
    update: &TrackerUpdate,
    frame_height: usize,
    frame_index: usize,
    counter: &mut Option<LineCounter>,
    line_y_ratio: f32,
    track_buffer: usize,
) {
    let counter =
        counter.get_or_insert_with(|| LineCounter::new(frame_height, line_y_ratio, track_buffer));
    counter.observe(&update.confirmed, &update.predicted, frame_index);
}

fn earliest_pending_slot(pending: &[Option<PendingInference>]) -> Option<usize> {
    pending
        .iter()
        .enumerate()
        .filter_map(|(slot, pending)| {
            pending
                .as_ref()
                .map(|pending| (slot, pending.frame.source_frame_index))
        })
        .min_by_key(|(_, frame_index)| *frame_index)
        .map(|(slot, _)| slot)
}

fn cancel_pending_requests(requests: &mut [InferRequest], pending: &[Option<PendingInference>]) {
    for (request, pending) in requests.iter_mut().zip(pending) {
        if pending.is_some() {
            let _ = request.cancel();
        }
    }
}

fn path_as_utf8(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use pcrt_model::SessionId;
    use pcrt_processing::{ProcessingStep, Processor};
    use pcrt_result_queue::{ResultQueue, Timestamp};
    use pcrt_service::ShutdownToken;
    use pcrt_storage::{CaptureMetadata, CapturedVideo, SessionStorage};
    use tempfile::tempdir;

    use super::{Frame, LetterboxGeometry, NativeInferenceBackend, letterbox_rgb_to_nchw};
    use crate::{
        config::{InferenceConfig, ProcessorConfig},
        result_encoder::TimelineResultEncoder,
    };

    #[test]
    fn letterbox_restores_source_coordinates() {
        let geometry = LetterboxGeometry::new(320, 240, 256);

        assert_eq!(geometry.to_source_x(0.0), 0.0);
        assert_eq!(geometry.to_source_y(32.0), 0.0);
        assert_eq!(geometry.to_source_x(256.0), 320.0);
        assert_eq!(geometry.to_source_y(224.0), 240.0);
    }

    #[test]
    fn letterbox_uses_rgb_channels_and_padding() {
        let frame = Frame {
            width: 1,
            height: 1,
            rgb: vec![255, 128, 0],
            source_frame_index: 0,
        };
        let mut destination = vec![0.0; 3 * 4 * 4];
        letterbox_rgb_to_nchw(&frame, 4, &mut destination);

        assert_eq!(destination[0], 1.0);
        assert_eq!(destination[16], 128.0 / 255.0);
        assert_eq!(destination[32], 0.0);
    }

    #[test]
    #[ignore = "requires the committed OpenVINO model and real video fixture"]
    fn production_fixture_completes_session_and_publishes_timeline_result() {
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf();
        let video_fixture = repository.join("4.mp4");
        let model_path = repository
            .join("models")
            .join("yolo26n-head-v3_int8_openvino_model")
            .join("yolo26n-head-v3.xml");
        assert!(video_fixture.is_file());
        assert!(model_path.is_file());

        let temporary = tempdir().unwrap();
        let storage = SessionStorage::open(temporary.path().join("sessions")).unwrap();
        let session_id = SessionId::new("cam-1-0").unwrap();
        let capture = storage
            .begin_capture(&session_id, CaptureMetadata::new("fixture-camera", 0))
            .unwrap();
        fs::copy(&video_fixture, capture.video_path("cam1.mp4").unwrap()).unwrap();
        storage
            .finalize_capture(
                capture,
                1,
                &[CapturedVideo::new("1", "cam1.mp4", "h264", "mp4", 1, 1, 1).unwrap()],
            )
            .unwrap();
        let queue_path = temporary.path().join("results.sqlite");
        let config = ProcessorConfig {
            sessions_dir: temporary.path().join("sessions"),
            queue_path: queue_path.clone(),
            endpoint: "inproc://fixture".to_owned(),
            door_state_ttl: std::time::Duration::from_secs(2),
            idle_sleep: std::time::Duration::from_millis(1),
            exit_after: None,
            bus_id: "BUS-FIXTURE".to_owned(),
            inference: InferenceConfig {
                model_path,
                streams: 4,
                confidence: 0.25,
                skip_frames: 2,
                target_size: 256,
                line_y_ratio: 0.40,
                track_threshold: 0.50,
                track_buffer: 30,
                track_match_threshold: 0.80,
                track_init_threshold: 0.60,
            },
        };
        let backend = NativeInferenceBackend::new(&config).unwrap();
        let mut processor = Processor::new(
            storage,
            ResultQueue::open(&queue_path).unwrap(),
            backend,
            TimelineResultEncoder::new(config.bus_id),
            ShutdownToken::default(),
        );

        assert_eq!(
            processor.process_one(true, 2).unwrap(),
            ProcessingStep::Completed(session_id.clone())
        );
        let entry = ResultQueue::open(queue_path)
            .unwrap()
            .next_due(Timestamp::from_unix_millis(2))
            .unwrap()
            .unwrap();
        let payload = serde_json::from_str::<serde_json::Value>(&entry.payload_json).unwrap();
        assert_eq!(entry.session_id, session_id);
        assert_eq!(payload["bus"], "BUS-FIXTURE");
        assert!(payload["in"].is_u64());
        assert!(payload["out"].is_u64());
    }
}
