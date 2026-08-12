#!/usr/bin/env bash
set -euo pipefail

UNIT_NAME="buspcrt-door-gateway.service"
UNIT_PATH="/etc/systemd/system/${UNIT_NAME}"

die() { echo "[ERROR] $*" >&2; exit 1; }
require_root() { [[ "${EUID:-$(id -u)}" -eq 0 ]] || die "Run as root (or via sudo)."; }
abs_path() { realpath "$1" 2>/dev/null || readlink -f "$1"; }

usage() {
  cat <<'EOF'
Usage:
  door_gateway_service.sh install --project-root PATH --env FILE [--binary FILE]
  door_gateway_service.sh uninstall
EOF
}

install_service() {
  local project_root="" env_file="" binary=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --project-root) [[ $# -ge 2 ]] || die "$1 requires a value"; project_root="$2"; shift 2 ;;
      --env) [[ $# -ge 2 ]] || die "$1 requires a value"; env_file="$2"; shift 2 ;;
      --binary) [[ $# -ge 2 ]] || die "$1 requires a value"; binary="$2"; shift 2 ;;
      -h|--help) usage; return 0 ;;
      *) die "Unknown option: $1" ;;
    esac
  done
  [[ -n "$project_root" && -n "$env_file" ]] || { usage >&2; exit 1; }
  project_root="$(abs_path "$project_root")"
  env_file="$(abs_path "$env_file")"
  binary="${binary:-$project_root/target/release/pcrt-door-gateway}"
  [[ -d "$project_root" ]] || die "Project root not found: $project_root"
  [[ -f "$env_file" ]] || die "Env file not found: $env_file"
  [[ -x "$binary" ]] || die "Gateway binary is not executable: $binary"

  cat >"$UNIT_PATH" <<EOF
[Unit]
Description=BusPCRT Door Gateway Service
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
WorkingDirectory=${project_root}
EnvironmentFile=${env_file}
RuntimeDirectory=pcrt
RuntimeDirectoryMode=0755
ExecStart=${binary} --config-env-file ${project_root}/config.env --env-file ${env_file} --device-env-file /etc/pcrt/device.env
Restart=on-failure
RestartSec=1
TimeoutStopSec=15
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable --now "$UNIT_NAME"
  echo "[INFO] Installed $UNIT_NAME"
}

uninstall_service() {
  systemctl disable --now "$UNIT_NAME" >/dev/null 2>&1 || true
  rm -f "$UNIT_PATH"
  systemctl daemon-reload
  echo "[INFO] Uninstalled $UNIT_NAME"
}

main() {
  case "${1:-}" in
    install) require_root; shift; install_service "$@" ;;
    uninstall) require_root; uninstall_service ;;
    -h|--help) usage ;;
    *) usage >&2; exit 1 ;;
  esac
}
main "$@"
