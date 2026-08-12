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
- `buspcrt-modem-watchdog.service`
- `buspcrt-updater.service` and `buspcrt-updater.timer`

Unpack the Linux release bundle into `/opt/pcrt` before installing services.
The bundle contains the release binaries in its root:

```bash
cd /opt/pcrt
sudo PROJECT_ROOT=/opt/pcrt bash scripts/services/install_services.sh
```

The installer reads `NUMBER_CAMS` from `/etc/pcrt/device.env` and installs only
valid `recorder-cam*.env` files whose numeric `CAMERA_ID` is within that range.
All services run as root and are restarted by systemd on failure.

`buspcrt-updater.timer` fast-forwards the configured Git branch and restarts
native services. The branch must contain current release binaries in the project
root; the updater does not run Cargo on the vehicle.

Production IPC is `ipc:///run/pcrt/doors.sock`. The gateway unit creates
`/run/pcrt`; recorder and processor configurations must retain this endpoint.

For a newly cloned device image, run the interactive first-boot script once:

```bash
sudo PROJECT_ROOT=/opt/pcrt bash scripts/firstboot/setup_firstboot.sh
```

It writes `/etc/pcrt/device.env` and `/etc/pcrt/frpc.toml`, configures the
hostname, regenerates machine/SSH identities, starts the reverse FRP tunnel and
installs the native services.
