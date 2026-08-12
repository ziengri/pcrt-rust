#!/usr/bin/env bash
set -Eeuo pipefail

log_info() { echo "[INFO] $*"; }
log_warn() { echo "[WARN] $*" >&2; }
die() { echo "[ERROR] $*" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"; }
abs_path() { realpath "$1" 2>/dev/null || readlink -f "$1"; }

usage() {
  cat <<'EOF'
Usage:
  updater.sh [--config-env-file FILE] [--updater-env-file FILE]
EOF
}

load_env_file() {
  [[ -f "$1" ]] || die "Env file not found: $1"
  set -a
  # shellcheck disable=SC1090
  source "$1"
  set +a
}

unit_exists() {
  systemctl list-unit-files "$1" --no-legend --no-pager 2>/dev/null | awk '{print $1}' | grep -Fxq "$1"
}

discover_units() {
  local unit
  for unit in buspcrt-door-gateway.service buspcrt-processor.service buspcrt-uploader.service buspcrt-modem-watchdog.service; do
    unit_exists "$unit" && echo "$unit"
  done
  systemctl list-unit-files 'buspcrt-recorder@*.service' --no-legend --no-pager 2>/dev/null |
    awk '{print $1}' |
    grep -E '^buspcrt-recorder@[0-9]+\.service$' |
    sort -u || true
}

restore_states() {
  local snapshot="$1" unit active
  while IFS='|' read -r unit active; do
    [[ -n "$unit" ]] || continue
    if [[ "$active" == 1 ]]; then systemctl start "$unit" >/dev/null 2>&1 || true; else systemctl stop "$unit" >/dev/null 2>&1 || true; fi
  done <"$snapshot"
}

restart_units() {
  local snapshot unit
  snapshot="$(mktemp)"
  local -a units=()
  mapfile -t units < <(discover_units)
  for unit in "${units[@]}"; do
    if systemctl is-active --quiet "$unit"; then echo "$unit|1" >>"$snapshot"; else echo "$unit|0" >>"$snapshot"; fi
  done
  for unit in "${units[@]}"; do
    log_info "Restarting: $unit"
    if ! systemctl restart "$unit"; then
      log_warn "Restart failed for $unit; restoring prior active states"
      restore_states "$snapshot"
      rm -f "$snapshot"
      return 1
    fi
  done
  rm -f "$snapshot"
}

main() {
  local config_env_file="config.env" updater_env_file="updater.env"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --config-env-file) [[ $# -ge 2 ]] || die "$1 requires a value"; config_env_file="$2"; shift 2 ;;
      --updater-env-file) [[ $# -ge 2 ]] || die "$1 requires a value"; updater_env_file="$2"; shift 2 ;;
      -h|--help) usage; return 0 ;;
      *) die "Unknown option: $1" ;;
    esac
  done
  require_cmd git; require_cmd systemctl; require_cmd flock
  config_env_file="$(abs_path "$config_env_file")"
  updater_env_file="$(abs_path "$updater_env_file")"
  load_env_file "$config_env_file"
  load_env_file "$updater_env_file"
  [[ "${UPDATE_ENABLED:-1}" == 1 ]] || { log_info "UPDATE_ENABLED=${UPDATE_ENABLED}; updater disabled"; return 0; }
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "Current directory is not a git repository"
  git remote get-url "${UPDATE_REMOTE:-origin}" >/dev/null 2>&1 || die "Git remote not found: ${UPDATE_REMOTE:-origin}"
  exec 9>/run/buspcrt-updater.lock
  flock -n 9 || { log_warn "Updater is already running; exit"; return 0; }
  local remote="${UPDATE_REMOTE:-origin}" branch="${UPDATE_BRANCH:-main}" local_head remote_head
  log_info "Checking updates: remote=$remote branch=$branch"
  git fetch --prune "$remote"
  local_head="$(git rev-parse HEAD)"
  remote_head="$(git rev-parse "$remote/$branch")" || die "Remote branch not found: $remote/$branch"
  [[ "$local_head" != "$remote_head" ]] || { log_info "No updates found"; return 0; }
  [[ -z "$(git status --porcelain)" ]] || die "Refusing update: worktree has local changes"
  log_info "Updating: $local_head -> $remote_head"
  git pull --ff-only "$remote" "$branch"
  restart_units
  log_info "Update applied successfully"
}
main "$@"
