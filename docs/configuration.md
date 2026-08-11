# Native Rust Configuration

Run native binaries with `rust/` as their working directory. Their default
configuration filenames then resolve without command-line flags.

## Project Configuration

| File | Native process | Purpose |
| --- | --- | --- |
| `config.env` | recorder, processor, uploader, gateway | Shared session root, result queue and ZeroMQ endpoint. |
| `door_gateway.env` | `pcrt-door-gateway` | RS-232 transport and door-state timing. |
| `recorder-cam.env` | `pcrt-recorder` | Camera 1 and door 1. |
| `recorder-cam2.env` | `pcrt-recorder` | Camera 2 and door 2. |
| `recorder-cam3.env` | `pcrt-recorder` | Camera 3 and door 3. |
| `recorder-cam4.env` | `pcrt-recorder` | Camera 4 and door 4. |
| `processor.env` | `pcrt-processor` | OpenCV, OpenVINO and ByteTrack settings. |
| `uploader.env` | `pcrt-uploader` | Timeline API and delivery retry settings. |

`SESSIONS_DIR`, `RESULT_QUEUE_DB` and `ZMQ_IPC_ENDPOINT` belong only in
`config.env`. Do not override these per service: all writers and readers must
use the same paths and IPC endpoint. `RESULT_QUEUE_DB` is the contract between
processor and uploader.

## Device Configuration

`external-config/device.env` is a template, not a project runtime config.
Populate it during device provisioning and install it as:

```text
/etc/pcrt/device.env
```

It contains device identity and physical capacity:

```dotenv
BUS_ID=BUS-001
NUMBER_CAMS=3
```

`BUS_ID` is required by `pcrt-processor` for timeline payloads. `NUMBER_CAMS`
is the fallback door count for `pcrt-door-gateway` and determines whether the
fourth recorder configuration is enabled. Valid values are `3` and `4`.

## Invocation

```bash
cd rust

cargo run -p pcrt-door-gateway -- \
  --device-env-file /etc/pcrt/device.env

cargo run -p pcrt-recorder -- --env-file recorder-cam.env
cargo run -p pcrt-recorder -- --env-file recorder-cam2.env
cargo run -p pcrt-recorder -- --env-file recorder-cam3.env
# Run this only when NUMBER_CAMS=4.
cargo run -p pcrt-recorder -- --env-file recorder-cam4.env

cargo run -p pcrt-processor -- \
  --device-env-file /etc/pcrt/device.env

cargo run -p pcrt-uploader --
```

All binaries support explicit `--config-env-file` and `--env-file` paths for
tests or alternate deployments. Config precedence is defaults, shared project
file, service project file, process environment and CLI. Device configuration
is read separately and is never overridden by project service files.
