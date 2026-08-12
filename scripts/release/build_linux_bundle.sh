#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
OUTPUT_DIR="${PROJECT_ROOT}/release/linux"

die() {
  echo "[ERROR] $*" >&2
  exit 1
}

log_info() {
  echo "[INFO] $*"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"
}

copy_file() {
  local source="$1"
  [[ -f "$source" ]] || die "Required file not found: $source"
  cp -f "$source" "$OUTPUT_DIR/"
}

copy_binary() {
  local name="$1"
  local source="${PROJECT_ROOT}/target/release/${name}"
  [[ -x "$source" ]] || die "Release binary not found or not executable: $source"
  cp -f "$source" "$OUTPUT_DIR/${name}"
}

for command in cargo cp date find mkdir rm; do
  require_command "$command"
done

log_info "Building release workspace"
cargo build --release --workspace --manifest-path "${PROJECT_ROOT}/Cargo.toml"

log_info "Preparing ${OUTPUT_DIR}"
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"

for binary in \
  pcrt-door-gateway \
  pcrt-processor \
  pcrt-recorder \
  pcrt-uploader \
  pcrt-license-tool; do
  copy_binary "$binary"
done

for config in config.env door_gateway.env processor.env uploader.env updater.env modem-watchdog.env; do
  copy_file "${PROJECT_ROOT}/${config}"
done

shopt -s nullglob
recorder_configs=("${PROJECT_ROOT}"/recorder-cam*.env)
(( ${#recorder_configs[@]} > 0 )) || die "No recorder-cam*.env files found"
for config in "${recorder_configs[@]}"; do
  copy_file "$config"
done
shopt -u nullglob

[[ -d "${PROJECT_ROOT}/models" ]] || die "Models directory not found"
cp -a "${PROJECT_ROOT}/models" "${OUTPUT_DIR}/models"

[[ -d "${PROJECT_ROOT}/scripts" ]] || die "Scripts directory not found"
cp -a "${PROJECT_ROOT}/scripts" "${OUTPUT_DIR}/scripts"

for binary in "${OUTPUT_DIR}"/pcrt-*; do
  chmod 0755 "$binary"
done
find "${OUTPUT_DIR}/scripts" -type f -name '*.sh' -exec chmod 0755 {} +

commit="unknown"
if command -v git >/dev/null 2>&1 && git -C "$PROJECT_ROOT" rev-parse --verify HEAD >/dev/null 2>&1; then
  commit="$(git -C "$PROJECT_ROOT" rev-parse --short HEAD)"
fi

cat >"${OUTPUT_DIR}/RELEASE" <<EOF
product=pcrt
platform=linux
commit=${commit}
built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF

log_info "Linux bundle ready: ${OUTPUT_DIR}"
