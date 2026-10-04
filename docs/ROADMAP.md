# Roadmap

This project is a learning resource for **edge inference on the ESP32-P4 in Rust**. The roadmap
takes it from "camera and display work" to a device whose **models and enrolled faces can be
updated in the field without reflashing firmware**, the way production edge-AI products are run.

Each milestone is delivered as **one self-contained pull request**, so it can be read and reviewed
on its own.

---

## Ground Rules for Every Milestone PR

- **Scope:** one milestone per PR. Unrelated fixes go in their own PR.
- **Guidelines:** follows [`CLAUDE.md`](../CLAUDE.md):
  - targets Rust 1.99;
  - native safe concurrency and zero-overhead abstractions;
  - no needless `RefCell` or per-frame heap allocation.
- **Tests:**
  - hardware-independent logic lives in `crates/biometric-core` with host unit tests;
  - hardware-dependent behaviour is validated on the ESP32-P4-NANO, and the PR records what was checked and how.
- **Docs:** README and this roadmap are updated in the same PR. They describe what the code does, not what it is meant to do.
- **Quality:** no compiler warnings in the firmware build.
- **Limitations:** known limitations are stated in the PR and the README's Project Status, not left implicit.

---

## Background: How Edge-Inference Data Is Managed

A deployed edge-AI device carries three kinds of artifacts with very different lifecycles:

| Artifact | Changes | Typical handling |
|---|---|---|
| **Firmware** (code) | Rarely, risky | Signed image, A/B app partitions, automatic rollback if the new image doesn't confirm itself (`ota_0`/`ota_1`, `p4_mark_app_valid` already exist here) |
| **Model weights** | Occasionally (retraining, new architecture) | Versioned artifact from the ML pipeline, delivered separately from firmware into its own A/B slot, verified before use |
| **Enrollment data** (known faces) | Often (people added/removed) | Server is the source of truth; the device keeps a synced cache; handled as sensitive personal data |

Five principles drive the design below:

1. **Model contract (manifest).** Firmware and model must agree on:
   - input size and pixel format;
   - normalisation and quantisation;
   - output dimension;
   - required runtime/operator versions.

   Each model ships with a manifest that also carries its version, hash and signature. The device refuses models whose contract it doesn't support.
2. **Templates are bound to a model version.** An embedding from model v1 is meaningless to model v2.
   - Every stored template records the `model_version` that produced it.
   - Changing the model means re-computing templates, which in practice happens server-side from retained enrollment images, or by re-enrolling.
3. **Safe update flow:**
   1. Download.
   2. Verify hash and signature.
   3. Write to the *inactive* slot.
   4. Validate: load the model and run a known test input against its expected output.
   5. Switch an NVS pointer.
   6. Keep the previous slot for rollback.

   Roll out to a few devices first.
4. **Device management.** Platforms such as ThingsBoard, AWS IoT Jobs or Azure device twins publish *desired* versions; the device reports *actual* versions and acts on the difference.
5. **Privacy.** Face templates are biometric data (e.g. GDPR special category, Illinois BIPA):
   - store templates, not images, on the device;
   - encrypt flash/NVS;
   - support deleting a person everywhere.

---

## Milestones

Status: ✅ done · 🚧 in progress · ⬜ planned

### ✅ Foundation (PR #6, #7)

- Firmware builds again; hygiene pass.
- Rust vision pipeline on Core 1: camera → PPA preview, plus inference hand-off.
- Host-tested `biometric-core` crate.
- Accurate README.
- LVGL pinned to Core 0.

### ✅ M0: Toolchain & Quality Gates

Make every later PR verifiable by anyone.

- **Rust 1.99 compliance:** the target moved from 1.90 to 1.99 (current stable).
  - Firmware: pinned to a 1.99-cycle nightly (needed for `build-std`).
  - Host crate: pinned to stable 1.99.0.
  - `rust-version = "1.99"` in every `Cargo.toml`.
- `rustfmt` (default style) and `clippy` (`-D warnings`) clean on both crates. The one clippy finding was
  `Ppa::scale_crop` taking 8 arguments; the destination is now grouped into a `ppa::Target`.
- Continuous integration (GitHub Actions, `.github/workflows/ci.yml`):
  - host tests on stable 1.99;
  - firmware release build (with cached ESP-IDF);
  - fmt/clippy checks.

**Validation:**
- CI green on the PR.
- The firmware built with the new toolchain boots on the board.

### ✅ M1: Real Inference with Embedded Models

Replace the stub detector and the untrained embedding model with Espressif's pretrained models,
compiled into the firmware.

- Integrate `espressif/human_face_detect` and `espressif/human_face_recognition`. Exact model variants and APIs are confirmed in the PR.
- Document the **model contract**: input format and size, preprocessing, output dimension. Make firmware constants derive from it rather than being duplicated.
- Show detection results on screen as an LVGL overlay (bounding boxes).
- Measure and document inference latency and memory use on the ESP32-P4.
- Retire the placeholder `mobilefacenet_quantized.espdl` and the untrained export scripts, or clearly mark them as a learning exercise.

**Validation:**
- Host tests for any new pure logic (box scaling between detector and display coordinates, contract checks).
- On-board: faces detected at the documented rate; latency numbers recorded in the PR.

**Outcome:**
- `human_face_detect` 0.4.2 (MSR+MNP) and `human_face_recognition` 0.3.2 (MFN, 512-d), with a
  C++ adapter (`face_inference.cpp`). Matching stays in Rust.
- Model contract in `biometric-core::contract`; the embedding length is checked at startup.
- Overlay boxes drawn on the preview.
- Measured on the board, as documented in the README:
  - detection 19–23 ms;
  - embedding about 187 ms;
  - faces found in up to about 80% of frames.
- Found and fixed a red/blue swap: PPA RGB888 is B,G,R in memory, so the buffer is passed to ESP-DL as BGR888.
- Pulled forward from M3: the partition table moved to the default `0x8000` (the old `0xc000` copy sat
  inside `nvs`), and app slots grew to 6 MB for the embedded models.
- Performance settings: XIP from PSRAM, 256 KB L2, `-O2`, and only the ESP-DL pixel conversion in use.
- Placeholder model, `ml/export/` and `tools/enroll_user.py` removed.

**Follow-ups (own PRs):**
- Camera pipeline throughput: about 8 fps, because each PPA scale takes 80–95 ms. *Partly addressed:*
  - 9.4–9.7 fps, by building the detector image from the preview, sizing the canvas to the image,
    and holding sensor buffers until a frame is processed.
  - The remaining limit is PSRAM bandwidth (see README, Measured Performance).
- Embedding latency: about 2× Espressif's figure.
- ESP-IDF 5.5.5 plus the esp-idf-* 0.53/0.47/0.38 crates. *Done:*
  - Builds needed `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` for the board's v1.3 silicon.
  - Performance-neutral for the pipeline (measured).

### ✅ M3a: Models in Their Own Partitions (Manifest + Verification)

Update model weights without rebuilding the firmware. Done before M2, so enrolled templates can
record the model version from the manifest (the runtime source of truth) from the start.

- Each model (MSR, MNP, MobileFaceNet) lives in its own partition. The `.espdl` data sits at
  offset 0, with a JSON manifest (model id, version, size, SHA-256) in the last 4 KiB sector.
- `biometric-core::manifest` defines the format and is used by both the firmware and the host
  packer. Host tests cover every failure mode.
- The firmware verifies each partition before ESP-DL loads it. A missing or corrupt model
  disables only its stage; the board never crashes because of a model.
- Espressif's face components can only load from partitions declared in an ESP-IDF partition
  table, which esp-idf-sys builds don't use. So the firmware builds the models itself (adapted
  from the components, MIT) with its own partition labels; the components are no longer built.
- Tooling:
  - `crates/model-packer` builds partition images;
  - `tools/face-models.sh` fetches Espressif's pinned releases (SHA-256 checked), packs and
    flashes them.
- The app image shrinks from 5.1 to 3.6 MB.

**Validation:**
- Host tests for the format and the packer.
- On-board: normal load; a single flipped bit in the embedder is rejected (detection continues);
  an erased detector partition is rejected (preview continues).

### ✅ M2: On-Device Enrollment

Enroll and recognise people locally, with no server.

- Template format v1 in `biometric-core`:
  - member id and name, role, embedding, `model_version`;
  - matching rejects templates from a different model version.
- Admin-button and touch flow to enroll the currently detected face; delete a member.
- Persist templates in flash (encrypted storage planned in M4b); load them at boot.
- Matching threshold documented and measured (false accept/reject on a small test set).

**Validation:**
- Host tests for serialisation, versioning and matching edge cases.
- On-board enroll → reboot → recognise → delete cycle.

**Outcome:**
- `biometric-core::template` (binary record, strict decoding), version-aware
  `matching::{closest, best_match}`, and `enrollment` (mean of 5 mutually consistent samples).
- Templates are NVS blobs in a new `templates` partition (256 KB carved from `storage`), up to
  32 members.
- The right-hand panel has a status line and an admin view (name field + keyboard, member list,
  Enroll / Delete / Done). Touch input now reaches LVGL in panel coordinates, so widgets can be
  pressed (LVGL applies the display rotation itself).
- The match threshold moved from 0.75 to Espressif's default of 0.5, and the log reports the
  similarity to the closest template.
- On the board: enrolled a person from the touch panel, recognised them (similarity 0.84),
  rebooted, recognised them again (0.77), deleted them, and they were no longer matched.

**Not done:**
- The threshold has **not** been measured on a test set. The only data is one enrolled person:
  similarity 0.32–0.87 per frame, 0.77 on average. Measuring false accepts needs people who are
  not enrolled.

**Follow-ups (own PRs):**
- Threshold measurement on a small test set, including non-enrolled people. Partly done by the
  evaluation harness below (public dataset); still open with this camera and live people.
- The display showed every frame about 150 px off, which made the first version of the touch
  panel unusable. Fixed separately by using the HX8394 driver's DSI timing.

### ✅ M3b: A/B Model Slots, Validation Run, Rollback

- Two slots per model (carved from `storage`), with an NVS pointer to the active slot.
- Before switching to a new model, run it on a golden input and compare the output with the
  expected result.
- Roll back to the previous slot on failure; report the active model version.

**Validation:**
- Host tests for the activation and rollback state machine.
- On-board: swap models without reflashing the app, and fall back when a new model fails validation.

**Outcome:**
- `biometric-core::activation`: a pure state machine (active slot, verdict on the standby image,
  trial marker) with host tests for activation, rejection, rollback, interrupted trials and the
  NVS record.
- Manifest format 2 adds `golden_sha256`; format 1 is still read. `model-packer --golden`.
- The golden run feeds the model a fixed pseudo-random input and hashes its output tensors, so
  it works for any model without model-specific test data. The expected digest ships in the
  manifest and is obtained from a device (the firmware logs it for an image without one).
- The state is saved to NVS before a trial, so a model that resets the chip is rejected after
  two unfinished trials.
- The boot log names the slot and version in use for each model.
- On the board: all three models switched to slot B after passing their golden runs; a wrong
  golden, a missing golden and random data were rejected; a corrupted active slot rolled back to
  the previous image. See the README for the full table.

**Not done:**
- A reset in the middle of a trial was not provoked on the board (host tests only): the random
  "model" that was expected to crash ESP-DL was rejected cleanly instead.
- No genuinely different model was available, so the images that were swapped differed in
  version label and golden only.

**Found along the way (fixed in its own PR):**
- The feature model's output was not repeatable when the inference thread was preempted: the
  same input gave the same output at top priority, and outputs a few quantisation steps apart
  when the camera thread interrupted the run. Cause: an ESP32-P4 hardware-loop erratum that
  ESP-IDF v5.5.5's context switch does not fully cover. Worked around in
  `hwlp_erratum.S` (see README); preempted runs are now bit-exact.

### ✅ On-Board Accuracy Evaluation

Measure recognition accuracy on the real hardware without people at the camera, to settle the
match threshold and to compare feature models before M4 fixes slot sizes.

**Done:**
- Firmware cargo feature `eval`: the board serves images sent over the console UART and returns
  the embeddings of every loaded feature model (`firmware/src/eval.rs`).
- A second, candidate feature model can run next to the active one; `partitions-eval.csv` gives
  it the unused second firmware slot.
- Host tool `crates/face-eval` and `tools/face-eval.sh` (fetch, flash, capture, report, restore).
- Metrics and wire format in `biometric-core` (`evaluation`, `eval_protocol`) with host tests.
- Run on the board with 1,189 LFW images of 400 people (1,317 genuine and 695,473 impostor pairs):
  - MFN (in use): equal error rate 1.07%; at threshold 0.5 no false accepts, 21.2% false rejects.
  - MBF (candidate): equal error rate 0.76%; at 0.01% false accepts it rejects 3.9% of genuine
    pairs against MFN's 5.4%, at 326 ms per run against 176 ms and 3.5 MB against 1.3 MB.

**Not done:**
- `MATCH_THRESHOLD` is unchanged; choosing a value from these numbers is a separate decision.
- Not measured with this device's camera, on live non-enrolled people, or against the
  five-sample templates the device actually stores.
- Thresholds are per model, but the firmware has one constant. A second feature model would need
  its threshold carried with the model (candidate for the manifest in M4).

### M4: Operating a Fleet

Four PRs, in this order. The device stays the place where people are enrolled; a server keeps
the durable copy of the templates and distributes them and new models.

#### ✅ M4a: Signed Models and the Trust Model

A hash shows a model image is intact; a signature shows who published it.

**Done:**
- Ed25519 signature over the manifest (which covers the model data's hash), stored in the last
  68 bytes of the manifest sector. `biometric-core::signing` and `manifest::verify_signed`; the
  same code signs on the host and verifies on the board.
- The firmware loads only images signed by a key in `firmware/trusted-model-keys.txt`, which is
  compiled in. Unsigned images are rejected; there is no switch to allow them.
- `model-packer --key`, `keygen` and `pubkey`; `tools/face-models.sh keygen`. The secret key lives
  outside the repository.
- Trust model documented in the README: who signs, how devices get keys, how to rotate them, and
  what signing does not protect against.
- On the board: unsigned, wrongly signed and tampered images were rejected and left the stage
  disabled or on the other slot; properly signed images loaded (see the PR).

**Not done:**
- No protection against replaying an older, validly signed image (planned with M4d).
- Without secure boot, someone with the serial cable can flash firmware that trusts another key.
  Signing protects the model update path, not the device against physical access (M4b).

#### ⬜ M4b: Encryption at Rest and Template Format v2

- NVS encryption for the template partition, with the key in eFuse. Secure boot and flash
  encryption are documented as the production step; whether they are burned on the development
  board is decided in the PR.
- Template format v2:
  - a member (id, name, role) is stored separately from its templates;
  - each template names its modality (face now, voice later) and the model release it belongs to;
  - a member can hold several templates at once, and a missing one means "needs enrollment";
  - member ids are unique across devices (today's `local-0001` is not).
- Migration of format v1 records.

**Validation:** host tests for the format and the migration; on the board, enrolled templates
survive the upgrade, and a flash dump no longer shows them in clear.

#### ⬜ M4c: Template Sync

- The device uploads what it enrolled and downloads what other devices enrolled, over TLS with
  a pinned server certificate and a per-device credential.
- Incremental: the server numbers every accepted change; the device asks for everything after
  the last number it saw. Local changes are pushed first, then remote ones pulled.
- Deletions are kept as markers so they propagate. On a conflict the higher revision wins, and
  a deletion beats an edit.
- A small reference server in this repository, for tests and for readers to run.

**Validation:** host tests for the sync logic; end-to-end against the local server, including an
interrupted sync and two devices' worth of changes.

#### ⬜ M4d: Remote Model Updates and Template Migration

- Download a signed model image to the standby slot, resumable after an interruption, then the
  M3b activation flow. Reject an image older than the one in use.
- A new feature model cannot use existing templates. Instead of re-enrolling everyone, the
  device runs the old and the new model side by side for a while: on a confident match by the
  old model, the same face is embedded with the new one, until the person has a template for it.
  People who do not show up before a deadline are flagged for enrollment. No face images are
  stored.
- Thresholds are per model (see On-Board Accuracy Evaluation), so the threshold travels with the
  model.

**Validation:** host tests for the download and migration state machines; on the board, an
interrupted download, a bad signature, and a migrated template compared with a directly
enrolled one.

Measured ahead of this work on the board: a second copy of the feature model costs 2.1 MB of
PSRAM (11.3 MB remain) and one extra run is 176 ms; the camera rate was unaffected.

### ⬜ M5: Telemetry & Model Monitoring

Know whether the model is working in the field.

- Report inference latency, match-score distributions, and enroll/match/reject counts.
- No biometric data in telemetry.
- Documentation on reading the metrics: drift, threshold tuning.

**Validation:**
- Host tests for metric aggregation.
- On-board reporting to the device-management server.

### Out of Scope (for now)

- On-device voice recognition: `tools/enroll_user.py` computes speaker embeddings on the host only.
- ESP-IDF 6.x: wait for mature `esp-idf-sys` support. 6.0 removes the legacy I2C driver, moves Ethernet
  PHY drivers out of IDF, and changes the DSI 2D-DMA API.
- The occasional white/cyan display flashes: suspected cable or power; tracked as an issue.
