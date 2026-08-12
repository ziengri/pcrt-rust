# Native Rust Deployment Scripts

## Build Linux Release Bundle

From the repository root:

```bash
./scripts/release/build_linux_bundle.sh
```

The bundle is created at `release/linux/`. Runtime binaries are placed directly
in that directory, alongside the current service configuration files, `models/`
and the service/firstboot scripts. Private keys, `license.lic`, `device.env`,
sessions and databases are intentionally excluded.

These scripts install the native Rust services from `/opt/pcrt`:

- `buspcrt-door-gateway.service`
- `buspcrt-recorder@<CAMERA_ID>.service`
- `buspcrt-processor.service`
- `buspcrt-uploader.service`

Build the release binaries before installing services:

```bash
cd /opt/pcrt
cargo build --release --workspace
sudo PROJECT_ROOT=/opt/pcrt bash scripts/services/install_services.sh
```

The installer reads `NUMBER_CAMS` from `/etc/pcrt/device.env` and installs only
valid `recorder-cam*.env` files whose numeric `CAMERA_ID` is within that range.
All services run as root and are restarted by systemd on failure.

Production IPC is `ipc:///run/pcrt/doors.sock`. The gateway unit creates
`/run/pcrt`; recorder and processor configurations must retain this endpoint.

For a newly cloned device image, run the interactive first-boot script once:

```bash
sudo PROJECT_ROOT=/opt/pcrt bash scripts/firstboot/setup_firstboot.sh
```

It writes `/etc/pcrt/device.env` and `/etc/pcrt/frpc.toml`, configures the
hostname, regenerates machine/SSH identities, starts the reverse FRP tunnel and
installs the native services.
