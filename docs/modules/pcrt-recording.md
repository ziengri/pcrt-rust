# pcrt-recording

## Migration contract

Rust replaces one Python `app/run_recorder.py` process at a time. Each process owns
one configured camera and one `DOOR_CHANNEL`; a door-open interval produces at most
one video session for that camera.

Configuration names and precedence remain compatible:

```text
defaults < config.env < recorder-camN.env < environment < CLI
```

Required keys are `SOURCE`, `CAMERA_ID` and `DOOR_CHANNEL`. Existing keys retain
their meanings: `SESSIONS_DIR`, `ZMQ_IPC_ENDPOINT`, `DOOR_OPEN_VALUE`, `WIDTH`,
`HEIGHT`, `FPS`, `MAX_SESSION_SECONDS` and `IDLE_SLEEP`.

The recorder subscribes only to `door.<DOOR_CHANNEL>.state`. It consumes the latest
valid message, treats missing or `stale: true` as closed, and treats
`state == DOOR_OPEN_VALUE` as open. The ZeroMQ wire frame remains
`<topic><space><compact JSON>`.

## Recording lifecycle

```text
Idle
  -- open and first frame --> Capturing
Capturing
  -- closed or stale --> Finalizing --> Idle
  -- duration limit --> DiscardedUntilClosed
  -- encoder/storage failure --> DiscardedUntilClosed
DiscardedUntilClosed
  -- closed or stale --> Idle
```

Repeated open messages do not create another writer. A writer is created only after
both open state and a valid frame are available. The duration cap remains frame based
for compatibility: a frame that makes `frame_count > MAX_SESSION_SECONDS * FPS`
causes that capture to be discarded and blocks a new capture until the door closes.
The normal duration-limit discard closes ffmpeg and removes its `capturing` directory
immediately, so it cannot consume storage until the next recovery. Encoder and storage
failures suppress new writes until a closed/stale door state; their incomplete capture
is retained for durable recovery diagnostics.

On clean `SIGTERM`/`SIGINT`, the binary stops source reads, closes encoder input and
attempts bounded finalization of an active capture. It never emits a synthetic door
state. Encoder failure never publishes a ready session.

## Video encoder

`OpenCvVideoSource` uses `VideoCapture` for numeric cameras, local files and RTSP
URLs. Network URLs use the OpenCV FFmpeg backend with a two-second open/read timeout,
so a stalled RTSP read cannot block service shutdown indefinitely. `RecordingService`
owns normalized door state and reads one frame each iteration,
requires `CV_8UC3` BGR input,
and, only while the door is open, resizes with `INTER_LINEAR` to configured `WIDTH x
HEIGHT`, then supplies exact `bgr24` raw bytes to `ffmpeg`. Closed/stale door state
still reads and discards frames to keep RTSP current, but skips BGR validation and
resize. Local file EOF finalizes an active capture and resets the file; an
RTSP/camera no-frame result is retried later. The Rust encoder command is:

```text
ffmpeg -hide_banner -loglevel error -y \
  -f rawvideo -pixel_format bgr24 -video_size <WIDTH>x<HEIGHT> -framerate <FPS> \
  -i pipe:0 -an -c:v libx264 -preset fast -crf 18 -f matroska <capture>/camN.mkv
```

`libx264`, `fast` and CRF `18` are intentional migration changes from Python
`ffv1`. Published storage metadata records `codec: "libx264"` and `format: "mkv"`.
The adapter uses bounded close and child-kill/reap steps so systemd shutdown cannot
leave an ffmpeg child behind. Process-group termination remains a future hardening step.

## Storage

At startup the binary calls camera-scoped `SessionStorage::recover_recording()`
before opening camera input. It never modifies processor-owned `ready` or `claimed`
sessions.
For every capture it writes only under the `CaptureSession` directory and finalizes
through `SessionStorage::finalize_capture`. Storage hashes and fsyncs the video,
writes the manifest, then atomically publishes the whole session directory to
`ready`. This deliberately replaces Python's separate video/metadata pair moves and
destructive cleanup of interrupted sessions.

The first Rust recorder publishes exactly one `CapturedVideo` named `camN.mkv` for a
door-open interval. Its session id is created by `SessionStorage` from `CAMERA_ID`
and the start timestamp.

## Implementation order

1. `pcrt-recording` lifecycle FSM, OpenCV source/resize loop, ffmpeg encoder and storage orchestration.
2. `pcrt-recorder` config and `pcrt-storage` startup recovery wiring.
3. `pcrt-recorder` ZeroMQ door-state adapter and one-camera shadow run.
4. Harden ffmpeg supervision with process-group termination.
