#!/usr/bin/env bash
set -euo pipefail

TEMPLATE_UNIT_NAME="buspcrt-recorder@.service"
TEMPLATE_UNIT_PATH="/etc/systemd/system/${TEMPLATE_UNIT_NAME}"
INSTANCE_PREFIX="buspcrt-recorder@"
die() { echo "[ERROR] $*" >&2; exit 1; }
require_root() { [[ "${EUID:-$(id -u)}" -eq 0 ]] || die "Run as root (or via sudo)."; }
abs_path() { realpath "$1" 2>/dev/null || readlink -f "$1"; }
unit_name() { echo "${INSTANCE_PREFIX}$1.service"; }
dropin_dir() { echo "/etc/systemd/system/${INSTANCE_PREFIX}$1.service.d"; }
usage() { echo "Usage: recorder_service.sh install --instance ID --project-root PATH --env FILE [--binary FILE] | uninstall --instance ID"; }

install_service() {
  local instance="" project_root="" env_file="" binary=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --instance) [[ $# -ge 2 ]] || die "$1 requires a value"; instance="$2"; shift 2 ;;
      --project-root) [[ $# -ge 2 ]] || die "$1 requires a value"; project_root="$2"; shift 2 ;;
      --env) [[ $# -ge 2 ]] || die "$1 requires a value"; env_file="$2"; shift 2 ;;
      --binary) [[ $# -ge 2 ]] || die "$1 requires a value"; binary="$2"; shift 2 ;;
      -h|--help) usage; return 0 ;;
      *) die "Unknown option: $1" ;;
    esac
  done
  [[ "$instance" =~ ^[0-9]+$ ]] || die "--instance must be numeric"
  [[ -n "$project_root" && -n "$env_file" ]] || { usage >&2; exit 1; }
  project_root="$(abs_path "$project_root")"; env_file="$(abs_path "$env_file")"
  binary="${binary:-$project_root/target/release/pcrt-recorder}"
  [[ -f "$env_file" ]] || die "Env file not found: $env_file"
  [[ -x "$binary" ]] || die "Recorder binary is not executable: $binary"
  cat >"$TEMPLATE_UNIT_PATH" <<'EOF'
[Unit]
Description=BusPCRT Recorder Service (%i)
After=network-online.target buspcrt-door-gateway.service
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
ExecStart=/bin/false
Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF
  local dir unit
  dir="$(dropin_dir "$instance")"; unit="$(unit_name "$instance")"
  mkdir -p "$dir"
  cat >"$dir/override.conf" <<EOF
[Service]
WorkingDirectory=${project_root}
EnvironmentFile=${env_file}
StateDirectory=pcrt
ExecStart=
ExecStart=${binary} --config-env-file ${project_root}/config.env --env-file ${env_file}
EOF
  systemctl daemon-reload; systemctl enable --now "$unit"
  echo "[INFO] Installed $unit"
}

uninstall_service() {
  local instance=""
  while [[ $# -gt 0 ]]; do
    case "$1" in --instance) [[ $# -ge 2 ]] || die "$1 requires a value"; instance="$2"; shift 2;; -h|--help) usage; return 0;; *) die "Unknown option: $1";; esac
  done
  [[ "$instance" =~ ^[0-9]+$ ]] || die "--instance must be numeric"
  systemctl disable --now "$(unit_name "$instance")" >/dev/null 2>&1 || true
  rm -rf "$(dropin_dir "$instance")"
  systemctl daemon-reload
  echo "[INFO] Uninstalled $(unit_name "$instance")"
}

main() { case "${1:-}" in install) require_root; shift; install_service "$@";; uninstall) require_root; shift; uninstall_service "$@";; -h|--help) usage;; *) usage >&2; exit 1;; esac; }
main "$@"
