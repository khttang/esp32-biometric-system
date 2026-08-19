# ESP32-P4 Multimodal Biometric Agent Firmware

![Target Architecture](https://img.shields.io/badge/Target-ESP32--P4-red?style=flat-square)
![Firmware Version](https://img.shields.io/badge/Version-v0.1.0-blue?style=flat-square)
![Framework](https://img.shields.io/badge/Framework-ESP--IDF%20v5.4-green?style=flat-square)
![LVGL](https://img.shields.io/badge/UI-LVGL%20v9-orange?style=flat-square)
![Language](https://img.shields.io/badge/Language-C%2B%2B20%20%7C%20Rust-brightgreen?style=flat-square)

An edge AI biometrics application firmware running on the ESP32-P4 dual-core RISC-V application processor. Integrates real-time video streaming, hardware-accelerated image scaling via PPA, local neural network face embedding extraction using ESP-DL, MIPI-DSI display graphics, GT911 capacitive touch input, full-duplex I2S audio, and RMII Ethernet networking.

---

## Technical Specifications

| Parameter | Specification |
| :--- | :--- |
| **Processor** | ESP32-P4 Dual-Core RISC-V @ up to 400 MHz |
| **Firmware Version** | `v0.1.0` |
| **Development Stack** | ESP-IDF v5.4, C++20, Rust (FFI Wrapper API) |
| **Display Panel** | HX8394 MIPI-DSI 5.5" IPS ($720 \times 1280$, $270^\circ$ Software Rotation) |
| **Touch Controller** | GT911 Capacitive Touch over Shared I2C (`0x5D`) |
| **Camera Module** | OV5647 CSI-2 ($1280 \times 960$ RGB565 via V4L2 `/dev/video0`) |
| **Hardware Scaling** | ESP32-P4 PPA (SRM Engine, 128-byte L2 Cache Line Aligned) |
| **Audio Subsystem** | I2S Full-Duplex $16\text{ kHz}$ Mono, 16-bit PCM |
| **Networking** | IP101 RMII Ethernet (Waveshare ESP32-P4-NANO EMAC) |
| **ML Models** | MobileFaceNet (ESP-DL engine, flash-mapped at `0x40225160`) |

---

## Hardware Architecture & Pinout Map
                 +----------------------------------+
                 |        ESP32-P4 Processor        |
                 +----------------------------------+
                 /    |         |        |         \
                /     |         |        |          \
         MIPI-DSI   I2C0       CSI      I2S       RMII ETH
            |         |         |        |            |
         HX8394     GT911    OV5647    PCM/MEMS    IP101 PHY
         Display    Touch    Camera    Audio       Ethernet

### Pin Allocation Table

| Subsystem | Signal Name | ESP32-P4 GPIO | Configuration / Description |
| :--- | :--- | :--- | :--- |
| **I2C Bus (Shared 0)** | SDA / SCL | `GPIO 7` / `GPIO 8` | Shared I2C master for Touch (`0x5D`), Expander (`0x45`), Camera SCCB |
| **Display (MIPI-DSI)** | PHY Power | LDO Channel 3 | 2.5V power rail for MIPI-DSI PHY |
| **Camera (CSI-2)** | PWDN / RESET | `GPIO 5` / `GPIO 6` | Hardware Power Down and Reset controls (Active LOW) |
| **Audio (I2S0)** | BCLK / WS | `GPIO 12` / `GPIO 13` | Bit Clock & Word Select (Shared master clocks) |
| | DIN / DOUT | `GPIO 11` / `GPIO 14` | Microphone Input / Speaker Output |
| **Ethernet (RMII)** | MDC / MDIO | `GPIO 31` / `GPIO 52` | SMI Management Data Clock / IO |
| | CLK / RESET | `GPIO 50` / `GPIO 53` | RMII External Input Clock / Hardware Reset |
| | RMII Data | `GPIO 28, 29, 30, 34, 35, 49` | CRS_DV, RXD0, RXD1, TXD0, TXD1, TX_EN |
| **System** | ADMIN_BTN | `GPIO 0` | Boot / Administration button |

---

## Software Architecture & Thread Model

### Dual-Core Thread Distribution

* **Core 0 (Application & UI Core):**
  * `main_task`: System bring-up and orchestration.
  * `taskLVGL`: LVGL 9 rendering pipeline, layout calculation, display flush routines, and GT911 input handling.
  * System state machine transitions and high-level biometric business logic.

* **Core 1 (Real-Time Pipeline Core):**
  * `process_camera_frame_task`: Dedicated task for dequeuing V4L2 MMAP buffers, driving the PPA hardware scaling engine, and invalidating the LVGL video canvas.
  * Audio worker threads for MEMS microphone capture and playback.

  Core 0 (OS & UI)                          Core 1 (Real-Time Pipeline)
+------------------------+                +--------------------------+
|  taskLVGL              |                | process_camera_frame_task|
|  - LVGL 9 Render Loop  |                | - V4L2 DQBUF             |
|  - Touch Handling      |                | - PPA SRM Scale (HW)     |
|  - State Machine       |                | - Canvas Invalidate      |
+------------------------+                +--------------------------+
            ^                                           |
            |------------ s_ui_canvas_buf --------------|

### Camera & Video Pipeline Optimization
1. **Sensor Capture:** OV5647 streams raw $1280 \times 960$ RGB565 frames into 3 queued MMAP buffers via `/dev/video0`.
2. **PPA Hardware Scaling:** The Pixel Processing Accelerator (SRM unit) scales $1280 \times 960 \rightarrow 360 \times 640$ directly into PSRAM with **zero CPU instruction overhead**.
3. **Cache Alignment:** Canvas output buffers (`s_ui_canvas_buf`) are allocated using `heap_caps_aligned_alloc()` to meet the **128-byte L2 cache line requirement** (`C_LINE_SIZE = 128`), preventing DMA cache-coherency failures.
4. **Display Flushing:** Uses a double-buffered DPI panel (`num_fbs = 2`) with software $270^\circ$ rotation (`sw_rotate = true`), bypassing panel-level hardware swapping issues.

---

## System Initialization Sequence            

[1. Audio System Init]  --->  [2. Shared I2C0 Bus & PPA Engine]
|
[4. GT911 Touch & UI]   <---  [3. DSI LDO, IO Expander & Panel]
|
[5. V4L2 Camera Task]   --->  [6. RMII Ethernet Driver]
|
[State Machine Active]  <---  [7. Flash Model Mapping (ESP-DL)]

1. **Audio Infrastructure:** Allocates full-duplex standard I2S channels on `I2S_NUM_0` @ 16 kHz.
2. **Hardware Engines:** Spawns `I2C_NUM_0` master bus and registers the PPA client for SRM hardware operations.
3. **Display Subsystem:** Powers MIPI PHY (2.5V LDO), executes IO expander (`0x45`) panel reset sequence, initializes the HX8394 DPI driver with 2 framebuffers, and starts `taskLVGL`.
4. **UI Layout:** Attaches GT911 touch input and builds the split-screen layout safely under `lvgl_port_lock(100)`. Sets `s_ui_ready = true`.
5. **Camera Subsystem:** Powers OV5647 sensor, initializes CSI interface, allocates MMAP buffers on `/dev/video0`, starts the stream, and spawns `process_camera_frame_task` on Core 1.
6. **Network & ML Subsystem:** Resets IP101 PHY, brings up RMII Ethernet, maps MobileFaceNet model weights from flash (`0x40225160`), and transitions system state to `DetectionValidation`.

---

## Directory & File Overview

```text
firmware/
├── components/
│   ├── biometrics_wrapper/
│   │   ├── biometrics_wrapper.cpp   # Core C++ hardware drivers & processing tasks
│   │   └── biometrics_wrapper.h     # C-compatible FFI header exports for Rust
│   └── esp-dl/                      # ESP-DL neural network library
├── main/
│   ├── main.rs                      # Rust application state machine & entry point
│   └── Cargo.toml                   # Rust dependency configuration
├── sdkconfig                        # ESP-IDF configuration settings
└── CMakeLists.txt                   # IDF build system script

Building and Flashing
Prerequisites
ESP-IDF: v5.4 or later configured for esp32p4 target.

Rust Toolchain: esp target toolchain installed via espup.

Build Instructions


License
Internal proprietary firmware developed for the ESP32-P4 Biometric Hardware Agent system.