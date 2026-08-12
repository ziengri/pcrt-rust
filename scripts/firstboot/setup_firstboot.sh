#!/usr/bin/env bash
set -Eeuo pipefail

PROJECT_ROOT="${PROJECT_ROOT:-/opt/pcrt}"
BASE_DIR="${PROJECT_ROOT}/scripts/firstboot"
DEVICE_TEMPLATE="${BASE_DIR}/device.env"
DEVICE_TARGET="/etc/pcrt/device.env"
FRPC_TEMPLATE="${BASE_DIR}/frpc.toml"
FRPC_TARGET="/etc/pcrt/frpc.toml"
REVERSE_TEMPLATE="${BASE_DIR}/reverse-tunnel.service.tpl"
REVERSE_TARGET="/etc/systemd/system/reverse-tunnel.service"
INSTALL_SERVICES_SCRIPT="${PROJECT_ROOT}/scripts/services/install_services.sh"
BUS_ID=""
NUMBER_CAMS=""
HOSTNAME_VALUE=""

die() { echo "[ERROR] $*" >&2; exit 1; }
log_info() { echo "[INFO] $*"; }
log_warn() { echo "[WARN] $*" >&2; }
require_root() { [[ "${EUID:-$(id -u)}" -eq 0 ]] || die "Run this script as root."; }
require_file() { [[ -f "$1" ]] || die "File not found: $1"; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"; }
backup_file() { [[ -e "$1" ]] && cp -a "$1" "$1.bak.$(date +%Y%m%d_%H%M%S)"; }
escape_sed_replacement() { printf '%s' "$1" | sed -e 's/[\/&]/\\&/g'; }

ask_bus_id() {
  local value
  while true; do
    read -r -p "Enter bus ID (example: mta230): " value
    value="${value,,}"
    if [[ "$value" =~ ^[a-z]{3}[0-9]{3}$ ]]; then
      BUS_ID="$value"; HOSTNAME_VALUE="bus-${value}"; return
    fi
    log_warn "Use exactly three English letters and three digits, e.g. mta230."
  done
}

ask_number_cams() {
  while true; do
    read -r -p "Enter number of cameras (3 or 4): " NUMBER_CAMS
    [[ "$NUMBER_CAMS" =~ ^(3|4)$ ]] && return
    log_warn "Camera count must be 3 or 4."
  done
}

confirm_apply() {
  printf '\nBus ID: %s\nCameras: %s\nHostname: %s\nProject: %s\n\n' "$BUS_ID" "$NUMBER_CAMS" "$HOSTNAME_VALUE" "$PROJECT_ROOT"
  local answer
  read -r -p "This regenerates machine and SSH host identities. Apply? [y/N]: " answer
  [[ "${answer,,}" =~ ^(y|yes)$ ]] || die "Cancelled."
}

render_template() {
  local template="$1" target="$2" temp
  temp="$(mktemp)"
  sed -e "s/__BUS_ID__/$(escape_sed_replacement "$BUS_ID")/g" -e "s/__NUMBER_CAMS__/${NUMBER_CAMS}/g" "$template" >"$temp"
  mkdir -p "$(dirname "$target")"
  backup_file "$target"
  install -m 0644 "$temp" "$target"
  rm -f "$temp"
  log_info "Written: $target"
}

install_reverse_tunnel() {
  systemctl disable --now reverse-tunnel.service >/dev/null 2>&1 || true
  backup_file "$REVERSE_TARGET"
  install -m 0644 "$REVERSE_TEMPLATE" "$REVERSE_TARGET"
  systemctl daemon-reload
  systemctl enable --now reverse-tunnel.service
}

main() {
  require_root
  for command in sed install systemctl hostnamectl systemd-machine-id-setup ssh-keygen; do require_cmd "$command"; done
  require_file "$DEVICE_TEMPLATE"; require_file "$FRPC_TEMPLATE"; require_file "$REVERSE_TEMPLATE"; require_file "$INSTALL_SERVICES_SCRIPT"
  require_file "$PROJECT_ROOT/RELEASE"
  ask_bus_id; ask_number_cams; confirm_apply

  log_info "Regenerating machine and SSH host identities"
  rm -f /etc/machine-id /var/lib/dbus/machine-id /etc/ssh/ssh_host_* /var/lib/systemd/random-seed
  systemd-machine-id-setup >/dev/null
  ssh-keygen -A >/dev/null
  hostnamectl set-hostname "$HOSTNAME_VALUE"
  render_template "$DEVICE_TEMPLATE" "$DEVICE_TARGET"
  render_template "$FRPC_TEMPLATE" "$FRPC_TARGET"
  install_reverse_tunnel
  bash "$INSTALL_SERVICES_SCRIPT"
  log_info "First boot setup completed for $BUS_ID using release bundle $PROJECT_ROOT"
}
main "$@"
