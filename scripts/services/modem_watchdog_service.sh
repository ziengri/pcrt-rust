#!/usr/bin/env bash
set -euo pipefail

UNIT_NAME="buspcrt-modem-watchdog.service"
UNIT_PATH="/etc/systemd/system/${UNIT_NAME}"

log_info() {
  echo "[INFO] $*"
}

log_error() {
  echo "[ERROR] $*" >&2
}

require_root() {
  if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    log_error "Run as root (or via sudo)."
    exit 1
  fi
}

abs_path() {
  local p="$1"
  if command -v realpath >/dev/null 2>&1; then
    realpath "$p"
  else
    readlink -f "$p"
  fi
}

usage() {
  cat <<'EOF'
Usage:
  modem_watchdog_service.sh install --project-root <path> --env <modem-watchdog.env>
  modem_watchdog_service.sh uninstall
EOF
}

install_service() {
  local project_root="" env_file=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --project-root) project_root="$2"; shift 2 ;;
      --env) env_file="$2"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) log_error "Unknown option: $1"; usage; exit 1 ;;
    esac
  done

  [[ -n "$project_root" ]] || { log_error "--project-root is required"; exit 1; }
  [[ -n "$env_file" ]] || { log_error "--env is required"; exit 1; }

  project_root="$(abs_path "$project_root")"
  env_file="$(abs_path "$env_file")"

  [[ -d "$project_root" ]] || { log_error "Project root not found: $project_root"; exit 1; }
  [[ -f "$env_file" ]] || { log_error "Env file not found: $env_file"; exit 1; }
  [[ -f "$project_root/scripts/modem_watchdog.sh" ]] || { log_error "Watchdog script not found: $project_root/scripts/modem_watchdog.sh"; exit 1; }

  log_info "Writing unit: $UNIT_PATH"
  cat >"$UNIT_PATH" <<EOF
[Unit]
Description=BusPCRT Modem Watchdog Service
After=NetworkManager.service
Wants=NetworkManager.service

[Service]
Type=simple
User=root
Group=root
WorkingDirectory=${project_root}
EnvironmentFile=${env_file}
ExecStart=/bin/bash ${project_root}/scripts/modem_watchdog.sh --env-file ${env_file}
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

  log_info "Reloading systemd"
  systemctl daemon-reload
  log_info "Enabling and starting ${UNIT_NAME}"
  systemctl enable --now "$UNIT_NAME"
  log_info "Done. Check: systemctl status ${UNIT_NAME}"
}

uninstall_service() {
  log_info "Stopping and disabling ${UNIT_NAME} (if exists)"
  systemctl disable --now "$UNIT_NAME" >/dev/null 2>&1 || true

  if [[ -f "$UNIT_PATH" ]]; then
    log_info "Removing unit file: $UNIT_PATH"
    rm -f "$UNIT_PATH"
  fi

  log_info "Reloading systemd"
  systemctl daemon-reload
  log_info "Uninstalled ${UNIT_NAME}"
}

main() {
  require_root

  local action="${1:-}"
  if [[ -z "$action" ]]; then
    usage
    exit 1
  fi
  shift || true

  case "$action" in
    install) install_service "$@" ;;
    uninstall) uninstall_service "$@" ;;
    -h|--help) usage ;;
    *) log_error "Unknown action: $action"; usage; exit 1 ;;
  esac
}

main "$@"
