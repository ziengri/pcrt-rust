#!/usr/bin/env bash
set -Eeuo pipefail

log_info() {
  echo "[INFO] $*"
}

log_warn() {
  echo "[WARN] $*" >&2
}

log_error() {
  echo "[ERROR] $*" >&2
}

require_cmd() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || {
    log_error "Required command not found: $cmd"
    exit 1
  }
}

load_env_file() {
  local path="$1"
  [[ -f "$path" ]] || return 0
  set -a
  # shellcheck disable=SC1090
  source "$path"
  set +a
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
  modem_watchdog.sh [--env-file <path>]

Watches a NetworkManager connection and resets the LTE USB modem via usbreset
whenever the connection is not active. Loops forever.
EOF
}

parse_args() {
  ENV_FILE="modem-watchdog.env"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --env-file) ENV_FILE="$2"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) log_error "Unknown option: $1"; usage; exit 1 ;;
    esac
  done
}

# True (exit 0) when the NetworkManager connection is currently active.
is_connected() {
  nmcli -t -f NAME connection show --active 2>/dev/null \
    | grep -Fxq "$MODEM_CONN_NAME"
}

reset_modem() {
  log_warn "Resetting modem USB ${MODEM_USB_ID} via usbreset"
  local rc=0
  usbreset "$MODEM_USB_ID" || rc=$?
  if [[ "$rc" -eq 0 ]]; then
    log_info "usbreset succeeded for ${MODEM_USB_ID}"
  else
    log_error "usbreset failed for ${MODEM_USB_ID} (rc=${rc})"
  fi
}

main() {
  parse_args "$@"

  ENV_FILE="$(abs_path "$ENV_FILE")"
  load_env_file "$ENV_FILE"

  MODEM_WATCHDOG_ENABLED="${MODEM_WATCHDOG_ENABLED:-1}"
  MODEM_CONN_NAME="${MODEM_CONN_NAME:-sim-internet}"
  MODEM_USB_ID="${MODEM_USB_ID:-12d1:15c1}"
  MODEM_CHECK_INTERVAL="${MODEM_CHECK_INTERVAL:-30}"
  MODEM_RESET_WAIT="${MODEM_RESET_WAIT:-300}"

  if [[ "$MODEM_WATCHDOG_ENABLED" != "1" ]]; then
    log_info "MODEM_WATCHDOG_ENABLED=${MODEM_WATCHDOG_ENABLED}; watchdog disabled, idling"
    # Idle instead of exiting so systemd does not restart-loop the unit.
    exec sleep infinity
  fi

  require_cmd nmcli
  require_cmd usbreset

  log_info "Modem watchdog started: conn='${MODEM_CONN_NAME}' usb=${MODEM_USB_ID} check=${MODEM_CHECK_INTERVAL}s reset_wait=${MODEM_RESET_WAIT}s"

  while true; do
    if is_connected; then
      sleep "$MODEM_CHECK_INTERVAL"
    else
      log_warn "Connection '${MODEM_CONN_NAME}' is not active"
      reset_modem
      log_info "Waiting ${MODEM_RESET_WAIT}s before re-checking"
      sleep "$MODEM_RESET_WAIT"
    fi
  done
}

main "$@"
