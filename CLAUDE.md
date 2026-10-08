# Claude Code Engineering Guidelines

## Project
Biometric inference firmware in Rust for the Waveshare **ESP32-P4-NANO**. The system is described
in [README.md](README.md) and the plan in [docs/ROADMAP.md](docs/ROADMAP.md); do not restate them here.

## Build & Test
Commands are in the README's "Build, Flash & Test" section. Three things to remember:
- **Release profile only.** The debug profile boot-loops on ESP-IDF's legacy-I2C conflict check;
  only LTO strips it.
- **After editing C++ or headers**, run `touch firmware/sdkconfig.defaults` before building.
  Otherwise `esp-idf-sys` relinks the old object and the board runs stale code.
- **After changing `partitions.csv`**, the chip must be erased, which also erases enrolled
  templates. Ask before doing it.

## Code Rules
- Rust code targets **Rust 1.99** (`rust-version = "1.99"` in every crate).
- **Where code goes:** logic that does not need ESP-IDF belongs in `crates/biometric-core` with
  host unit tests. Firmware modules stay thin over FFI, because firmware code cannot be tested
  on the host.
- **Rust first:** write C++ only for what Rust cannot reach directly (today: LVGL, V4L2 camera
  ioctls, panel bring-up, ESP-DL inference). Before adding C++, say why Rust cannot do it. Keep
  the C++ side a thin C API with the logic in Rust.
- **Concurrency:** use native safe concurrency. No `RefCell` unless explicitly asked.
- **Stages never block each other.** This is why the project is in Rust. Sensor paths
  (`cam_pipeline`, audio capture, touch) and processing (`inference`, matching, the state
  machine) must not wait on one another:
  - Hand data between stages by moving ownership through a bounded channel with `try_send` /
    `try_recv`; when the queue is full, drop or skip, never wait. A stage may block only while
    waiting for its own input.
  - Share state between stages lock-free (atomics, `ArcSwap`). No `Mutex` held across stages.
  - Inside the sensor loops, do not lock a `Mutex` and do not create or clone an `Arc`.
    One exception: `cam_pipeline` may *try* the LVGL lock for the canvas swap, without
    waiting (LVGL is not thread-safe). When LVGL is busy it skips that preview update.
- **Allocation:** no heap allocation per frame in the `cam_pipeline` or `inference` threads.
  Allocate at startup and reuse. Allocation on rare paths (enrollment, boot, migration) is fine.
- **`unsafe`:** every `unsafe` block has a `// Safety:` comment stating why it is sound. Keep the
  block as small as the FFI call or pointer operation it covers.
- **Lints:** do not add code (a trait impl, a helper) or an `#[allow(...)]` only to satisfy a
  lint. Report the lint with `file:line` and the options, and ask.
- **Core affinity:** Core 1 is reserved for the vision pipeline. Do not add work to Core 1 or
  change task priorities without saying so and updating the README thread model table.

## Abstraction Rules
Prefer explicit code and what the language already supports over wrappers and abstractions that
add no functionality.

- Prefer what the language and standard library provide over hand-written equivalents. Before
  adding a trait impl, helper, or type, check whether a std trait or blanket impl already covers
  it (e.g. bound on `Borrow<T>` rather than writing `impl AsRef<T> for T`).
- Do not add a wrapper, forwarding function, newtype, trait, or generic parameter unless it
  adds behaviour, enforces an invariant, or has at least two real (non-test) uses today. "Might
  be useful later" is not a reason.
- Prefer concrete types in signatures. Introduce a generic only when existing callers pass
  different types.

## Workflow
- Follow "Ground Rules for Every Milestone PR" in [docs/ROADMAP.md](docs/ROADMAP.md).
- **Open the PR; do not merge it.** The maintainer reviews and merges.
- **Never commit or print secrets:** Wi-Fi credentials, model signing secret keys.

## Review
When asked to review existing code:
- Report violations of the Code Rules and Abstraction Rules as findings, with `file:line`.
- Separate what was verified (compiled, tested, read in full) from what was inferred.
- Propose the fix, but do not apply it until asked.

## Communication
- **Keep tool output small.** Capture board logs to a file with a bounded timeout and report a
  filtered summary; do not stream the serial monitor. Cap search output.
- **Ask before a multi-step investigation.** Once a root cause is likely, propose the fix rather
  than proving it exhaustively.
- **When a rule here does not fit the case**, say so and ask, rather than working around it
  silently.
