#!/usr/bin/env bash
set -euo pipefail

SERVICE_NAME="buspcrt-updater.service"
TIMER_NAME="buspcrt-updater.timer"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}"
TIMER_PATH="/etc/systemd/system/${TIMER_NAME}"
die() { echo "[ERROR] $*" >&2; exit 1; }
require_root() { [[ "${EUID:-$(id -u)}" -eq 0 ]] || die "Run as root (or via sudo)."; }
abs_path() { realpath "$1" 2>/dev/null || readlink -f "$1"; }
read_env_value() { awk -F= -v key="$2" '$0 ~ "^[[:space:]]*" key "=" { sub(/^[^=]*=/, ""); gsub(/^[[:space:]]+|[[:space:]]+$/, ""); print; exit }' "$1"; }
usage() { echo "Usage: updater_service.sh install --project-root PATH --env FILE | uninstall"; }

install_service() {
  local project_root="" env_file=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --project-root) [[ $# -ge 2 ]] || die "$1 requires a value"; project_root="$2"; shift 2 ;;
      --env) [[ $# -ge 2 ]] || die "$1 requires a value"; env_file="$2"; shift 2 ;;
      -h|--help) usage; return 0 ;;
      *) die "Unknown option: $1" ;;
    esac
  done
  [[ -n "$project_root" && -n "$env_file" ]] || { usage >&2; exit 1; }
  project_root="$(abs_path "$project_root")"; env_file="$(abs_path "$env_file")"
  [[ -f "$project_root/scripts/updater.sh" ]] || die "Updater script not found: $project_root/scripts/updater.sh"
  [[ -f "$project_root/config.env" ]] || die "Config file not found: $project_root/config.env"
  [[ -f "$env_file" ]] || die "Env file not found: $env_file"
  local interval
  interval="$(read_env_value "$env_file" UPDATE_INTERVAL)"; interval="${interval:-1h}"
  cat >"$SERVICE_PATH" <<EOF
[Unit]
Description=BusPCRT Native Updater Service
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
User=root
Group=root
WorkingDirectory=${project_root}
EnvironmentFile=${env_file}
ExecStart=/bin/bash ${project_root}/scripts/updater.sh --config-env-file ${project_root}/config.env --updater-env-file ${env_file}
StandardOutput=journal
StandardError=journal
EOF
  cat >"$TIMER_PATH" <<EOF
[Unit]
Description=Run BusPCRT native updater periodically

[Timer]
OnBootSec=2min
OnUnitActiveSec=${interval}
Persistent=true
Unit=${SERVICE_NAME}

[Install]
WantedBy=timers.target
EOF
  systemctl daemon-reload
  systemctl enable --now "$TIMER_NAME"
  echo "[INFO] Installed $SERVICE_NAME and $TIMER_NAME"
}

uninstall_service() {
  systemctl disable --now "$TIMER_NAME" >/dev/null 2>&1 || true
  systemctl disable --now "$SERVICE_NAME" >/dev/null 2>&1 || true
  rm -f "$TIMER_PATH" "$SERVICE_PATH"
  systemctl daemon-reload
  echo "[INFO] Uninstalled $SERVICE_NAME and $TIMER_NAME"
}
main() { case "${1:-}" in install) require_root; shift; install_service "$@";; uninstall) require_root; uninstall_service;; -h|--help) usage;; *) usage >&2; exit 1;; esac; }
main "$@"
