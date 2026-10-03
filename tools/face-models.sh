#!/usr/bin/env bash
# Fetch, package and flash the face models into their flash partitions.
#
#   tools/face-models.sh fetch   # download Espressif's pinned model releases, verify, extract
#   tools/face-models.sh pack    # build partition images (model + manifest) with model-packer
#   tools/face-models.sh flash   # write the images with espflash (set ESPFLASH_PORT to choose a port)
#   tools/face-models.sh all     # fetch + pack + flash
#
# Models are not part of the firmware build. Each lives in its own partition with a manifest
# (model id, version, size, SHA-256) that the firmware verifies before loading it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODELS="$ROOT/models"
PARTITIONS="$ROOT/firmware/partitions.csv"
PACKER_DIR="$ROOT/crates/model-packer"
REGISTRY="https://components-file.espressif.com/components/espressif"

# Pinned model releases: component, version, SHA-256 of the release archive.
RELEASES=(
  "human_face_detect 0.4.2 f5a9b84a64e89fd932d1d4250c341dd26c16d706f7a1ed73e4fd376a8c353da6"
  "human_face_recognition 0.3.2 fdab7b1c65a04aee41ffac103f73ec9074c393b31314c689acaf92e3505df8e9"
)

# Partition, model id (= .espdl file stem), release it comes from.
IMAGES=(
  "face_msr human_face_detect_msr_s8_v1 human_face_detect 0.4.2"
  "face_mnp human_face_detect_mnp_s8_v1 human_face_detect 0.4.2"
  "face_feat human_face_feat_mfn_s8_v1 human_face_recognition 0.3.2"
)

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

fetch() {
  mkdir -p "$MODELS/archives"
  for release in "${RELEASES[@]}"; do
    read -r component version expected <<<"$release"
    archive="$MODELS/archives/espressif__${component}-v${version}.zip"
    if [[ ! -f "$archive" ]]; then
      echo "Downloading $component $version"
      curl --fail --silent --show-error --location -o "$archive.tmp" \
        "$REGISTRY/$component/$version/espressif__${component}-v${version}.zip"
      mv "$archive.tmp" "$archive"
    fi
    actual="$(sha256 "$archive")"
    if [[ "$actual" != "$expected" ]]; then
      echo "SHA-256 mismatch for $archive: expected $expected, got $actual" >&2
      exit 1
    fi
    # Only the ESP32-P4 models that the firmware uses.
    for image in "${IMAGES[@]}"; do
      read -r _ model image_component _ <<<"$image"
      if [[ "$image_component" == "$component" ]]; then
        unzip -o -q -j "$archive" "models/p4/$model.espdl" -d "$MODELS/p4"
      fi
    done
    echo "Verified $component $version"
  done
}

pack() {
  for image in "${IMAGES[@]}"; do
    read -r partition model component version <<<"$image"
    # Run from the crate directory so rustup selects its pinned toolchain (rust-toolchain.toml).
    (cd "$PACKER_DIR" && cargo run --quiet --release -- \
      --input "$MODELS/p4/$model.espdl" --model "$model" --version "$component $version" \
      --label "$partition" --partitions "$PARTITIONS" --out "$MODELS/$partition.bin")
  done
}

flash() {
  for image in "${IMAGES[@]}"; do
    read -r partition _ <<<"$image"
    offset="$(awk -F, -v p="$partition" '$1 ~ "^"p"[[:space:]]*$" { gsub(/[[:space:]]/, "", $4); print $4 }' "$PARTITIONS")"
    if [[ -z "$offset" ]]; then
      echo "Partition $partition not found in $PARTITIONS" >&2
      exit 1
    fi
    echo "Writing $partition at $offset"
    espflash write-bin "$offset" "$MODELS/$partition.bin"
  done
}

case "${1:-}" in
  fetch) fetch ;;
  pack) pack ;;
  flash) flash ;;
  all) fetch && pack && flash ;;
  *) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
