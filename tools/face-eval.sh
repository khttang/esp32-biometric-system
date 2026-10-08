#!/usr/bin/env bash
# Measure face-recognition accuracy on the board with a public dataset (see README, Measuring
# Accuracy on the Board).
#
#   tools/face-eval.sh fetch      # download the LFW dataset, verify, extract
#   tools/face-eval.sh flash      # flash the evaluation firmware and the candidate feature model
#   tools/face-eval.sh capture    # send the images to the board, store the embeddings it returns
#   tools/face-eval.sh report     # false accept / false reject rates per feature model
#   tools/face-eval.sh restore    # flash the regular firmware and partition table again
#
# The board compares its active feature model with a candidate (CANDIDATE, default Espressif's
# larger MBF model; CANDIDATE=none evaluates the active model alone). Run
# `tools/face-models.sh all a` first: the evaluation uses the models in the regular slots.
#
# Environment: PORT (serial port, default: the only USB serial port found), PEOPLE and
# PER_PERSON (selection size, default 400 and 4), RESULTS (default models/eval/results).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EVAL="$ROOT/models/eval"
FIRMWARE="$ROOT/firmware"
ELF="$FIRMWARE/target/riscv32imafc-esp-espidf/release/firmware"
PARTITIONS="$FIRMWARE/partitions-eval.csv"
RESULTS="${RESULTS:-$EVAL/results}"

# Labeled Faces in the Wild (http://vis-www.cs.umass.edu/lfw/), from the mirror scikit-learn uses.
LFW_URL="https://ndownloader.figshare.com/files/5976018"
LFW_SHA256="055f7d9c632d7370e6fb4afc7468d40f970c34a80d4c6f50ffec63f5a8d536c0"

# Candidate feature model: model id (= .espdl file stem) in the release archive that
# tools/face-models.sh downloads and verifies.
CANDIDATE="${CANDIDATE:-human_face_feat_mbf_s8_v1}"
CANDIDATE_RELEASE="human_face_recognition 0.3.2"
CANDIDATE_ARCHIVE="$ROOT/models/archives/espressif__human_face_recognition-v0.3.2.zip"

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

port() {
  if [[ -n "${PORT:-}" ]]; then echo "$PORT"; return; fi
  local found=()
  for candidate in /dev/cu.usbmodem* /dev/cu.usbserial* /dev/cu.wchusbserial* /dev/ttyUSB* /dev/ttyACM*; do
    [[ -e "$candidate" ]] && found+=("$candidate")
  done
  if [[ ${#found[@]} -ne 1 ]]; then
    echo "Set PORT to the board's serial port (found: ${found[*]:-none})" >&2
    exit 1
  fi
  echo "${found[0]}"
}

face_eval() {
  # Run from the crate directory so rustup selects its pinned toolchain (rust-toolchain.toml).
  (cd "$ROOT/crates/face-eval" && cargo run --quiet --release -- "$@")
}

fetch() {
  mkdir -p "$EVAL"
  local archive="$EVAL/lfw.tgz"
  if [[ ! -f "$archive" ]]; then
    echo "Downloading LFW (about 180 MB)"
    curl --fail --silent --show-error --location -o "$archive.tmp" "$LFW_URL"
    mv "$archive.tmp" "$archive"
  fi
  local actual; actual="$(sha256 "$archive")"
  if [[ "$actual" != "$LFW_SHA256" ]]; then
    echo "SHA-256 mismatch for $archive: expected $LFW_SHA256, got $actual" >&2
    exit 1
  fi
  tar -xzf "$archive" -C "$EVAL"
  echo "LFW extracted to $EVAL/lfw"
}

flash() {
  local offset
  offset="$(awk -F, '$1 ~ "^eval_feat[[:space:]]*$" { gsub(/[[:space:]]/, "", $4); print $4 }' "$PARTITIONS")"
  if [[ "$CANDIDATE" == "none" ]]; then
    # An erased partition has no manifest, so the firmware runs the active model alone.
    espflash erase-region "$offset" 0x1000
  else
    if [[ ! -f "$CANDIDATE_ARCHIVE" ]]; then
      echo "Run tools/face-models.sh fetch first ($CANDIDATE_ARCHIVE is missing)" >&2
      exit 1
    fi
    unzip -o -q -j "$CANDIDATE_ARCHIVE" "models/p4/$CANDIDATE.espdl" -d "$ROOT/models/p4"
    (cd "$ROOT/crates/model-packer" && cargo run --quiet --release -- \
      --input "$ROOT/models/p4/$CANDIDATE.espdl" --model "$CANDIDATE" --version "$CANDIDATE_RELEASE" \
      --label eval_feat --partitions "$PARTITIONS" --out "$EVAL/eval_feat.bin" \
      --key "${MODEL_SIGNING_KEY:-$HOME/.config/esp32-biometric/model-signing.key}")
    echo "Writing eval_feat at $offset"
    espflash write-bin "$offset" "$EVAL/eval_feat.bin"
  fi
  (cd "$FIRMWARE" && cargo build --release --features eval)
  espflash flash --partition-table "$PARTITIONS" --non-interactive "$ELF"
}

restore() {
  (cd "$FIRMWARE" && cargo build --release)
  espflash flash --partition-table "$FIRMWARE/partitions.csv" --non-interactive "$ELF"
}

case "${1:-}" in
  fetch) fetch ;;
  flash) flash ;;
  capture)
    face_eval capture --port "$(port)" --dataset "$EVAL/lfw" --out "$RESULTS" \
      --people "${PEOPLE:-400}" --per-person "${PER_PERSON:-4}" ;;
  report) face_eval report --results "$RESULTS" ;;
  restore) restore ;;
  *) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
