# ESP32-P4 Multimodal Biometric Agent Firmware

[![CI](https://github.com/khttang/esp32-biometric-system/actions/workflows/ci.yml/badge.svg)](https://github.com/khttang/esp32-biometric-system/actions/workflows/ci.yml)
![Target](https://img.shields.io/badge/Target-ESP32--P4-red?style=flat-square)
![Version](https://img.shields.io/badge/Version-v0.1.0-blue?style=flat-square)
![ESP-IDF](https://img.shields.io/badge/ESP--IDF-v5.5.5-green?style=flat-square)
![LVGL](https://img.shields.io/badge/UI-LVGL%209-orange?style=flat-square)
![Rust](https://img.shields.io/badge/Rust-1.99-brightgreen?style=flat-square)
![Language](https://img.shields.io/badge/Language-Rust%20%7C%20C%2B%2B-brightgreen?style=flat-square)

Edge-AI biometrics firmware for the Waveshare **ESP32-P4-NANO** (dual-core RISC-V). It streams an
OV5647 camera through the ISP and Pixel-Processing Accelerator (PPA) to a MIPI-DSI display, runs
a face-embedding model with ESP-DL, and drives GT911 touch, I2S audio and RMII Ethernet.

The application, real-time pipeline and state machine are written in **Rust**
(`esp-idf-svc` / `esp-idf-hal`). A thin **C++** component handles what Rust can't reach directly:
LVGL, the V4L2 camera ioctls, panel bring-up and ESP-DL inference.

---

## Project Status

Planned work is tracked milestone by milestone in [docs/ROADMAP.md](docs/ROADMAP.md).

| Area | State |
|---|---|
| Display, touch, camera preview, Ethernet, I2S audio | Working |
| Vision pipeline (camera → PPA → preview / inference threads) | Working |
| Face detection | Working: ESP-DL MSR+MNP models, green boxes drawn over the preview |
| Face embedding | Working: ESP-DL MobileFaceNet, 512-d, aligned from 5 landmarks |
| Model updates | Models live in their own flash partitions with a verified manifest; update them without reflashing the firmware ([Model Partitions](#model-partitions)) |
| Enrollment | Not yet (M2): no templates exist, so matching never fires |
| Template download / matching | Matching implemented and unit-tested; template download is not yet triggered by the state machine |
| Voice recognition | Not implemented |

Known issues:
- **Camera pipeline runs at about 9–10 fps, not 45.** Scaling the 1280×960 frame for the preview takes about 95 ms. The system is PSRAM-bandwidth-bound: the ISP, PPA, display scan-out, LVGL and code all share it (see [Measured Performance](#measured-performance-esp32-p4-rev-13-360-mhz)).
- **Embedding takes about 187 ms**, against Espressif's published 96 ms for MFN on the P4.
- Occasional full-screen white/cyan flashes (seen on older builds too; suspected display cable or power).
- Deep-sleep wake pins don't match the admin button (see [Power](#power--deep-sleep)).

---

## Hardware

| Component | Part | Interface / Notes |
|---|---|---|
| SoC | ESP32-P4 (silicon v1.3), 2× RISC-V @ 360 MHz | 32 MB PSRAM @ 200 MHz, 16 MB flash |
| Display | HX8394 720×1280 IPS | MIPI-DSI, 2 lanes @ 700 Mbps, 58 MHz DPI clock (the HX8394 driver's recommended timing, about 55 Hz), RGB565; rotated 270° in software to 1280×720 landscape |
| Touch | GT911 | I2C `0x5D`, polled at ~66 Hz (INT pin not used) |
| Camera | OV5647 | MIPI-CSI, RAW10 1280×960 @ 45 fps (binning) → ISP → RGB565 via `esp_video` (`/dev/video0`) |
| Audio | I2S MEMS mic (INMP441-style) + speaker output | `I2S_NUM_0` full-duplex, 16 kHz, 16-bit mono |
| Ethernet | IP101 PHY | RMII, external 50 MHz reference clock, PHY address 1 |
| Panel power / reset | IO expander | I2C `0x45` |

### Pin Map

| Subsystem | Signal | GPIO | Notes |
|---|---|---|---|
| **I2C0** (shared) | SDA / SCL | 7 / 8 | Touch `0x5D`, IO expander `0x45`, camera SCCB; internal pull-ups |
| **Camera** | PWDN / RESET | 5 / 6 | Active low |
| **MIPI PHY power** | LDO channel 3 | n/a | 2.5 V for the DSI PHY |
| **I2S0** | BCLK / WS | 12 / 13 | Driven by the TX channel, shared with RX |
| | DIN (mic) / DOUT (speaker) | 11 / 14 | |
| **Ethernet RMII** | MDC / MDIO | 31 / 52 | MDIO pull-up enabled |
| | REF_CLK (in) / PHY RESET | 50 / 53 | |
| | TX_EN / TXD0 / TXD1 | 49 / 34 / 35 | |
| | CRS_DV / RXD0 / RXD1 | 28 / 29 / 30 | |
| **System** | Admin button | 0 | Input, pull-up, active low |
| **Console** | UART TX / RX | 37 / 38 | ROM/IDF console (USB-serial on the NANO) |

Pin constants live in `BoardPins` in `components/biometrics_wrapper/biometrics_wrapper.cpp`; the
admin button is configured in Rust (`system.rs`).

---

## Software Architecture

### Thread Model

| Task | Core | Priority | Stack | Created by | Role |
|---|---|---|---|---|---|
| `main` | 0 | 1 | 8 KB | ESP-IDF (Rust `main`) | Boot, then state machine loop (~20 ms period) |
| LVGL (`taskLVGL`) | 0 | 4 | 7 KB | `esp_lvgl_port` | Render, sw-rotate 270°, flush to DPI framebuffer; 5 ms timer |
| `gt911_poller` | 1 | 5 | 3 KB | C++ | Poll GT911 every 15 ms |
| `cam_pipeline` | 1 | 6 | 8 KB | Rust `pipeline.rs` | Dequeue frame → PPA preview → (on request) detector image from preview → swap canvas; logs fps/PPA stats every 10 s |
| `inference` | 1 | 3 | 32 KB | Rust `pipeline.rs` | Detect → overlay → embed → match, ≤ 10 Hz (~27 KB of stack never used) |
| audio capture | any | 5 | 4 KB | Rust `audio_worker.rs` | Read I2S mic into a bounded queue |
| inactivity watchdog | any | 5 | 4 KB | Rust `power.rs` | Deep sleep after 180 s without input |
| IDF system tasks | n/a | n/a | n/a | ESP-IDF | Event loop, lwIP, EMAC RX, ISP/CSI drivers |

LVGL is pinned to Core 0 via `port_cfg.task_affinity` in `init_display_system()` (esp_lvgl_port has
no Kconfig option for this), keeping Core 1 free for the vision pipeline.

```text
┌──────────────────────── Core 0 ────────────────────────┐   ┌──────────────────────── Core 1 ────────────────────────┐
│ main (prio 1)                                           │   │ cam_pipeline (prio 6)                                   │
│   BiometricSystem::tick() every ~20 ms                  │   │   Camera::next_frame()  (blocking V4L2 DQBUF)           │
│   ◀── InferenceEvent { FaceSeen | Match(member) } ──────┼───┤   PPA 1280×960 → 640×480 RGB565 preview                 │
│   touch / admin button → InactivityTimer::reset()       │   │   p4_ui_present_camera(back buffer)  (LVGL lock, 5 ms)  │
│                                                         │   │   on request: PPA 1280×960 → 640×480 RGB888 ──┐         │
│ LVGL (prio 4)                                           │   │ inference (prio 3)                             ▼         │
│   render canvas + right panel, rotate 270°, DSI flush   │   │   detect → overlay → align+embed → match ─────┐         │
│                                                         │   │ gt911_poller (prio 5)                          │         │
└─────────────────────────────────────────────────────────┘   └──────────────────────────────────────────────┼─────────┘
                                                                                                             └─▶ events
```

### Vision Pipeline

1. **Capture**: the OV5647 streams RAW10; the ISP converts to RGB565 into three MMAP'd V4L2
   buffers (2.4 MB each, PSRAM). `camera.rs` hands out a `Frame` that borrows a buffer zero-copy and
   re-queues it to the driver when dropped.
2. **Preview**: the PPA scales the full frame to 640×480 into one of two preview buffers.
   - The LVGL camera canvas is exactly this 640×480 image, centred in the 640×720 left column on a
     black screen background.
   - LVGL invalidates the whole canvas on every buffer swap, so keeping the canvas to the image area
     limits per-frame re-rendering and rotation to image pixels.
   - `p4_ui_present_camera()` swaps the canvas to the new buffer under the LVGL lock; the camera
     thread then fills the other one.
   - If LVGL is busy for more than 5 ms, that frame's preview update is skipped.
3. **Inference hand-off**: the inference thread owns a single 640×480 RGB888 detector buffer. It
   requests a frame by handing the buffer back.
   - The camera fills it from the next frame's **preview**: a 640×480 → 640×480 pixel-format
     conversion, about 19 ms. Re-scaling the 1280×960 sensor frame took about 95 ms.
   - The conversion runs before the canvas swap, so LVGL never reads a buffer the PPA is writing.
   - Inference therefore never stalls the preview, always sees a frame at most one sensor period
     old, and is capped at 10 Hz.
   - The sensor buffer is returned to the ISP only after the frame is fully processed. Returning it
     earlier let the ISP stream the next 2.4 MB frame into PSRAM concurrently, which measurably
     slowed every stage.
4. **Detection**: ESP-DL `human_face_detect` (MSR proposals + MNP refinement) returns boxes and 5
   facial landmarks. Boxes are mapped into canvas coordinates and drawn as LVGL overlay objects.
5. **Recognition**: for each face with landmarks, ESP-DL `human_face_recognition` aligns the face to
   112×112 and computes a 512-d, L2-normalised MFN embedding. It is matched by cosine similarity
   (threshold 0.75) against enrolled `GroupMember` templates.

The C++ side is a thin adapter (`face_inference.cpp`). Detection results come back to Rust, and
matching and enrollment stay in Rust (`HumanFaceRecognizer`'s own database is not used).

**Colour order.** The PPA's "RGB888" is ESP-IDF's `color_pixel_rgb888_data_t`, stored **B, G, R** in
memory. That is what ESP-DL calls BGR888. The detector buffer is therefore passed as `BGR888`. Passing
it as RGB888 swaps red and blue; on the device this cut detection to a few percent of frames.

### Model Contract

Defined in `crates/biometric-core/src/contract.rs` and checked at startup:

| Item | Value |
|---|---|
| Detector | `human_face_detect_msr_s8_v1` + `human_face_detect_mnp_s8_v1` (from `human_face_detect` 0.4.2) |
| Embedder | `human_face_feat_mfn_s8_v1` (from `human_face_recognition` 0.3.2) |
| Model ids | `MSR_MODEL_ID`, `MNP_MODEL_ID`, `FEATURE_MODEL_ID`; each partition's manifest must name the expected id |
| Detector input | 640×480 PPA RGB888 (BGR888 to ESP-DL), full field of view |
| Landmarks | 5 per face; required for alignment |
| Embedding | 512 × `f32`, L2-normalised. A model reporting another length disables recognition (detection keeps running). |

The models come from Espressif's `human_face_detect` / `human_face_recognition` releases, but
those components are not part of the firmware build. `face_inference.cpp` builds the models with
the same pre/post-processing parameters, adapted under the MIT licence, so it can load them from
partitions with our own labels.

### Model Partitions

Each model lives in its own flash partition, so a model update needs no firmware rebuild:

```text
offset 0                                    size - 4096           size
├─ .espdl model (`size` bytes) ─ 0xFF padding ─┼─ manifest JSON ─────┤
```

- **Manifest:** in the partition's last 4 KiB sector:
  `{"format":1,"model":"human_face_feat_mfn_s8_v1","version":"human_face_recognition 0.3.2","size":1295200,"sha256":"…"}`.
  The `version` will be recorded with enrolled templates (M2), so templates are only compared with
  embeddings from the same model.
- **Verify before load:** at startup the inference thread memory-maps each partition and checks
  that the manifest names the expected model, the size fits, and the data's SHA-256 matches. Only
  then does ESP-DL load it.
  - ESP-DL itself aborts the chip on an unmappable partition and has no integrity check.
  - Verification takes 22 ms (MSR), 32 ms (MNP) and 376 ms (MobileFaceNet, 1.3 MB).
- **Failure handling, tested on the device:**

  | Partition state | Result |
  |---|---|
  | Embedder data corrupted (one flipped bit) | `model data does not match the manifest's SHA-256`; recognition disabled, detection keeps running |
  | Detector partition erased | `no manifest`; detection disabled, the camera preview keeps running |

  The board never crashes or boot-loops because of a model partition.
- **Single source of truth:** the format lives in `biometric-core::manifest`. It is shared by the
  firmware (`firmware/src/models.rs`) and the host packer (`crates/model-packer`), with host tests
  for every failure mode.

Updating models:

```sh
tools/face-models.sh fetch   # download Espressif's pinned releases, check their SHA-256, extract
tools/face-models.sh pack    # build models/face_*.bin partition images (model-packer)
tools/face-models.sh flash   # espflash write-bin each image at its partition offset
tools/face-models.sh all     # all three steps
```

Model files are not committed: `models/` is gitignored and the releases are pinned by SHA-256 in
the script.

### Measured Performance (ESP32-P4 rev 1.3, 360 MHz)

Measured on ESP-IDF 5.4.4 and re-measured on 5.5.5, with the same results within run-to-run noise.

| Stage | Time | Notes |
|---|---|---|
| Detection (MSR+MNP) | 19–40 ms | Espressif publishes about 17 ms; rises with PSRAM load and face candidates |
| Embedding (MFN, incl. alignment) | 173–178 ms | Espressif publishes about 96 ms |
| Camera pipeline | 9.4–9.7 fps | Preview PPA (1280×960 → 640×480) about 95 ms; detector PPA (from preview) about 19 ms |
| Inference rate | 3.6–4.3 /s | Limited by the camera rate |
| Faces found | up to ~70–80% of frames | One person in front of the camera, indoor light |

**Bottleneck: PSRAM bandwidth.** The ISP (2.4 MB per frame), PPA, display scan-out, LVGL, and code
running from PSRAM all share it. Measured evidence:
- Pausing LVGL canvas updates cut PPA time by 15–30 ms.
- The 800×640 sensor mode reached 21 fps, but it is a centre crop (zoomed in), so it is not used.
- Returning camera buffers early slowed every stage.

Remaining levers:
- YUV420 ISP output (25% less frame data);
- a lower display refresh rate;
- (ESP-IDF 5.5.5's cache/PSRAM fixes were measured: no change.)

These numbers need the ESP-DL-oriented settings in `sdkconfig.defaults`:
- `CONFIG_SPIRAM_XIP_FROM_PSRAM` (run code and models from PSRAM);
- 256 KB L2 cache;
- `-O2`.

Without them, detection measured 28–120 ms and embedding 466–706 ms. Only the RGB888 → RGB888 ESP-DL
pixel conversion is compiled in, which saves about 1.1 MB of code.

All PPA work, DMA-buffer allocation (128-byte aligned PSRAM) and frame lifetimes are owned by Rust
(`ppa.rs`, `camera.rs`, `pipeline.rs`). The PPA driver performs cache maintenance on its input and
output windows itself.

### Boot Sequence

1. `main`: mark the running OTA image valid, initialise logging.
2. `SystemResources::build()`:
   1. Take NVS, the system event loop and the timer service.
   2. `p4_hardware_init_all()` (C++):
      1. I2S duplex audio.
      2. I2C0 bus.
      3. DSI PHY LDO, DSI bus, panel power/reset via IO expander, HX8394 init.
      4. LVGL port + display (2 framebuffers, 270° rotation).
      5. GT911 touch.
      6. Split-screen UI.
      7. `esp_video` + V4L2 stream on.
      8. Ethernet.
   3. Start the audio capture thread; configure the admin button.
   4. Start the inactivity watchdog.
   5. Spawn the vision pipeline (camera + inference threads). Before taking its first frame, the
      inference thread verifies each model partition and loads the detector and embedder
      independently (`p4_face_init_detector` / `p4_face_init_embedder`).
3. State machine loop (`biometrics.rs`):
   `Initialize → DetectionValidation ⇄ ActionExecuted`, plus `RetrieveRuntimeData` /
   `UpdatingRuntimeData` for template sync.

C++ init failures are returned to Rust. In release builds `power::handle_fatal_init_error` retries
up to 3 times (crash counter in RTC RAM), then deep-sleeps for an hour; debug builds panic.

### Power / Deep Sleep

The inactivity watchdog enters deep sleep after 180 s without touch, admin-button or face activity.
Wake sources are LP GPIO 0 and 1 (low level).
**Known issue:** `power.rs` treats GPIO 0 as the GT911 INT and GPIO 1 as the admin button, but the
admin button is on GPIO 0 and the GT911 INT is not wired in this firmware.

---

## Repository Layout

```text
.
├── README.md
├── CLAUDE.md                         # Engineering guidelines for Claude Code
├── crates/
│   ├── biometric-core/               # Hardware-independent logic, host unit tests
│   │   └── src/
│   │       ├── geometry.rs           # PixelFormat, Rect, ImageRef, crop / fit / mapping math
│   │       ├── contract.rs           # Model contract (input, partitions, model ids, embedding size)
│   │       ├── manifest.rs           # Model partition image format: build + verify
│   │       ├── matching.rs           # GroupMember, best_match, cosine similarity
│   │       └── stats.rs              # Allocation-free latency statistics
│   └── model-packer/                 # Host tool: .espdl + manifest -> partition image
├── firmware/                         # ESP32-P4 application (Rust + ESP-IDF)
│   ├── .cargo/config.toml            # Target, build-std, espflash runner, ESP-IDF version
│   ├── rust-toolchain.toml           # Pinned nightly + rust-src
│   ├── Cargo.toml                    # Crates + esp-idf-sys extra component / bindings header
│   ├── build.rs                      # Propagates ESP-IDF link args, links libstdc++
│   ├── sdkconfig.defaults            # ESP-IDF Kconfig (PSRAM, cache, camera, LVGL, Ethernet …)
│   ├── partitions.csv                # Flash layout (see below)
│   ├── components_esp32p4.lock       # Locked ESP-IDF managed component versions
│   ├── components/biometrics_wrapper/      # C++ ESP-IDF component
│   │   ├── biometrics_wrapper.cpp    # Display/LVGL, touch, camera V4L2, audio, Ethernet, OTA
│   │   ├── face_inference.cpp        # ESP-DL face models (MSR, MNP, MobileFaceNet) from partitions
│   │   ├── include/biometrics_wrapper.h    # C API used from Rust
│   │   ├── include/bindings.h        # Headers esp-idf-sys generates Rust bindings from
│   │   ├── idf_component.yml         # Managed component dependencies
│   │   └── CMakeLists.txt
│   └── src/
│       ├── main.rs                   # Entry point, main loop
│       ├── system.rs                 # SystemResources builder, template fetch/cache
│       ├── biometrics.rs             # State machine
│       ├── pipeline.rs               # Camera + inference threads (Core 1)
│       ├── camera.rs                 # V4L2 frame lifetime (RAII)
│       ├── models.rs                 # Verify model partitions before loading
│       ├── ppa.rs                    # PPA client, DMA buffers
│       ├── audio_worker.rs           # I2S mic capture thread
│       ├── speaker.rs                # I2S speaker output (chime)
│       └── power.rs                  # Inactivity watchdog, deep sleep, boot crash counter
├── tools/face-models.sh              # Fetch, package and flash the face models
├── models/                           # (gitignored) downloaded models + partition images
└── docs/ROADMAP.md                    # Milestones
```

### Flash Layout (`firmware/partitions.csv`, 16 MB)

| Name | Type | Offset | Size |
|---|---|---|---|
| nvs | data/nvs | `0x9000` | 64 KB |
| otadata | data/ota | `0x19000` | 8 KB |
| phy_init | data/phy | `0x1B000` | 4 KB |
| ota_0 | app | `0x20000` | 5 MB |
| ota_1 | app | `0x520000` | 5 MB |
| face_msr | data | `0xA20000` | 128 KB (MSR model, 61 KB) |
| face_mnp | data | `0xA40000` | 192 KB (MNP model, 130 KB) |
| face_feat | data | `0xA70000` | 2 MB (MobileFaceNet, 1.3 MB) |
| storage | data/spiffs | `0xC70000` | 3.56 MB |

The partition table lives at the default `0x8000`, where `espflash` writes it and the app reads it.
- The app image is about 3.6 MB, 69% of a slot.
- Model partitions are 64 KiB aligned, because ESP-DL memory-maps them.
- `storage` leaves room to carve out M3b's A/B model slots without moving the app.

---

## Toolchain

The project targets **Rust 1.99** (see `CLAUDE.md`); `rust-version = "1.99"` is set in every crate.
Tested on macOS (Apple Silicon) locally and Ubuntu in CI. Most of the toolchain is fetched
automatically on the first build. Run `rustup toolchain install` once in `firmware/` and in
`crates/biometric-core/` to install the pinned toolchains.

| Tool | Version | How it's installed |
|---|---|---|
| Rust (firmware) | `nightly-2026-07-21` (1.99.0-nightly) + `rust-src`, `rustfmt`, `clippy` | Pinned in `firmware/rust-toolchain.toml`. Nightly is required for `build-std` on `riscv32imafc-esp-espidf` (no `espup` needed for RISC-V). |
| Rust (host tests) | `1.99.0` + `rustfmt`, `clippy` | Pinned in `crates/biometric-core/rust-toolchain.toml` |
| `ldproxy` | latest | `cargo install ldproxy` (linker wrapper used by `.cargo/config.toml`) |
| `espflash` | 4.x | `cargo install espflash` |
| `curl`, `unzip`, `shasum`/`sha256sum` | any | Used by `tools/face-models.sh`; preinstalled on macOS and most Linux distributions |
| Python | 3.12 | `brew install python@3.12`. **Path is hardcoded** as `PYTHON=/opt/homebrew/bin/python3.12` in `firmware/.cargo/config.toml`; adjust for your machine. |
| ESP-IDF | v5.5.5 | Downloaded by `esp-idf-sys` into `firmware/.embuild/` on the first build. The NANO's P4 is silicon v1.x, so `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` is required: ESP-IDF 5.5 otherwise targets v3.01+ and the image crashes at boot. |
| CMake, Ninja, RISC-V GCC 14.2, esp-clang, ROM ELFs | n/a | Installed by `esp-idf-sys` into `firmware/.embuild/espressif/tools/` |
| ESP-IDF managed components (esp-dl, esp_video, LVGL, esp_lvgl_port, HX8394, GT911, …) | see `components_esp32p4.lock` | Fetched by the IDF Component Manager from `idf_component.yml` |

Rust bindings for the C++ component, the PPA driver and the heap API are generated by `esp-idf-sys`
from `components/biometrics_wrapper/include/bindings.h` (`bindings_header` in `firmware/Cargo.toml`)
and used as `crate::ffi`.

---

## Build, Flash & Test

```sh
# Host unit tests (no board needed)
cd crates/biometric-core   # and crates/model-packer
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# Firmware: always use the release profile
cd firmware
cargo build --release          # first build downloads ESP-IDF + tools (needs network, takes a while)
cargo run --release            # build, flash (espflash, partitions.csv) and open the serial monitor
../tools/face-models.sh all     # first time, or after a model change: write the face models
cargo fmt --check && cargo clippy --release -- -D warnings   # same checks as CI
```

- **CI** (`.github/workflows/ci.yml`) runs on every pull request and push to `main`:
  - rustfmt, clippy (`-D warnings`) and unit tests for `biometric-core` and `model-packer` on Rust 1.99.0;
  - rustfmt, clippy and a release build of the firmware.

  ESP-IDF (`firmware/.embuild`, about 5 GB) is cached between runs. The first run is slow.
  CI overrides the macOS `PYTHON` path from `.cargo/config.toml` with the runner's Python.

- **Serial port:** `espflash` prompts when several ports exist. To pick one non-interactively,
  set `ESPFLASH_PORT`, e.g. `ESPFLASH_PORT=/dev/cu.usbmodem5B5E1311931 cargo run --release`.
- **Monitor only:** `espflash monitor --elf target/riscv32imafc-esp-espidf/release/firmware`
  (decodes backtraces using the ELF).
- **Release only:** the debug profile boot-loops with
  `i2c: CONFLICT! driver_ng is not allowed to be used with this old driver`. Without LTO,
  `esp-idf-hal`'s legacy-I2C references pull in ESP-IDF's conflict check. Release builds
  (`lto = true`, `codegen-units = 1`) strip it.
- **After editing C++ or headers**, run `touch sdkconfig.defaults` before `cargo build --release`.
  `esp-idf-sys` only re-runs its CMake build (and regenerates bindings) when that file changes;
  otherwise the old C++ object is silently relinked.
- **Models** are written separately from the firmware (see [Model Partitions](#model-partitions)).
  A board without models boots normally: the preview runs and the log says which models are missing.
- **After changing `partitions.csv`**, erase the chip once: `espflash erase-flash`, then
  `cargo run --release` and `tools/face-models.sh all`. Stale data from the old layout can
  otherwise be misread.

---

## License

Internal proprietary firmware developed for the ESP32-P4 Biometric Hardware Agent system.
