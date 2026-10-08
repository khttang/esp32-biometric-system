#!/usr/bin/env bash
# Fetch, package and flash the face models into their flash partitions.
#
#   tools/face-models.sh fetch          # download Espressif's pinned model releases, verify, extract
#   tools/face-models.sh pack  a|b      # build partition images (model + manifest) with model-packer
#   tools/face-models.sh flash a|b      # write the images with espflash (set ESPFLASH_PORT to choose a port)
#   tools/face-models.sh all   a|b      # fetch + pack + flash
#   tools/face-models.sh keygen         # create the signing key; prints its public key
#
# Models are not part of the firmware build. Each has two flash slots (a and b) and a manifest
# (model id, version, size, SHA-256, golden) that the firmware verifies before loading it.
#
# Which slot: on a new or erased board, use a. To update a model on a board that already runs
# one, use the slot that is NOT in use: the boot log says `[Models] msr: using slot A` (and the
# same for mnp and feat). The firmware gives an image in the other slot a golden run at the next
# boot and switches only if it passes. An image written over the slot in use is loaded without
# that test and leaves nothing to fall back to, which is why there is no default slot.
#
# Set NO_GOLDEN=1 to pack without goldens, e.g. to have the device compute and log them.
#
# Images are signed with the key in MODEL_SIGNING_KEY (default
# ~/.config/esp32-biometric/model-signing.key); the firmware only loads images signed by a key
# in firmware/trusted-model-keys.txt. Create your own key with `tools/face-models.sh keygen`
# and put the public key it prints into that file.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODELS="$ROOT/models"
PARTITIONS="$ROOT/firmware/partitions.csv"
PACKER_DIR="$ROOT/crates/model-packer"
REGISTRY="https://components-file.espressif.com/components/espressif"
SIGNING_KEY="${MODEL_SIGNING_KEY:-$HOME/.config/esp32-biometric/model-signing.key}"

# Pinned model releases: component, version, SHA-256 of the release archive.
RELEASES=(
  "human_face_detect 0.4.2 f5a9b84a64e89fd932d1d4250c341dd26c16d706f7a1ed73e4fd376a8c353da6"
  "human_face_recognition 0.3.2 fdab7b1c65a04aee41ffac103f73ec9074c393b31314c689acaf92e3505df8e9"
)

# Partition (without the slot suffix), model id (= .espdl file stem), release it comes from, and
# the golden: the SHA-256 of the model's outputs in the firmware's golden run, as logged by an
# ESP32-P4 running this firmware's ESP-DL version (see README, Model Partitions).
IMAGES=(
  "face_msr human_face_detect_msr_s8_v1 human_face_detect 0.4.2 f5cc3e078da10f69787d705548d8ff28d66034f5f8b36353715f113809ec9593"
  "face_mnp human_face_detect_mnp_s8_v1 human_face_detect 0.4.2 9127698ed41b7b45e8aadab4b6049fcc5acccce2a4e89c310d33484b5d22aa56"
  "face_feat human_face_feat_mfn_s8_v1 human_face_recognition 0.3.2 48fa61984e4bf6654ff9af55ba441d41addbaacfa6fe0d741dfb05afd9e1beeb"
)

slot_arg() {
  case "${1:-}" in
    a|b) echo "$1" ;;
    "")
      echo "Which slot? Give a or b: a on a new or erased board, otherwise the slot the boot log" >&2
      echo "does not name in \`[Models] ...: using slot A|B\`." >&2
      exit 2 ;;
    *) echo "slot must be a or b, not '$1'" >&2; exit 2 ;;
  esac
}

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
      read -r _ model image_component _ _ <<<"$image"
      if [[ "$image_component" == "$component" ]]; then
        unzip -o -q -j "$archive" "models/p4/$model.espdl" -d "$MODELS/p4"
      fi
    done
    echo "Verified $component $version"
  done
}

keygen() {
  (cd "$PACKER_DIR" && cargo run --quiet --release -- keygen --out "$SIGNING_KEY")
  echo "Secret key written to $SIGNING_KEY; add the public key above to firmware/trusted-model-keys.txt" >&2
}

require_key() {
  if [[ ! -f "$SIGNING_KEY" ]]; then
    echo "No signing key at $SIGNING_KEY: run tools/face-models.sh keygen (or set MODEL_SIGNING_KEY)" >&2
    exit 1
  fi
}

pack() {
  local slot; slot="$(slot_arg "${1:-}")"
  require_key
  for image in "${IMAGES[@]}"; do
    read -r base model component version golden <<<"$image"
    partition="${base}_${slot}"
    golden_args=(--golden "$golden")
    if [[ -n "${NO_GOLDEN:-}" ]]; then golden_args=(); fi
    # Run from the crate directory so rustup selects its pinned toolchain (rust-toolchain.toml).
    (cd "$PACKER_DIR" && cargo run --quiet --release -- \
      --input "$MODELS/p4/$model.espdl" --model "$model" --version "$component $version" \
      --label "$partition" --partitions "$PARTITIONS" --out "$MODELS/$partition.bin" \
      --key "$SIGNING_KEY" \
      ${golden_args[@]+"${golden_args[@]}"})
  done
}

flash() {
  local slot; slot="$(slot_arg "${1:-}")"
  for image in "${IMAGES[@]}"; do
    read -r base _ <<<"$image"
    partition="${base}_${slot}"
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
  pack) pack "${2:-}" ;;
  flash) flash "${2:-}" ;;
  all) slot_arg "${2:-}" >/dev/null && fetch && pack "${2:-}" && flash "${2:-}" ;;
  keygen) keygen ;;
  *) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
