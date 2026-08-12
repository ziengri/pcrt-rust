#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="${PROJECT_ROOT:-/opt/pcrt}"
DEVICE_ENV_FILE="${DEVICE_ENV_FILE:-/etc/pcrt/device.env}"
SYSTEMD_DIR="/etc/systemd/system"

die() { echo "[ERROR] $*" >&2; exit 1; }
log_info() { echo "[INFO] $*"; }
log_warn() { echo "[WARN] $*" >&2; }
require_root() { [[ "${EUID:-$(id -u)}" -eq 0 ]] || die "Run as root (or via sudo)."; }
require_file() { [[ -f "$1" ]] || die "File not found: $1"; }
require_executable() { [[ -x "$1" ]] || die "Executable not found: $1"; }
service_script() { echo "$PROJECT_ROOT/scripts/services/$1"; }

read_env_value() {
  local env_file="$1" key="$2"
  awk -F= -v key="$key" '$0 ~ "^[[:space:]]*" key "=" { sub(/^[^=]*=/, ""); gsub(/^[[:space:]]+|[[:space:]]+$/, ""); print; exit }' "$env_file"
}

recorder_instances() {
  { systemctl list-unit-files 'buspcrt-recorder@*.service' --no-legend --no-pager 2>/dev/null || true; } |
    awk '{print $1}' |
    sed -n 's/^buspcrt-recorder@\([0-9][0-9]*\)\.service$/\1/p'
  { systemctl list-units --all 'buspcrt-recorder@*.service' --no-legend --no-pager 2>/dev/null || true; } |
    awk '{print $1}' |
    sed -n 's/^buspcrt-recorder@\([0-9][0-9]*\)\.service$/\1/p'
  find "$SYSTEMD_DIR" -maxdepth 1 -type d -name 'buspcrt-recorder@[0-9]*.service.d' -printf '%f\n' 2>/dev/null |
    sed -n 's/^buspcrt-recorder@\([0-9][0-9]*\)\.service\.d$/\1/p'
}

uninstall_recorder_services() {
  local instance
  while IFS= read -r instance; do
    [[ -n "$instance" ]] || continue
    bash "$(service_script recorder_service.sh)" uninstall --instance "$instance"
  done < <(recorder_instances | sort -u)
  rm -f "$SYSTEMD_DIR/buspcrt-recorder@.service"
}

install_recorder_services() {
  local installed=0 env_file camera_id door_channel
  shopt -s nullglob
  for env_file in "$PROJECT_ROOT"/recorder-cam*.env; do
    camera_id="$(read_env_value "$env_file" CAMERA_ID)"
    door_channel="$(read_env_value "$env_file" DOOR_CHANNEL)"
    if [[ -z "$camera_id" || -z "$(read_env_value "$env_file" SOURCE)" || -z "$door_channel" ]]; then
      log_warn "Skipping incomplete recorder config: $env_file"
      continue
    fi
    [[ "$camera_id" =~ ^[0-9]+$ ]] || die "CAMERA_ID must be numeric in $env_file"
    [[ "$door_channel" =~ ^[1-4]$ ]] || die "DOOR_CHANNEL must be from 1 to 4 in $env_file"
    if (( camera_id < 1 || camera_id > NUMBER_CAMS )); then
      log_info "Skipping camera $camera_id; NUMBER_CAMS=$NUMBER_CAMS"
      continue
    fi
    bash "$(service_script recorder_service.sh)" install --instance "$camera_id" --project-root "$PROJECT_ROOT" --env "$env_file" --binary "$(binary_path pcrt-recorder)"
    installed=$((installed + 1))
  done
  shopt -u nullglob
  (( installed > 0 )) || log_warn "No recorder services were installed. Check recorder-cam*.env files."
}

main() {
  require_root
  require_file "$DEVICE_ENV_FILE"
  # shellcheck disable=SC1090
  source "$DEVICE_ENV_FILE"
  [[ "${NUMBER_CAMS:-}" =~ ^(3|4)$ ]] || die "NUMBER_CAMS must be 3 or 4 in $DEVICE_ENV_FILE"

  require_file "$PROJECT_ROOT/config.env"
  require_file "$PROJECT_ROOT/door_gateway.env"
  require_file "$PROJECT_ROOT/processor.env"
  require_file "$PROJECT_ROOT/uploader.env"
  require_file "$PROJECT_ROOT/modem-watchdog.env"
  require_file "$PROJECT_ROOT/updater.env"
  grep -qx 'ZMQ_IPC_ENDPOINT=ipc:///run/pcrt/doors.sock' "$PROJECT_ROOT/config.env" || \
    die "config.env must set ZMQ_IPC_ENDPOINT=ipc:///run/pcrt/doors.sock"
  require_executable "$PROJECT_ROOT/target/release/pcrt-door-gateway"
  require_executable "$PROJECT_ROOT/target/release/pcrt-recorder"
  require_executable "$PROJECT_ROOT/target/release/pcrt-processor"
  require_executable "$PROJECT_ROOT/target/release/pcrt-uploader"
  require_file "$(service_script door_gateway_service.sh)"
  require_file "$(service_script recorder_service.sh)"
  require_file "$(service_script processor_service.sh)"
  require_file "$(service_script uploader_service.sh)"
  require_file "$(service_script modem_watchdog_service.sh)"
  require_file "$(service_script updater_service.sh)"

  log_info "Reinstalling native Rust services from $PROJECT_ROOT for $NUMBER_CAMS cameras"
  uninstall_recorder_services
  bash "$(service_script uploader_service.sh)" uninstall
  bash "$(service_script modem_watchdog_service.sh)" uninstall
  bash "$(service_script updater_service.sh)" uninstall
  bash "$(service_script processor_service.sh)" uninstall
  bash "$(service_script door_gateway_service.sh)" uninstall

  bash "$(service_script door_gateway_service.sh)" install --project-root "$PROJECT_ROOT" --env "$PROJECT_ROOT/door_gateway.env"
  bash "$(service_script processor_service.sh)" install --project-root "$PROJECT_ROOT" --env "$PROJECT_ROOT/processor.env"
  bash "$(service_script uploader_service.sh)" install --project-root "$PROJECT_ROOT" --env "$PROJECT_ROOT/uploader.env"
  bash "$(service_script modem_watchdog_service.sh)" install --project-root "$PROJECT_ROOT" --env "$PROJECT_ROOT/modem-watchdog.env"
  bash "$(service_script updater_service.sh)" install --project-root "$PROJECT_ROOT" --env "$PROJECT_ROOT/updater.env"
  install_recorder_services
  log_info "Native Rust services reinstalled successfully"
}
main "$@"
