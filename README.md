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
face detection and face-embedding models with ESP-DL, matches faces against templates enrolled on
the device, and drives GT911 touch and I2S audio.

The application, real-time pipeline and state machine are written in **Rust**
(`esp-idf-svc` / `esp-idf-hal`). A thin **C++** component handles what Rust can't reach directly:
LVGL, the V4L2 camera ioctls, panel bring-up and ESP-DL inference.

---

## Engineering Highlights

Where the embedded-systems work in this repository is, with the section that documents each.

**Real-time and concurrency**
- Sensor-paced camera loop and inference loop pinned to one core, the UI and state machine on the
  other, each with an explicit FreeRTOS priority ([Thread Model](#thread-model)).
- Stages never wait on each other: ownership of one image buffer moves between the two loops
  through bounded channels with `try_send` / `try_recv`, and shared state is read lock-free
  (`ArcSwap`). The camera loop only *tries* the LVGL lock and skips a preview update when it is
  busy ([Vision Pipeline](#vision-pipeline)).
- A preemption bug that made neural-network output differ from run to run was traced to a
  silicon erratum and fixed in the interrupt path; output is now identical in 24 of 24 runs
  ([Hardware-Loop Erratum Workaround](#esp32-p4-hardware-loop-erratum-workaround)).

**Resource constraints**
- The pipeline is limited by PSRAM bandwidth, not CPU. Each stage is timed on the board and the
  design follows the numbers: the detector image is converted from the 640×480 preview (about
  19 ms) and not re-scaled from the sensor frame (about 95 ms)
  ([Measured Performance](#measured-performance-esp32-p4-rev-13-360-mhz)).
- No heap allocation per frame. All seven image buffers, about 7.7 MB, are allocated once at
  start-up, and a third sensor buffer was removed after measuring it
  ([Image Buffers](#image-buffers)).
- Build configuration tuned for inference (code and models executed from PSRAM, 256 KB L2 cache,
  `-O2`): detection went from 28–120 ms to 19–40 ms and embedding from 466–706 ms to about
  175 ms. Compiling in only the one pixel conversion in use saves about 1.1 MB of code.
- 16 MB of flash laid out for A/B model slots that update without reflashing the firmware
  ([Flash Layout](#flash-layout-firmwarepartitionscsv-16-mb), [Model Partitions](#model-partitions)).

**DMA and zero-copy data paths**
- Camera frames are borrowed zero-copy from the driver's memory-mapped buffers, which the ISP
  fills by DMA, and are returned to the driver only when the frame has been fully processed.
- Scaling and colour conversion run on the Pixel-Processing Accelerator into PSRAM buffers
  aligned to the 128-byte cache line, as its DMA output requires. Buffer ownership and lifetimes
  are expressed in Rust types (`firmware/src/ppa.rs`, `camera.rs`).

**Instrumentation and measurement**
- Each pipeline thread accumulates its own timings and prints one summary line every 10 seconds,
  with no allocation and nothing shared between threads
  ([Built-in Instrumentation](#built-in-instrumentation)).
- Design choices are made from those lines and recorded with the numbers: the detector image
  source (95 ms → 19 ms), two sensor buffers instead of three, the LVGL try-lock (1 preview
  update skipped in about 480).
- Recognition accuracy is measured on the board itself, by sending a public dataset through the
  same models over the serial port ([Measuring Accuracy on the Board](#measuring-accuracy-on-the-board)).

**Register-level and hardware bring-up**
- RISC-V assembly in the interrupt entry and exit paths reads and writes the CPU's
  hardware-loop and vector-extension control registers (CSRs). It runs from IRAM and is attached
  with the linker's `--wrap`, so ESP-IDF itself is unmodified
  (`firmware/components/biometrics_wrapper/hwlp_erratum.S`).
- GT911 touch controller polled through its status, point and clear registers over I2C.
- MIPI-DSI panel bring-up: lane bit rate, pixel clock and porch timings
  (`biometrics_wrapper.cpp`).
- Safe Rust over a thin C API: every `unsafe` block is the size of one FFI call and carries a
  `// Safety:` comment.

---

## Project Status

As of 2026-10-07. Milestone details are in [docs/ROADMAP.md](docs/ROADMAP.md).

**Target state.** A stand-alone device that recognises an enrolled person, greets them by name
and holds a spoken conversation through a remote voice agent over Wi-Fi, with face models that
can be updated in the field without reflashing the firmware.

**Current state.** The device detects, enrolls and recognises faces on its own. When it
recognises an enrolled person it plays a chime and shows "Welcome, [name]" on the LCD for
three seconds. Models are signed and live in A/B flash slots. It has no network connection in
use and no speech.

### Current State

| Area | State |
|---|---|
| Display, touch, camera preview | Working |
| Network | None in use. The Ethernet driver is still started at boot but nothing uses it; it is to be removed when Wi-Fi is added. Wi-Fi through the on-board ESP32-C6 was proven in a spike and is not in this firmware |
| Audio | On-board codec configured; a chime plays at boot and on a match; the microphone is captured and its level logged. Nothing consumes the microphone audio yet |
| Vision pipeline (camera → PPA → preview / inference threads) | Working |
| Face detection | Working: ESP-DL MSR+MNP models, green boxes drawn over the preview |
| Face embedding | Working: ESP-DL MobileFaceNet, 512-d, aligned from 5 landmarks |
| Model updates | Each model has two flash slots (A/B) with a signed, verified manifest; the firmware loads only images signed by a trusted key. A new image written to the standby slot is activated only after a golden run passes, and the previous image is kept for rollback; no firmware reflash ([Model Partitions](#model-partitions)) |
| Enrollment | On-device: the admin view on the touch panel enrolls the face in view and deletes members; templates persist in flash ([Enrollment & Templates](#enrollment--templates)) |
| Matching | Cosine similarity against enrolled templates of the same model release. Enroll → reboot → recognise → delete works on the board. The threshold (0.5) is Espressif's default; on a public dataset it accepted no impostor pair and rejected 21% of genuine single-image pairs; not yet measured with this camera on non-enrolled people ([Measuring Accuracy on the Board](#measuring-accuracy-on-the-board)) |
| Template sync between devices | Dropped from the plan. Templates exist only on the device that enrolled them |
| Speech | Not implemented. Planned: a spoken greeting, then a conversation with a remote voice agent. Recognising people by voice is not planned |
| Remote model updates, telemetry | Not implemented; parked until the device has a network connection |

Known issues:
- **Camera pipeline runs at about 9–10 fps.** Scaling the 1280×960 frame for the preview takes about 80–90 ms. The system is PSRAM-bandwidth-bound: the ISP, PPA, display scan-out, LVGL and code all share it (see [Measured Performance](#measured-performance-esp32-p4-rev-13-360-mhz)).
- **Embedding takes about 170–180 ms**, against Espressif's published 96 ms for MFN on the P4.
- Deep-sleep wake pins don't match the admin button (see [Power](#power--deep-sleep)).
- **Templates are stored unencrypted** and the admin view is open to anyone at the device (see [Enrollment & Templates](#enrollment--templates)).
- **Core 1 is not exclusively the vision pipeline.** The touch poller is pinned to it at a priority above the inference thread, and the audio-capture and inactivity threads are unpinned, so they can run there too ([Thread Model](#thread-model)).
- The inference thread waits up to 10 ms for the LVGL lock to draw the face boxes; only the camera thread never waits for it.
- Unused today: the 1 MB `storage` partition, the `p4_perform_ota_update` function (nothing calls it) and the Ethernet driver.
- Enrollment, deletion through the touch panel and a golden run have not been re-tested on the builds of the open pull requests (#23–#25); the pipeline, detection and matching were.

### Plan to the Target State

In this order. Each step is one pull request ([docs/ROADMAP.md](docs/ROADMAP.md), M6).

| Step | What it adds | State |
|---|---|---|
| 1. Wi-Fi | Wi-Fi through the ESP32-C6: credentials entered on the touch panel, reconnect, status on screen; Ethernet removed | Next. The link itself was proven in a spike |
| 2. Audio path | A consumer for the microphone audio and playback of streamed audio | Planned |
| 3. Spoken greeting | "Welcome, [name]" on a match | Planned |
| 4. Conversation | A speech-to-speech agent, started by a face match | Planned; provider not chosen |
| 5. Hardening | Reconnects, session and cost limits, handling of the API key | Planned |
| Later | Remote model updates with template migration (M4d); telemetry (M5) | Parked until Wi-Fi exists |

Open decisions: the voice provider and where its API key lives; the match threshold (0.5 today,
about 0.4 under consideration after the accuracy run); who may enroll (an admin role exists in the template
format but nothing checks it); encryption at rest, which needs an eFuse key burned.

---

## Hardware

| Component | Part | Interface / Notes |
|---|---|---|
| SoC | ESP32-P4 (silicon v1.3), 2× RISC-V @ 360 MHz | 32 MB PSRAM @ 200 MHz, 16 MB flash |
| Display | HX8394 720×1280 IPS | MIPI-DSI, 2 lanes @ 700 Mbps, 58 MHz DPI clock (the HX8394 driver's recommended timing, about 55 Hz), RGB565; rotated 270° in software to 1280×720 landscape |
| Touch | GT911 | I2C `0x5D`, polled at ~66 Hz (INT pin not used) |
| Camera | OV5647 | MIPI-CSI, RAW10 1280×960 @ 45 fps (binning) → ISP → RGB565 via `esp_video` (`/dev/video0`) |
| Audio | On-board ES8311 codec, microphone and NS4150B speaker amplifier | `I2S_NUM_0` full-duplex, 16 kHz, 16-bit mono; codec configured over I2C0 (`0x18`) with `esp_codec_dev` |
| Ethernet | IP101 PHY | RMII, external 50 MHz reference clock, PHY address 1 |
| Panel power / reset | IO expander | I2C `0x45` |

### Pin Map

| Subsystem | Signal | GPIO | Notes |
|---|---|---|---|
| **I2C0** (shared) | SDA / SCL | 7 / 8 | Touch `0x5D`, IO expander `0x45`, audio codec `0x18`, camera SCCB; internal pull-ups |
| **Camera** | PWDN / RESET | 5 / 6 | Active low |
| **MIPI PHY power** | LDO channel 3 | n/a | 2.5 V for the DSI PHY |
| **I2S0** (ES8311 codec) | MCLK / BCLK / WS | 13 / 12 / 10 | Shared by both directions |
| | DOUT (to the codec's DAC) / DIN (from its ADC) | 9 / 11 | |
| | Amplifier enable | 53 | Active high; driven by the codec driver |
| **Ethernet RMII** | MDC / MDIO | 31 / 52 | MDIO pull-up enabled |
| | REF_CLK (in) | 50 | The PHY is reset over MDIO, not by a pin |
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
| `cam_pipeline` | 1 | 6 | 8 KB | Rust `pipeline.rs` | Dequeue frame → PPA preview → (on request) detector image from preview → swap canvas (LVGL try-lock, skipped when LVGL is busy); logs fps, PPA and swap stats every 10 s |
| `inference` | 1 | 3 (highest during a golden run) | 32 KB | Rust `pipeline.rs` | Detect → overlay → embed → match, ≤ 10 Hz (~23 KB of stack never used). At start-up, a golden run of a new model image runs at the highest priority, so `cam_pipeline` and the preview pause for its duration |
| audio capture | any | 5 | 8 KB | Rust `audio_worker.rs` | Read the microphone into a bounded queue; log its level every 10 s |
| inactivity watchdog | any | 5 | 4 KB | Rust `power.rs` | Deep sleep after 180 s without input |
| IDF system tasks | n/a | n/a | n/a | ESP-IDF | Event loop, lwIP, EMAC RX, ISP/CSI drivers |

LVGL is pinned to Core 0 via `port_cfg.task_affinity` in `init_display_system()` (esp_lvgl_port has
no Kconfig option for this), keeping Core 1 free for the vision pipeline.

```text
┌──────────────────────── Core 0 ────────────────────────┐   ┌──────────────────────── Core 1 ────────────────────────┐
│ main (prio 1)                                           │   │ cam_pipeline (prio 6)                                   │
│   BiometricSystem::tick() every ~20 ms                  │   │   Camera::next_frame()  (blocking V4L2 DQBUF)           │
│   ◀── InferenceEvent { FaceSeen | Match(member) } ──────┼───┤   PPA 1280×960 → 640×480 RGB565 preview                 │
│   touch / admin button → InactivityTimer::reset()       │   │   p4_ui_present_camera(back buffer)  (LVGL try-lock)    │
│                                                         │   │   on request: PPA preview → 640×480 RGB888 ───┐         │
│ LVGL (prio 4)                                           │   │ inference (prio 3)                             ▼         │
│   render canvas + right panel, rotate 270°, DSI flush   │   │   detect → overlay → align+embed → match ─────┐         │
│                                                         │   │ gt911_poller (prio 5)                          │         │
└─────────────────────────────────────────────────────────┘   └──────────────────────────────────────────────┼─────────┘
                                                                                                             └─▶ events
```

### Vision Pipeline

1. **Capture**: the OV5647 streams RAW10; the ISP converts to RGB565 into two MMAP'd V4L2
   buffers (2.4 MB each, PSRAM). `camera.rs` hands out a `Frame` that borrows a buffer zero-copy and
   re-queues it to the driver when dropped. The camera loop holds one buffer while the driver
   fills the other; a third buffer was measured and dropped (see
   [Image Buffers](#image-buffers)).
2. **Preview**: the PPA scales the full frame to 640×480 into one of two preview buffers.
   - The LVGL camera canvas is exactly this 640×480 image, centred in the 640×720 left column on a
     black screen background.
   - LVGL invalidates the whole canvas on every buffer swap, so keeping the canvas to the image area
     limits per-frame re-rendering and rotation to image pixels.
   - `p4_ui_present_camera()` swaps the canvas to the new buffer under the LVGL lock; the camera
     thread then fills the other one.
   - The camera thread does not wait for that lock: if LVGL holds it, that frame's preview
     update is skipped (about 0.2 % of frames, measured with an idle scene). The camera log
     line reports the time spent in the swap and the number of skipped swaps.
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
   (threshold 0.5) against the enrolled `GroupMember` templates that were produced by the same
   model release. During an enrollment the embeddings are collected into a new template instead.

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

Each model lives in flash partitions of its own, so a model update needs no firmware rebuild:

```text
offset 0                                    size - 4096                    size
├─ .espdl model (`size` bytes) ─ 0xFF padding ─┼─ manifest JSON ─ signature ─┤
```

- **Manifest:** in the partition's last 4 KiB sector:
  `{"format":2,"model":"human_face_feat_mfn_s8_v1","version":"human_face_recognition 0.3.2","size":1295200,"sha256":"…","golden_sha256":"…"}`.
  The `version` is recorded with enrolled templates, so templates are only compared with
  embeddings from the same model. Only format 2 is accepted; format 1 (no `golden_sha256`)
  predates signing, so no loadable format 1 image exists.
- **Signature:** the sector's last 68 bytes hold `SIG1` and an Ed25519 signature over the
  manifest JSON. The manifest contains the data's hash, so the signature covers the whole image
  ([Signed Models and the Trust Model](#signed-models-and-the-trust-model)).
- **Verify before load:** at startup the inference thread memory-maps each partition and checks
  that the manifest is signed by a trusted key, names the expected model, the size fits, and the
  data's SHA-256 matches. Only then does ESP-DL load it.
  - The signature is checked over the stored bytes before they are parsed, so an unauthenticated
    manifest never reaches the JSON parser.
  - ESP-DL itself aborts the chip on an unmappable partition and has no integrity check.
  - Verification takes 30 ms (MSR), 60 ms (MNP) and 405 ms (MobileFaceNet, 1.3 MB) per slot,
    of which the signature check is roughly 10–25 ms. Both slots of every model are verified at
    each boot, about 1 s in total when all are filled.
- **A model can never take the device down:** if neither slot of a model holds a usable image,
  only the stage that needs it is disabled. Tested on the device with the single-slot layout: a
  corrupted embedder left detection running, and an erased detector left the preview running.
- **Single source of truth:** the format lives in `biometric-core::manifest`. It is shared by the
  firmware (`firmware/src/models.rs`) and the host packer (`crates/model-packer`), with host tests
  for every failure mode.

#### Signed Models and the Trust Model

A SHA-256 shows that a model image is intact. It does not show who made it: whoever can write a
model can write its hash too. So every image is signed, and the firmware loads only images signed
by a key it trusts.

- **Who signs:** whoever holds the secret key. `tools/face-models.sh keygen` creates one in
  `~/.config/esp32-biometric/model-signing.key` (or `MODEL_SIGNING_KEY`), readable only by its
  owner, and prints the public key. The secret key stays on the signing machine: never on a
  device, never in the repository.
- **How devices get keys:** the public keys are listed in `firmware/trusted-model-keys.txt` and
  compiled into the firmware. A device trusts exactly the keys of the firmware it runs.
- **Building for your own devices:** run `keygen`, replace the key in `trusted-model-keys.txt`
  with yours, then build the firmware and run `tools/face-models.sh all`. Images signed with the
  key committed here can only be produced by this project's maintainer.
- **Rotating a key:** add the new public key, release firmware, re-sign the models with the new
  key, and remove the old key in a later firmware release. Several keys can be trusted at once.
- **No bypass:** an unsigned image is rejected like a corrupt one, and there is no build option
  to allow it. The slot is skipped; if neither slot of a model is usable, only that stage is
  disabled.

Checked on the board, one slot each: an unsigned image, an image signed with another key, and a
signed image whose manifest was changed afterwards (the version, with the model data intact)
were all rejected, and the properly signed images in the other slots loaded.

What signing does **not** protect against:
- **Physical access.** Without secure boot, anyone with the serial cable can flash a firmware
  that trusts a different key, or none. Signing protects the path by which models reach the
  device (the remote updates of M4d), not the device itself. See [Encryption at Rest](#encryption-at-rest-not-enabled).
- **A stolen secret key.** Whoever has it can publish models until the key is rotated out.
- **Replay.** An older image with a valid signature is still accepted. Rejecting downgrades is
  planned with remote updates (M4d).
- **A bad model signed in good faith.** The signature says who published it, not that it works;
  the golden run below and the accuracy evaluation cover that.

The signature scheme is in `biometric-core::signing` (Ed25519 via `ed25519-dalek`, pure Rust), so
the packer and the firmware run the same code, with host tests for wrong keys, changed manifests,
moved signatures and corrupt trailers. Each signature is made over a purpose string plus the
manifest, so a signature made for one purpose cannot be reused for another.

#### A/B Slots, Golden Run and Rollback

Every model has two slots (`face_msr_a`/`_b`, `face_mnp_a`/`_b`, `face_feat_a`/`_b`). One is
active; an update is written to the other, the standby slot. Nothing else has to be told: at boot
the firmware sees an image in the standby slot that it has not evaluated and gives it a trial.

Only the standby slot is watched. An image written over the *active* slot is loaded without a
trial, and so is the first image on a device with no saved state, because the state does not
record which image the active slot held. Updates must therefore go to the standby slot.

1. **Golden run.** The candidate is loaded, run once on a fixed pseudo-random input, and the
   SHA-256 of its output tensors is compared with the manifest's `golden_sha256`. The SHA-256 of
   the file proves the bytes are intact; the golden run shows that *this firmware's* ESP-DL build
   can load the model and computes what the publisher's did (it would catch a model that needs
   operators or memory the firmware lacks).
2. **Pass:** the candidate becomes the active slot. The old image stays in the other slot as the
   *previous* image.
3. **Fail** (digest differs, no golden, or the data is not a runnable model): the candidate is
   marked *rejected* and never tried again; the active slot stays in use.
4. **Rollback:** if the active slot later fails verification, the firmware switches back to the
   previous image. It never falls back to a rejected one.
5. **Crash protection:** the state is written to NVS *before* the golden run. If the device
   resets during a trial, the next boot sees the marker; after two unfinished trials of the same
   image it is rejected. Two attempts are allowed so that a power cut does not condemn a good
   model.

The state per model (active slot, the verdict on the standby image, the trial marker) is a 68-byte
record in the `models` NVS namespace. The decision logic is `biometric_core::activation`, a pure
state machine with host tests; `firmware/src/models.rs` feeds it and runs the trial. An image is
identified by a hash over its manifest fields, so the same model repackaged with another version
or golden counts as a new image, and re-writing an identical image does nothing.

Tested on the device (feature model unless noted):

| Situation | Result |
|---|---|
| Upgrade from the single-slot layout, standby slots empty | Slot A in use, no reflash of the models needed |
| New images with goldens written to slot B (all three models) | Golden runs pass (59 / 54 / 355 ms); slot B active; no trial on later boots |
| Image without a golden in the standby slot | Not activated; the log gives the digest this device computes |
| Image with a wrong golden | `golden mismatch`; rejected; not retried on the next boot |
| Random data with a valid manifest and SHA-256 | `does not hold a runnable model`; rejected (ESP-DL did not crash) |
| Active slot corrupted (4 KiB overwritten) | `model data does not match the manifest's SHA-256`; rolled back to the previous image in slot A |
| Identical image written to the standby slot | Nothing happens |

Not tested on the device: a reset in the middle of a trial (covered by host tests only), and a
switch to a genuinely different model (only Espressif's one release of each was available, so the
"new" images differed in version label and golden only).

**The golden is exact and belongs to a firmware generation.** Outputs are quantised integers, so
the digest is compared bit for bit. It must come from a device running the same ESP-DL version;
after an ESP-DL upgrade the goldens of the pinned models have to be re-measured. The golden run
executes at the highest task priority, because a preempted run of the feature model is not
repeatable without the erratum workaround below, and a false mismatch would reject a good model for good.

Updating models:

```sh
tools/face-models.sh fetch      # download Espressif's pinned releases, check their SHA-256, extract
tools/face-models.sh pack  b    # build models/face_*_b.bin partition images (model-packer)
tools/face-models.sh flash b    # espflash write-bin each image at its partition offset
tools/face-models.sh all        # all three steps; the slot defaults to a
tools/face-models.sh keygen     # once: create the signing key, print its public key
```

To publish a model of your own, get its golden from a device: pack it without `--golden`
(`NO_GOLDEN=1` with the script), write it to the standby slot, and boot. The log shows
`image has no golden and is not activated; this device computes <digest>`. Pack it again with
`--golden <digest>`.

Model files are not committed: `models/` is gitignored and the releases are pinned by SHA-256 in
the script, together with their goldens.

### ESP32-P4 Hardware-Loop Erratum Workaround

ESP-DL's assembly kernels use the P4's hardware-loop registers and its PIE vector extension, with
PIE instructions inside hardware loops. On silicon before v3 (this board is v1.3) a preempted
inference could resume with a loop one iteration short. Nothing crashes; the result is just
slightly wrong. Measured with the feature model on a fixed input:

| Inference thread | Result |
|---|---|
| Highest priority (never preempted) | Identical output in every run |
| Normal priority, camera thread preempting it | A different output in most runs, a few quantisation steps apart |
| Normal priority, with the workaround | Identical output in 24 of 24 runs over three boots |

`firmware/components/biometrics_wrapper/hwlp_erratum.S` wraps FreeRTOS's interrupt entry and
exit routines (linker `--wrap`, set in `firmware/.cargo/config.toml`; ESP-IDF is not modified):

1. **Entry:** the CPU does not always flag a running hardware loop, and ESP-IDF v5.5.5 only saves
   the loop registers when it is flagged ([espressif/esp-idf#19025](https://github.com/espressif/esp-idf/issues/19025)).
   The wrapper flags a loop whose counter is not zero, so the registers are saved.
2. **Exit:** after a context switch ESP-IDF leaves PIE off and lets the task's next PIE
   instruction trap to switch it back on. A trap taken inside a hardware loop is what costs the
   iteration. The wrapper switches PIE back on directly when the resumed task still owns the
   core's PIE registers, so there is no trap.

What the measurements do and do not show:
- The entry half alone did not make the output repeatable. Entry plus exit did. The exit half on
  its own was not tested, so whether the entry half is needed for this symptom is not known; it
  is kept because the unsaved-register case was observed (39 interrupt entries with an unflagged
  running loop during one boot).
- The effect on recognition has not been measured: no before/after comparison of match scores.
- Two PIE-using tasks on the same core are not covered (the one that lost ownership still
  traps). This firmware has at most one per core.

Remove the file and the linker options once the ESP-IDF version in use fixes both paths.

### Enrollment & Templates

Members are enrolled on the device itself; no server is involved.

**Flow.** The right-hand panel shows a status line and an **Admin** button (the admin button on
GPIO 0 does the same). The admin view has a name field and the member list on the left,
**Enroll**, **Delete** and **Done** on the right, and an on-screen keyboard along the bottom:

1. **Enroll** asks the inference thread for a template. It needs exactly one face in view (with
   several, there is no telling whose it should be).
2. The inference thread averages 5 embeddings (`biometric_core::enrollment`). Each sample must
   reach the match threshold against the mean of the earlier ones, so a second person stepping
   in is not averaged into the template. The panel shows the progress.
3. The main thread rejects the template if it already matches a member ("Already enrolled as …").
   Otherwise it stores it and publishes the new member list to the matcher (`ArcSwap`, no lock on
   the inference path).
4. An enrollment that has not finished after 15 s is abandoned. The admin view closes after 60 s
   without a touch.

An empty name becomes `Member <n>`. Ids are `<device>-<n>`, for example `80f1b2d2da2e-0004`:
the device part is the board's factory MAC address, so ids of members enrolled on different
devices never collide, and `n` is a counter kept in flash, so an id is never reused after a
deletion. All members enrolled on the device have the `USER` role.

**Members and templates.** A member (id, name, role) is separate from its templates. A template
is one embedding for one modality (face today; the format already has a value for voice) and one
model release. A member holds at most one template per modality and release, and may hold none.

**Bound to the model release.** Each template records the `version` from the feature model's
manifest ([Model Partitions](#model-partitions)). Matching uses only a member's face template for
the release that is loaded: an embedding from one model means nothing to another. After a model
update a member whose template is for the old release stays on the device but cannot be
recognised, and the log says how many members that is.

**Enrolling again.** To give such a member a template for the new release, type their exact name
and press **Enroll**: the new template is added to that member instead of creating a second
person. A name only selects a member who has no face template for the loaded release, so it
cannot be used to overwrite someone who is already recognisable.

**Template format v2** (`biometric_core::template`). Two kinds of binary record:
- a member record: 8-byte header (magic `FMBR`, format version, role, field lengths), then the id
  and name as UTF-8;
- a template record: 12-byte header (magic `FTPL`, format version, modality, field lengths,
  embedding dimension), then the member's id and the model version as UTF-8 and the embedding as
  little-endian `f32`. That is about 2.1 KB for a 512-d embedding.

Decoding is strict: wrong magic or version, lengths that disagree with the header, invalid UTF-8
and embeddings that are not finite unit vectors are all rejected. A template names its member, so a record makes sense on
its own.

**Storage** (`firmware/src/templates.rs`). Records are NVS blobs in the `templates` partition, a
separate NVS partition so that erasing the system `nvs` leaves enrollments alone. A member
occupies a slot `NN` (up to 32): `mNN` is the member record, `tNN_0` and `tNN_1` its templates.
Two templates per member are enough for a face template for the model in use plus one for a model
being introduced; more (voice) needs a larger partition. NVS provides wear levelling, a CRC per
entry and power-fail-safe single writes. Across the several writes of an enrollment or deletion,
the member record is the commit point: templates are written before it and erased after it, and
template blobs without a member are removed at the next start.

**Format v1 is no longer read.** Format v1 kept one face embedding inside the member record
(`tplNN`, ids `local-<n>`). The start-up conversion to the current format was removed; a `tplNN`
record still in flash is ignored, and that member has to be enrolled again.

**Threshold.** `MATCH_THRESHOLD` is 0.5, the default of Espressif's `HumanFaceRecognizer` for this
model. [Measuring Accuracy on the Board](#measuring-accuracy-on-the-board) reports what that
threshold does on a public dataset. It has not been measured with this camera on people who are
not enrolled; the one live data point is a single enrolled person, who scored 0.32–0.87 per frame
(0.77 on average). The inference log line reports the similarity of each embedded face to its
closest template every 10 s, so live scores can be compared with the dataset's:

```text
… | closest template n=42 min 0.61 avg 0.74 max 0.83 (threshold 0.5)
```

**Limitations.**
- Templates are stored **unencrypted**. They are biometric data: anyone who can read the flash
  can read them. See [Encryption at Rest](#encryption-at-rest-not-enabled) for why and what it
  takes to change that.
- Deleting a member erases its NVS entries, but NVS only marks them as erased: the bytes stay in
  flash until that page is recycled.
- The admin list does not show which members need to be enrolled again; only the log does.
- Re-enrollment selects the member by exact name; two members with the same name cannot be told
  apart that way.
- Anyone at the device can open the admin view; there is no admin authentication yet.
- There is no liveness check: a photo of an enrolled person matches.
- Roles are stored but not used for anything yet.

### Encryption at Rest (not enabled)

Encrypting the templates in flash needs a key that an attacker with the flash contents cannot
read, and on this chip that means an eFuse, which is burned once and cannot be changed:

- **NVS encryption** encrypts an NVS partition with keys that are themselves protected either by
  flash encryption or by the chip's HMAC peripheral with an eFuse key. Both need an eFuse.
  With the HMAC scheme, ESP-IDF v5.5.5 generates and **burns the eFuse key by itself at the first
  boot** if none is present (`nvs_sec_provider.c`), so merely enabling `CONFIG_NVS_ENCRYPTION` on
  a board is already the permanent step.
- **What it would protect:** reading the flash chip, or dumping it over the serial port.
- **What it would not protect:** without secure boot, someone can flash a firmware that asks the
  chip to decrypt the templates. Full protection is NVS encryption plus secure boot plus flash
  encryption, after which every firmware must be signed.

This project's development board keeps its eFuses untouched, so none of this is enabled and it
has not been tested here. `CONFIG_NVS_ENCRYPTION` must stay off until that decision is made for a
given device.

### Measuring Accuracy on the Board

How often does the device accept the wrong person, or reject the right one? That depends on the
quantised models as they run on this chip, so it is measured on the board, not on a PC:

1. `tools/face-eval.sh flash` flashes the firmware built with the `eval` cargo feature. That
   build does not start the camera; it loads the models and waits on the console UART.
2. `tools/face-eval.sh capture` (host tool `crates/face-eval`) sends images of the
   [LFW](http://vis-www.cs.umass.edu/lfw/) dataset to the board, one at a time. For each image
   the board runs the same detector and feature models as in normal operation, on the largest
   face it finds, and returns the embeddings.
3. `tools/face-eval.sh report` compares every pair of embeddings on the host: two images of the
   same person are a *genuine* pair, images of two different people an *impostor* pair.
4. `tools/face-eval.sh restore` flashes the regular firmware again.

Two rates describe a threshold: the **false accept rate** is the share of impostor pairs that
score at or above it, and the **false reject rate** is the share of genuine pairs that score
below it. Raising the threshold trades false accepts for false rejects; the **equal error rate**
is where the two meet.

The evaluation build can load a second, *candidate* feature model and run it on the same
detections, so two models are compared like for like. The candidate lives in `eval_feat`, a
partition that `firmware/partitions-eval.csv` puts in place of the unused second firmware slot.

**Results** (ESP32-P4 rev 1.3, ESP-IDF v5.5.5, the pinned model releases; 400 LFW people with two
or more images, up to four images each, chosen at even intervals through the sorted names):

- 1,189 images sent; a face was detected in 1,181 (8 without).
- 1,317 genuine pairs and 695,473 impostor pairs.
- The run took about 48 minutes, 2.4 s per image, most of it the transfer.

| Metric | MFN (`human_face_feat_mfn_s8_v1`, in use) | MBF (`human_face_feat_mbf_s8_v1`, candidate) |
|---|---|---|
| Equal error rate | 1.07% | 0.76% |
| False accepts at the current threshold (0.5) | none in 695,473 pairs | none in 695,473 pairs |
| False rejects at the current threshold (0.5) | 21.2% | 36.7% |
| Highest impostor score | 0.444 | 0.374 |
| Threshold for 1% false accepts → false rejects there | 0.245 → 1.2% | 0.174 → 0.6% |
| Threshold for 0.1% false accepts → false rejects there | 0.323 → 2.7% | 0.233 → 1.5% |
| Threshold for 0.01% false accepts → false rejects there | 0.381 → 5.4% | 0.281 → 3.9% |
| Mean genuine / impostor score | 0.592 / 0.026 | 0.534 / 0.027 |
| Model size / one run on the board | 1.3 MB / 176 ms | 3.5 MB / 326 ms |

What this shows:
- **0.5 is a cautious threshold for MFN.** No impostor pair reached it, but one genuine pair in
  five fell below it when a single image is compared with a single image.
- **Each model needs its own threshold.** MBF scores lower overall, so MFN's threshold would
  reject far more genuine pairs with MBF. A threshold belongs to a model release, like a template.
- **MBF is somewhat more accurate.** At equal false accept rates it rejects fewer genuine pairs
  (3.9% against 5.4% at 0.01%), for nearly twice the inference time and a 3.5 MB image, which does
  not fit the 2 MB feature slots of the regular layout.

`MATCH_THRESHOLD` is unchanged by this measurement.

**How it works.** The host sends a 16-byte header (magic, width, height, length, CRC-32) and the
pixels as packed B, G, R bytes at 921,600 baud; the board answers with one text line carrying the
detector's score and each model's embedding as hex, with its own CRC-32
(`biometric_core::eval_protocol`). A text line survives next to log output on the same UART. The
metrics are in `biometric_core::evaluation`. Results are written as they arrive, so an
interrupted capture resumes where it stopped.

**What the numbers do and do not say.**
- LFW images are press photos of public figures: varied pose, lighting and age, but not taken
  with this device's camera, and each is sent as a 250 × 250 image, not as a 640 × 480 frame.
- Each comparison is one image against one image. The device compares a live frame with a
  template averaged from five samples, which should do better, but that is not measured here.
- The smallest false accept rate that can be measured is one over the number of impostor pairs.
- The `eval` feature is for the bench: it accepts images from anyone on the serial port. Never
  flash it on a deployed device.

### Built-in Instrumentation

The firmware reports on itself over the serial log; no debugger or external probe is attached.
This is local logging. Reporting from devices in the field is a later milestone (M5) and is not
implemented.

| What | Reported | Where |
|---|---|---|
| Camera loop | Frames per second; preview and detector PPA time (average, longest); canvas-swap time; preview updates skipped because LVGL was busy | `pipeline.rs`, every 10 s |
| Inference loop | Inferences per second; detection and embedding time (average, longest); frames with a face; lowest, average and highest similarity to the closest template, next to the threshold | `pipeline.rs`, every 10 s |
| Inference stack | Bytes of the 32 KB stack never used, after the deepest call path has run | `pipeline.rs`, once |
| Microphone | Peak and RMS level | `audio_worker.rs`, every 10 s |
| Model activation | Duration and result of each golden run | `models.rs`, when a new image is tried |

```text
[Pipeline] camera 9.9 fps over 10s | preview PPA avg 79.2 ms max 105.7 ms | detector PPA n=49 avg 18.7 ms max 19.2 ms | present avg 0.1 ms max 0.1 ms, 0 of 99 skipped (LVGL busy)
[Pipeline] inference 4.7/s over 10s | detect avg 35.0 ms max 42.9 ms | embed n=11 avg 168.5 ms max 173.5 ms | frames with faces 11 | closest template n=11 min 0.42 avg 0.49 max 0.64 (threshold 0.5)
```

- **Cheap enough to leave on.** A sample is a counter, a sum and a maximum, kept in a local
  variable of the thread that measures it (`LatencyStats`, `ScoreStats` and `AudioLevel` in
  `crates/biometric-core/src/stats.rs`, with host unit tests). Nothing is allocated or locked per
  frame.
- **Used for tuning.** The similarity figures are what a threshold change is judged by, and the
  timing figures are how every entry in the tables below was obtained.
- **Used for debugging.** The skipped-update count showed how often the camera thread meets a
  busy LVGL lock, and comparing model output bit for bit across runs is what exposed the
  hardware-loop erratum.

### Measured Performance (ESP32-P4 rev 1.3, 360 MHz)

Measured on ESP-IDF 5.4.4 and re-measured on 5.5.5, with the same results within run-to-run noise.
The camera, embedding, inference-rate and canvas-swap rows are from a 60-second run of the
current build on 2026-10-07, taken from the 10-second log lines of the pipeline.

| Stage | Time | Notes |
|---|---|---|
| Detection (MSR+MNP) | 19–40 ms | Espressif publishes about 17 ms; rises with PSRAM load and face candidates |
| Embedding (MFN, incl. alignment) | 169–180 ms | Espressif publishes about 96 ms |
| Camera pipeline | 9.1–10.0 fps | Preview PPA (1280×960 → 640×480) 79–88 ms on average; detector PPA (from preview) about 19 ms; two sensor buffers |
| Inference rate | 4.7–5.0 /s | Limited by the camera rate |
| Canvas swap (`p4_ui_present_camera`) | 0.1 ms average, 0.2 ms at most | 1 swap skipped in about 480 because LVGL held its lock |
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

### Image Buffers

Every image-sized buffer is allocated once at start-up, in PSRAM. None is allocated per frame.

| Buffer | Count | Contents | Size each | Written by | Read by |
|---|---|---|---|---|---|
| Sensor (V4L2, MMAP) | 2 | 1280×960 RGB565 | 2.4 MB | ISP | PPA (preview scale) |
| Preview | 2 | 640×480 RGB565 | 614 KB | PPA | LVGL canvas; PPA (detector conversion) |
| Detector | 1 | 640×480 RGB888 | 922 KB | PPA | ESP-DL detection and embedding |
| Start-up canvas | 1 | 640×480 RGB565 | 614 KB | cleared to black once | LVGL, until the first camera frame |

Together about 7.7 MB.

- **Preview pair:** LVGL reads the buffer on the canvas whenever it redraws, so the PPA fills the
  other one and the two are swapped. This is double buffering for the display, not a camera
  requirement.
- **Detector buffer:** the inference thread keeps its image for 100 ms or more, while a preview
  buffer can be overwritten as soon as the next camera frame (when a swap was skipped), so
  inference needs a copy of its own. Exactly one exists, and it moves
  between the camera and inference threads through two channels of capacity 1; whichever thread
  holds it is the only one that touches its pixels.
- **Alignment:** the preview and detector buffers are aligned to the 128-byte L2 cache line and
  sized in multiples of it. The PPA writes by DMA and requires this of its output buffer, so the
  alignment is a correctness requirement and has no speed-up to measure.

**Two sensor buffers instead of three.** The camera loop takes about 100 ms and the sensor
delivers a frame every 22 ms, so the loop always holds one buffer and the driver fills the rest
long before the loop returns. Measured on 2026-10-07, one 60-second run each (five 10-second
windows), same build apart from `CAM_BUF_COUNT`; the scene was not identical (more faces in view
in the two-buffer run):

| | 3 sensor buffers | 2 sensor buffers (in use) |
|---|---|---|
| Camera loop | 10.4–10.5 fps | 9.1–10.0 fps |
| Preview PPA, average per window | 85.8–87.5 ms | 79.2–88.0 ms |
| Preview PPA, longest | 122.4 ms | 114.2 ms |
| Detector PPA, average | 18.4 ms | 18.6–18.7 ms |
| Inference rate | 4.9–5.3 /s | 4.7–5.0 /s |
| Canvas swaps skipped | 1 of 524 | 1 of 481 |
| Driver errors or warnings | none | none |
| PSRAM for sensor buffers | 7.4 MB | 4.9 MB |

Two buffers save 2.4 MB of PSRAM for about 0.5–1 fps of preview rate. The PPA itself got no
slower, so the extra time per loop is most likely spent waiting in `next_frame()` for the sensor
to finish a frame; that wait, and the age of the frame the loop receives, were not measured.

### Exploratory Measurements (not in this firmware)

Taken on local spike branches on 2026-10-03 and 2026-10-04 to inform design decisions. The code
is not merged, and the figures were not re-measured afterwards.

| Question | Result |
|---|---|
| Cost of keeping a second feature model loaded (for migrating templates) | A second MFN copy takes 2.1 MB of PSRAM (11.3 MB remain); MBF takes 4.3 MB (9.1 MB remain). Internal heap use is about 20 KB either way, leaving 147 KB. The camera stayed at about 10 fps |
| On-device text to speech (PicoTTS) | 1.43 MB of voice data, 1.1 MB of PSRAM, synthesis at about real time |
| Wi-Fi through the on-board ESP32-C6 (`esp_wifi_remote` 1.6.5 + `esp_hosted` 3.0.9, SDIO at 20 MHz) | Joined a WPA2 network in 4 s; an HTTPS request took about 0.6 s; a 2 MB download ran at 1.9 Mbit/s; 6 of 6 stability rounds without a disconnect |
| Memory with Wi-Fi up | About 75 KB of internal RAM free, 38 KB at the lowest. TLS buffers must be in PSRAM (`CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC=y`); in internal RAM the link stalled |
| Firmware size with Wi-Fi | 4.11 MB of the 5.24 MB slot (3.61 MB without) |

### Boot Sequence

1. `main`: mark the running OTA image valid, initialise logging.
2. `SystemResources::build()`:
   1. Take NVS, the system event loop and the timer service.
   2. `p4_hardware_init_all()` (C++):
      1. Shared I2C bus, I2S duplex audio, ES8311 codec (amplifier on).
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
   `Initialize → DetectionValidation ⇄ ActionExecuted`, and `Admin ⇄ Enrolling` from the
   admin view.

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
│   │       ├── contract.rs           # Model contract (input, model ids and slots, embedding size)
│   │       ├── manifest.rs           # Model partition image format: build, sign, verify
│   │       ├── signing.rs            # Ed25519 signatures and key files
│   │       ├── activation.rs         # A/B slot activation and rollback state machine
│   │       ├── matching.rs           # GroupMember, model-version-aware matching, dot product
│   │       ├── template.rs           # Member and template records as stored in flash
│   │       ├── hex.rs                # Hex text for digests, keys and device ids
│   │       ├── enrollment.rs         # Sample accumulator, member ids / names / slots, re-enrollment
│   │       ├── stats.rs              # Allocation-free latency and score statistics
│   │       └── eval_protocol.rs      # Wire format of the on-board evaluation harness
│   ├── model-packer/                 # Host tool: .espdl + manifest -> signed partition image; keygen
│   └── face-eval/                    # Host tool: sends dataset images to the eval firmware, computes FAR/FRR
├── firmware/                         # ESP32-P4 application (Rust + ESP-IDF)
│   ├── .cargo/config.toml            # Target, build-std, espflash runner, ESP-IDF version
│   ├── rust-toolchain.toml           # Pinned nightly + rust-src
│   ├── Cargo.toml                    # Crates + esp-idf-sys extra component / bindings header
│   ├── build.rs                      # Propagates ESP-IDF link args, links libstdc++
│   ├── sdkconfig.defaults            # ESP-IDF Kconfig (PSRAM, cache, camera, LVGL, Ethernet …)
│   ├── partitions.csv                # Flash layout (see below)
│   ├── trusted-model-keys.txt        # Public keys allowed to sign model images (compiled in)
│   ├── partitions-eval.csv           # Evaluation layout: second firmware slot -> candidate model
│   ├── components_esp32p4.lock       # Locked ESP-IDF managed component versions
│   ├── components/biometrics_wrapper/      # C++ ESP-IDF component
│   │   ├── biometrics_wrapper.cpp    # Display/LVGL, touch, camera V4L2, audio, Ethernet, OTA
│   │   ├── face_inference.cpp        # ESP-DL face models (MSR, MNP, MobileFaceNet) from partitions
│   │   ├── hwlp_erratum.S            # ESP32-P4 hardware-loop erratum workaround (context switch)
│   │   ├── include/biometrics_wrapper.h    # C API used from Rust
│   │   ├── include/bindings.h        # Headers esp-idf-sys generates Rust bindings from
│   │   ├── idf_component.yml         # Managed component dependencies
│   │   └── CMakeLists.txt
│   └── src/
│       ├── main.rs                   # Entry point, main loop
│       ├── system.rs                 # SystemResources: board bring-up, enroll / delete
│       ├── biometrics.rs             # State machine (recognition, admin view, enrollment)
│       ├── templates.rs              # Members and templates as NVS blobs
│       ├── ui.rs                     # Control panel: status line, admin view events
│       ├── pipeline.rs               # Camera + inference threads (Core 1)
│       ├── camera.rs                 # V4L2 frame lifetime (RAII)
│       ├── models.rs                 # Verify model slots, golden run, choose the active slot
│       ├── ppa.rs                    # PPA client, DMA buffers
│       ├── audio_worker.rs           # I2S mic capture thread
│       ├── speaker.rs                # I2S speaker output (chime)
│       └── power.rs                  # Inactivity watchdog, deep sleep, boot crash counter
├── tools/face-models.sh              # Fetch, package and flash the face models
├── tools/face-eval.sh                # Measure recognition accuracy on the board with a public dataset
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
| face_msr_a | data | `0xA20000` | 128 KB (MSR model, 61 KB) |
| face_mnp_a | data | `0xA40000` | 192 KB (MNP model, 130 KB) |
| face_feat_a | data | `0xA70000` | 2 MB (MobileFaceNet, 1.3 MB) |
| templates | data/nvs | `0xC70000` | 256 KB (enrolled templates, up to 32 × 2.1 KB) |
| face_msr_b | data | `0xCB0000` | 128 KB |
| face_mnp_b | data | `0xCD0000` | 192 KB |
| face_feat_b | data | `0xD00000` | 2 MB |
| storage | data/spiffs | `0xF00000` | 1 MB (unused) |

The partition table lives at the default `0x8000`, where `espflash` writes it and the app reads it.
- The app image is about 3.6 MB, 69% of a slot.
- Model partitions are 64 KiB aligned, because ESP-DL memory-maps them.
- The A slots and `templates` kept their offsets when the B slots were added, so a device with
  the earlier single-slot layout keeps its models and enrollments.

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
cd crates/biometric-core   # and crates/model-packer, crates/face-eval
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# Firmware: always use the release profile
cd firmware
cargo build --release          # first build downloads ESP-IDF + tools (needs network, takes a while)
cargo run --release            # build, flash (espflash, partitions.csv) and open the serial monitor
../tools/face-models.sh all     # first time, or after a model change: write the face models
cargo fmt --check && cargo clippy --release -- -D warnings   # same checks as CI
cargo clippy --release --features eval -- -D warnings          # the evaluation build, also in CI
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
  otherwise be misread. Erasing the chip also erases the enrolled templates.

---

## License

Internal proprietary firmware developed for the ESP32-P4 Biometric Hardware Agent system.
