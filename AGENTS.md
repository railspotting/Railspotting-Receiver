# Guide for AI coding agents

This file is for AI agents (Codex, Claude Code and others) working in this repository. `CLAUDE.md`
imports it, so keep everything shared here. Read `README.md` for what the receiver does and how to
build it; this file covers how to work on it.

## The project

RailSpotting receivers listen to railroad radio and report to the RailSpotting headend (a
separate, closed service reached over a signed HTTPS API). A receiver uses:

- two RTL-SDR dongles: a VHF one for voice channels, and a UHF one for end-of-train (EOT)
  telemetry;
- optionally, a Uniden SR30C scanner.

It detects:

- voice calls;
- EOT packets (decoded on the receiver);
- defect detector announcements. These are recognized by sound against learned templates, with
  the milepost number read by speech recognition.

The receiver is a Rust workspace (version 2.x). It runs in production on small Linux machines. A macOS menu bar app is in development (`apps/desktop/MACOS_APP.md`).

## Layout

| Path | What |
|---|---|
| `crates/common` | settings (`receiver.json` over defaults), channel plan helpers, clock, logging |
| `crates/proto` | Ed25519 keys and request signing, API client, device linking, a fake headend for tests (feature `fake`) |
| `crates/dsp` | channelizer, per-channel detection, bursts and recordings, EOT decoder and encoder, audio taps, radio simulator |
| `crates/itc` | ITC (220 MHz PTC) decoding: channel plan, channelizer, bursts, pi/4-DQPSK, link layer (header, RS(16,12), CRC), messages, feed, survey; its `messages.md` and `findings.md` |
| `crates/detect` | detector templates and assessment, v1.5 detector objects, transcript text parsing |
| `crates/store` | upload outbox and history (SQLite), identity, automatic dongle roles, USB presence |
| `crates/console` | the terminal console (`railspotting-receiver console`): a ratatui screen over the running receiver's control socket |
| `crates/scanner` | SR30C driver, scanner monitor, focus and roles, checks, headend commands, a fake SR30C |
| `crates/engine` | the receiver: agent loops, librtlsdr devices, speech (feature `speech`), dashboard web server, CLI |
| `apps/receiver` | the `railspotting-receiver` binary (a thin `main` over the engine's CLI) |
| `apps/desktop` | the macOS menu bar app (Tauri 2); build notes in its `README.md`, plan and status in `MACOS_APP.md` |
| `packaging/linux` | systemd unit, installer, Proxmox LXC guide |
| `packaging/macos` | macOS signing and packaging (to come) |
| `profiles/` | detector voice profiles, built into the receiver (see its README) |
| `fixtures/` | recorded reference outputs (API vectors, EOT decodes, radio scenarios, detector assessments) that tests compare against; `fixtures/local/` holds private recordings and is git-ignored |

## Commands

```
cargo build --release -p railspotting-receiver                    # receiver without speech
cargo build --release -p railspotting-receiver --features speech  # with whisper.cpp
cargo test -p <crate>                                             # one crate (railspotting-<name>)
cargo test --workspace                                            # everything; run before merging
cargo clippy -p <crate> --tests
```

- Some tests need private recordings in `fixtures/local/` and skip when they are missing. That
  is expected, not a failure.
- The `engine` integration tests take about two minutes. They are one test binary
  (`crates/engine/tests/it/`) to keep disk use down.
- On small machines, limit build parallelism: `CARGO_BUILD_JOBS=2`, or a local
  `.cargo/config.toml`, which is git-ignored.

## Rules

### Behaviour

- **Production depends on this code.** Don't change what is detected, reported or uploaded
  without being asked. When asked, add or update tests that pin the new behaviour.
- **Keep the API contract.** Requests are signed exactly as `crates/proto` does it (canonical
  string, SHA-256 body hash, Ed25519). Detector objects follow contract v1.5;
  `detect::validate_detector` is the check. Field names and JSON shapes are what the headend
  expects.
- **Detector data comes from the headend.** `GET /api/v1/config` carries `detectors` (v1.5) and
  `nearby_detectors` (v1.6: each nearby detector's channel and expected wording). The receiver
  watches a nearby detector's channel for speech only when its railroad has no sound template,
  prefers confirmed wording from the headend as the speech prompt, and matches announcements to
  the nearby list before the wider one. Don't hard-code detectors or channels; the small
  per-railroad wording table in `crates/engine/src/transcribe.rs` is only a fallback.
- **Detector rules (deliberate decisions, not accidents):**
  - **Voice profiles** come from the headend (`/config` `voice_profiles`, and `channels[].profile`
    for which channel uses which; contract v1.9, `docs/contract-v1.9.md`). The copies in
    `profiles/` are built in as a fallback until headends serve them. The receiver never changes
    profiles or their channels; its status page only shows them and tests them on recordings.
    A profile says how a detector style speaks: wording with
    `{milepost}`-style placeholders, typed values with limits, the audio filter, the all-clear
    phrase (and how it is misheard, `heard_as`), defect words, and triggers.
  - **Every detector announcement is transcribed whole**, filtered as its profile says, with the
    profile's wording as the prompt (values left out, since the expected number biases what is
    heard). Speech runs only for detector announcements, so there is no sustained load.
  - **Milepost:** from the transcript. Within 7 miles of the template's milepost it is that
    detector; further away it is a different detector, looked up in the headend's list, and sent
    as a transcript only if it isn't listed. Values outside the profile's limits are misheard.
  - **Result:** sound and words together. Sound's all-clear stands. When sound missed the
    all-clear but the words have it, it is an all-clear: a v1.5 result with `method: "words"`
    only if the headend lists `words` in `detector_methods` (v1.9, `docs/contract-v1.9.md`),
    otherwise a transcript only. Defect words outside the all-clear phrase make it a defect
    report, with the words in `heard_text`. Anything else stays unconfirmed. Without a speech
    model the old rule applies: longer than the all-clear and its ending not matched is a defect.
  - **Watched channels** (detectors without a sound template): a call that opens together with
    more channels than the profile allows is interference, and a transcript that repeats a phrase
    three or more times is speech recognition looping on noise; both are dropped.
- **ITC messages stay on the receiver.** Decoded ITC messages describe railroad operations: they
  go to `state/itc/` and the local feed, never to the headend. Only wayside state changes and base
  stations are uploaded (`POST /itc`, contract v1.18, `docs/contract-v1.18.md`), and only while
  `/config` `itc.upload` is true, which the owner turns on; repeats are never sent. Turned off,
  the queued records are dropped. Their JSON fields (`crates/itc/messages.md`) are read by RailSpotting-ITC-Monitor: change them
  only on purpose, and update both.
- **Dongle roles** fill in `sdr.role_order` (default EOT, ITC, VHF, HOT; an owner decision of
  October 2026). The headend can set the order (`sdr_role_order`, proposed contract v1.17, `docs/contract-v1.17.md`) and
  the receiver writes it into receiver.json.
- **The receiver's status page is never editable.** Settings change only from the local console
  (receiver.json, or `railspotting-receiver console` over `state/control.sock`, a Unix socket for
  the service user, its group and root) or the Mac app's own window, which gets a per-run token
  injected by the app. Don't add a way for a browser to get the token (no settings switch, no token in the
  address, no token file). An owner decision of October 2026.
- **librtlsdr is GPL.** It is loaded at run time with `libloading` and never linked or vendored.
  Keep it that way unless the maintainers decide otherwise. The one exception, an owner decision of
  October 2026: the Android app (`railspotting/Railspotting-Android`) ships a patched librtlsdr as
  its own shared library, because Android has no system librtlsdr. This crate still only loads
  it at run time.

### Code

- **One codebase for all platforms.**
  - Platform differences live inside crates behind `#[cfg(target_os = "...")]`, with one API
    (example: `store::usb`).
  - No per-platform branches or copied crates.
  - Every change must build and pass tests on Linux and macOS (`.github/workflows/ci.yml`).
- **The desktop app stays out of default builds.** It goes in workspace `members` but never in
  `default-members`, so Linux appliances don't build it.
- **Style:** match the surrounding code.
  - Threads and channels, not async.
  - `serde_json::Value` for JSON that crosses crate boundaries.
  - No `unwrap()` on input or I/O outside tests; log and carry on.
  - Time is `f64` epoch seconds through `common::Clock`, so tests can use `FakeClock`.
- **Comments and docs:**
  - Short, plain sentences that explain why.
  - No em dashes.
  - Log lines are read by people running receivers, so keep them clear and stable.
- **Tests:** port or add a test for every behaviour change. The reference fixtures in `fixtures/`
  pin exact outputs; change them only on purpose, with the reason in the commit message.

### Repository hygiene

- **Never commit personal or secret data:**
  - email addresses;
  - usernames;
  - private keys or `credentials.json`;
  - real device serial numbers, MAC addresses or device IDs;
  - IP addresses, hostnames or home locations;
  - absolute local paths.

  Use made-up values in tests (the existing tests use serials `30001111` and `30002222`).
  Review `git diff --cached` before every commit.
- **Commit author:** commits use the repository's configured author with a GitHub noreply
  address. Don't add co-author or sign-off lines that contain email addresses.
- **Ignored, never commit:** a running receiver's files (`receiver.json`, `credentials.json`,
  `state/`, `models/`, `*.bin`). Use `receiver.json.example` and `credentials.json.example` as
  references.
- **Branches:** short-lived feature branches off `main`, merged back once tests pass.

## Where to start

| Task | Start here |
|---|---|
| macOS app | `apps/desktop/MACOS_APP.md`: phases, acceptance criteria, hardware checklist |
| Detection behaviour | `profiles/` (voice profiles), `crates/detect/src/profile.rs`, `crates/detect/src/detectors.rs` and `crates/engine/src/transcribe.rs` |
| Radio processing | `crates/dsp/src/sdr.rs` |
| Headend API | `crates/proto/src/api.rs`, `link.rs` |
| Dashboard | `crates/engine/web/dashboard.html` (one self-contained file, no build step, no CDN) and `crates/engine/src/web.rs` |
| Look and feel (dashboard, macOS app icons, text) | `branding/`: brand guide, logo kit and logo files. A copy from the headend repository; change it there, not here. |
| Installing on Linux | `packaging/linux/install.sh`, `lxc-setup.md` |
