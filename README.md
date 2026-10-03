# ESP32-P4 Multimodal Biometric Agent Firmware

[![CI](https://github.com/khttang/esp32-biometric-system/actions/workflows/ci.yml/badge.svg)](https://github.com/khttang/esp32-biometric-system/actions/workflows/ci.yml)
![Target](https://img.shields.io/badge/Target-ESP32--P4-red?style=flat-square)
![Version](https://img.shields.io/badge/Version-v0.1.0-blue?style=flat-square)
![ESP-IDF](https://img.shields.io/badge/ESP--IDF-v5.4.4-green?style=flat-square)
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
| Face detection | Working: ESP-DL `human_face_detect` (MSR+MNP), green boxes drawn over the preview |
| Face embedding | Working: ESP-DL `human_face_recognition` (MFN), 512-d, aligned from 5 landmarks |
| Enrollment | Not yet (M2): no templates exist, so matching never fires |
| Template download / matching | Matching implemented and unit-tested; template download is not yet triggered by the state machine |
| Voice recognition | Not implemented |

Known issues:
- **Camera pipeline runs at about 8 fps, not 45.** Each PPA scale of the 1280×960 frame takes 80–95 ms. Cache maintenance and LVGL's PPA use were ruled out as causes. This caps inference at about 4/s.
- **Embedding takes about 187 ms**, against Espressif's published 96 ms for MFN on the P4.
- Occasional full-screen white/cyan flashes (seen on older builds too; suspected display cable or power).
- Deep-sleep wake pins don't match the admin button (see [Power](#power--deep-sleep)).

---

## Hardware

| Component | Part | Interface / Notes |
|---|---|---|
| SoC | ESP32-P4, 2× RISC-V @ 360 MHz | 32 MB PSRAM @ 200 MHz, 16 MB flash |
| Display | HX8394 720×1280 IPS | MIPI-DSI, 2 lanes @ 1000 Mbps, 60 MHz DPI clock, RGB565; rotated 270° in software to 1280×720 landscape |
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
| `cam_pipeline` | 1 | 6 | 8 KB | Rust `pipeline.rs` | Dequeue frame → PPA preview → swap canvas; feed detector |
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
│   ◀── InferenceEvent { FaceSeen | Match(member) } ──────┼───┤   PPA 1280×960 → 640×480 RGB565, letterboxed            │
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
2. **Preview**: the PPA scales the full frame to 640×480 into one of two 640×720 preview buffers
   (black letterbox bars above and below). `p4_ui_present_camera()` swaps the LVGL canvas to that
   buffer under the LVGL lock; the camera thread then fills the other one. If LVGL is busy for more
   than 5 ms, that frame's preview update is skipped.
3. **Inference hand-off**: the inference thread owns a single 640×480 RGB888 detector buffer. It
   requests a frame by handing the buffer back; the camera fills it from the next frame. Inference
   therefore never stalls the preview, always sees a frame at most one sensor period old, and is
   capped at 10 Hz.
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
| Detector | `human_face_detect` 0.4.2, `human_face_detect_msr_s8_v1` + `mnp_s8_v1` |
| Embedder | `human_face_recognition` 0.3.2, `human_face_feat_mfn_s8_v1` (`FEATURE_MODEL_VERSION`) |
| Detector input | 640×480 PPA RGB888 (BGR888 to ESP-DL), full field of view |
| Landmarks | 5 per face; required for alignment |
| Embedding | 512 × `f32`, L2-normalised. A model reporting another length disables recognition (detection keeps running). |

The two components must be upgraded together: `human_face_recognition` 0.3.x requires
`human_face_detect ~0.4.1` (detect 0.5.x is incompatible).

### Measured Performance (ESP32-P4 rev 1.3, 360 MHz)

| Stage | Time | Notes |
|---|---|---|
| Detection (MSR+MNP) | 19–23 ms | Espressif publishes about 17 ms |
| Embedding (MFN, incl. alignment) | 181–195 ms | Espressif publishes about 96 ms; under investigation |
| Camera pipeline | 7–9 fps | PPA preview and detector scaling, 80–95 ms each |
| Inference rate | 3–4 /s | Limited by the camera rate |
| Faces found | up to ~80% of frames | One person in front of the camera, indoor light |

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
   5. Spawn the vision pipeline (camera + inference threads). The inference thread loads both face
      models (`p4_face_init`) before taking its first frame.
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
│   └── biometric-core/               # Hardware-independent logic, host unit tests
│       └── src/
│           ├── geometry.rs           # PixelFormat, Rect, ImageRef, crop / letterbox math
│           ├── contract.rs           # Model contract (input format, landmarks, embedding size)
│           ├── matching.rs           # GroupMember, best_match, cosine similarity
│           └── stats.rs              # Allocation-free latency statistics
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
│   │   ├── face_inference.cpp        # ESP-DL face detection + embedding adapter
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
│       ├── ppa.rs                    # PPA client, DMA buffers
│       ├── audio_worker.rs           # I2S mic capture thread
│       ├── speaker.rs                # I2S speaker output (chime)
│       └── power.rs                  # Inactivity watchdog, deep sleep, boot crash counter
└── docs/ROADMAP.md                    # Milestones
```

### Flash Layout (`firmware/partitions.csv`, 16 MB)

| Name | Type | Offset | Size |
|---|---|---|---|
| nvs | data/nvs | `0x9000` | 64 KB |
| otadata | data/ota | `0x19000` | 8 KB |
| phy_init | data/phy | `0x1B000` | 4 KB |
| ota_0 | app | `0x20000` | 6 MB |
| ota_1 | app | `0x620000` | 6 MB |
| model | data/spiffs | `0xC20000` | 2 MB (reserved for M3) |
| storage | data/spiffs | `0xE20000` | 1.875 MB |

The partition table lives at the default `0x8000`, where `espflash` writes it and the app reads it.
The app slots hold the firmware plus the embedded face models: about 5.1 MB, 81% of a slot.

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
| Python | 3.12 | `brew install python@3.12`. **Path is hardcoded** as `PYTHON=/opt/homebrew/bin/python3.12` in `firmware/.cargo/config.toml`; adjust for your machine. |
| ESP-IDF | v5.4.4 | Downloaded by `esp-idf-sys` into `firmware/.embuild/` on the first build |
| CMake, Ninja, RISC-V GCC 14.2, esp-clang, ROM ELFs | n/a | Installed by `esp-idf-sys` into `firmware/.embuild/espressif/tools/` |
| ESP-IDF managed components (esp-dl, esp_video, LVGL, esp_lvgl_port, HX8394, GT911, …) | see `components_esp32p4.lock` | Fetched by the IDF Component Manager from `idf_component.yml` |

Rust bindings for the C++ component, the PPA driver and the heap API are generated by `esp-idf-sys`
from `components/biometrics_wrapper/include/bindings.h` (`bindings_header` in `firmware/Cargo.toml`)
and used as `crate::ffi`.

---

## Build, Flash & Test

```sh
# Host unit tests (no board needed)
cd crates/biometric-core
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# Firmware: always use the release profile
cd firmware
cargo build --release          # first build downloads ESP-IDF + tools (needs network, takes a while)
cargo run --release            # build, flash (espflash, partitions.csv) and open the serial monitor
cargo fmt --check && cargo clippy --release -- -D warnings   # same checks as CI
```

- **CI** (`.github/workflows/ci.yml`) runs on every pull request and push to `main`:
  - rustfmt, clippy (`-D warnings`) and unit tests for `biometric-core` on Rust 1.99.0;
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
- **Models** come from the `human_face_detect` / `human_face_recognition` components and are embedded
  at build time; the variants are chosen in `sdkconfig.defaults`.
- **After changing `partitions.csv`**, erase the chip once: `espflash erase-flash`, then
  `cargo run --release`. Stale data from the old layout can otherwise be misread.

---

## License

Internal proprietary firmware developed for the ESP32-P4 Biometric Hardware Agent system.
