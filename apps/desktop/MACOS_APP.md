# Building the RailSpotting macOS app

This file directs development of the macOS desktop app and the macOS support it needs. It is
written for a coding agent (Claude Code) running on the developer's Mac, and for the developer.
Read it fully, then read `README.md` and `AGENTS.md` at the repository root before changing code.

## Where things stand

- The Rust receiver (this repository) has run the production receiver since 2026-10-12, on a
  Linux container (Celeron J4125, 4 cores, 2 GB RAM) with two RTL-SDR dongles. Production must
  keep working: every change here also builds and tests on Linux.
- The engine already compiles for macOS in principle. Never built on a Mac yet; expect small
  breakages on the first `cargo test`.
- The developer's Mac is Apple Silicon, with RTL-SDR dongles and a Uniden SR30C scanner attached.
  It is the hardware test bench for everything below.

## The goal

A signed macOS (Apple Silicon) app that:

1. runs the receiver engine in-process, as a menu bar app;
2. shows a dashboard window (the engine's web UI), and keeps running in the menu bar when the
   window is closed;
3. does everything the Linux receiver does (SDR dongles, the SR30C scanner, detector templates,
   linking, uploads), using macOS facilities where Linux uses sysfs or systemd;
4. installs from a `.dmg` and updates itself from GitHub Releases.

## Ground rules

- **One codebase.** Work on short-lived branches off `main` (`feature/macos-usb`,
  `feature/desktop-tray`, ...) and merge back. No long-lived macOS branch.
- **Platform code inside the crates**, behind `#[cfg(target_os = "macos")]`, with one API for
  both platforms. Pattern: `crates/store/src/usb/{mod.rs, linux.rs, macos.rs}`, where callers
  only see `usb::plugged_in_here()`.
- **The Linux appliance never builds the desktop app.** When `apps/desktop` becomes a crate, add
  it to `members` in the root `Cargo.toml` but not to `default-members`.
- **Don't change receiver behaviour** (what is detected, reported or uploaded) as part of macOS
  work. In particular, keep the detector rules as they are:
  - **Milepost:** always read by speech. Within 7 miles of the template's milepost it is that
    detector; further away it is a different detector.
  - **`defects: true`:** only for a message longer than the all-clear whose all-clear ending
    didn't match.
  - **Contract:** keep the v1.5 API contract.

  If something must change for macOS, change it for both platforms, with tests.
- **CI must stay green** on both `ubuntu-latest` and `macos-14` (`.github/workflows/ci.yml`).
- **Keep tests passing:**
  - Run `cargo test -p <crate>` for what you touch, and `cargo test --workspace` before merging.
  - Tests needing `fixtures/local` (private recordings) skip when it is missing; that is
    expected on the Mac.
- **No personal data in the repo:**
  - no email addresses (commits use the account's GitHub noreply address via repo-local
    `git config`);
  - no real device serials, MAC addresses, IPs, home locations or local paths.
  - Check `git diff --cached` before every commit.
- **Writing style for docs, comments and log text:**
  - Short, plain sentences.
  - No em dashes.
  - Comments explain why, at the density of the surrounding code.
- **The Mac is its own receiver.** On first run it creates its own key and device ID and links
  as a separate receiver. Never copy `credentials.json` or `state/` from the Linux server.

## Facts about the engine you will need

| What | Where |
|---|---|
| Engine handle | `crates/engine/src/handle.rs`: `Engine::start(settings, EngineOptions)`, `status()`, `subscribe()`, `apply_settings()`, `relink()`, `stop()`. The CLI's `run` uses it. The engine owns the status page, so a settings change restarts the agent behind it without changing the port. |
| Agent lifecycle | `crates/engine/src/agent.rs`: `Agent::new(settings, AgentOptions)`, `start()`, `shutdown()`, `status()`. Use the engine handle rather than the agent directly. |
| Settings | `railspotting_common::Settings` (`crates/common`): `receiver.json` merged over `default_settings()`; `state_dir` is relative to the settings file. Example: `receiver.json.example`. |
| Dashboard | `crates/engine/web/dashboard.html`, served by `crates/engine/src/web.rs` (tiny_http): `/`, `/api/status`, `/api/events`, `/api/signal`, `/link-qr.svg`. Bind address and port come from settings `web.bind` and `web.port`. |
| librtlsdr | `crates/engine/src/rtl.rs`: opened at run time with `libloading`. Already tries `/opt/homebrew/lib/librtlsdr.dylib` and `/usr/local/lib/librtlsdr.dylib`. GPL: never link it statically. |
| USB presence | `crates/store/src/usb.rs`: sysfs on Linux, returns `None` elsewhere (the dashboard then can't say "unplugged"). |
| Identity | `crates/store/src/identity.rs`: on macOS the MAC comes from `networksetup -listallhardwareports`, preferring `en0`. |
| Scanner | `crates/scanner/src/scanner.rs`: on macOS, finds `/dev/cu.usbserial*` or `/dev/cu.SLAB_USBtoUART*`. |
| Speech | `crates/engine/src/whisper.rs` (feature `speech`, whisper-rs 0.16). Prefers `ggml-small.en-q8_0.bin`, then `ggml-small.en.bin`, in `models/` next to the state dir. Shrinks the encoder window for short clips. Uses up to 4 threads. |
| Signals | The CLI waits for SIGTERM/SIGINT with `signal-hook`, then calls `Engine::stop()`. Inside an app, the app owns shutdown instead. |

## Phase 0: build and test on the Mac

1. Install the tools:
   ```
   xcode-select --install
   brew install librtlsdr cmake
   curl https://sh.rustup.rs -sSf | sh
   ```
2. Run `cargo test --workspace` and fix what fails on macOS. Keep the fixes cross-platform.
   Likely spots:
   - Unix socket paths longer than macOS's 104-byte limit in tests (use short temp dirs);
   - `/proc` or sysfs assumptions;
   - file modes;
   - `networksetup` output parsing.
3. Run `cargo build --release -p railspotting-receiver --features speech`.
4. With the dongles plugged in:
   - `target/release/railspotting-receiver sdr-list` must list both dongles;
   - `run` with a scratch `receiver.json` (copied from the example) must stream both, show the
     dashboard on port 8080, and hear the NOAA reference.
5. The SR30C must be found and must answer.
   - **Cable driver:** the cable is a Silicon Labs CP210x. If no `/dev/cu.*` device appears,
     the Silicon Labs VCP driver is needed. Document that in the README rather than working
     around it.
6. **Done when:** tests pass on the Mac, CI is green on both platforms, and a terminal `run`
   receives from both dongles and the scanner.

## Phase 1: macOS support in the crates

1. **USB presence on macOS:** `usb/macos.rs` using IOKit (the `io-kit-sys` crate is already a
   dependency of `serialport`). It finds RTL-SDR dongles (vendor 0x0bda, products 0x2832 and
   0x2838) with their serial numbers and a location string, so the dashboard can say which
   dongle was unplugged. Write a parser test with captured IOKit data.
2. **App data locations:**
   - When run by the app, settings and state go in `~/Library/Application Support/RailSpotting/`
     (`receiver.json`, `credentials.json`, `state/`, `models/`), and logs go in
     `~/Library/Logs/RailSpotting/`.
   - The CLI keeps today's behaviour (paths relative to `--config`).
   - Put the choice in one place, such as a `paths` module in `crates/common`.
3. **Metal speech on Apple Silicon:**
   - Add an engine feature that enables whisper-rs's Metal support. Check the feature name in
     whisper-rs 0.16's `Cargo.toml`.
   - Make the app build use it, and measure the per-clip time on the real detector clips. Use
     `crates/engine/examples/transcribe_clips.rs`; the clips are not in the repo, so record
     your own or ask the developer for some.
   - If transcribing every call becomes cheap, keep `transcribe_other_calls` a user setting.
     Don't turn it on by default.
4. **Dashboard binding:** for the app, default `web.bind` to `127.0.0.1`. A laptop moves between
   networks.
5. **Model download:**
   - A function that downloads `ggml-small.en-q8_0.bin` from
     `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/` into `models/`;
   - download to a `.part` file, then rename;
   - check the size or a SHA-1 published in the whisper.cpp repo;
   - report progress, so the app can show it.

## Phase 2: the engine as a library the app can drive

1. **A small handle API** in `crates/engine`. It is shared with Linux; the CLI should use it
   too.
   ```rust
   let rx = Receiver::start(settings, options)?;   // spawns everything, returns quickly
   rx.status() -> Value;                           // what /api/status returns
   rx.subscribe() -> Receiver<EngineEvent>;        // status changes, events, link code, errors
   rx.apply_settings(new: Value) -> Result<(), String>; // validate, save receiver.json, restart parts
   rx.stop();                                      // flush the outbox, close dongles and scanner
   ```
2. **Settings from the dashboard.** Add `POST` endpoints for:
   - dongle roles and gains;
   - `watch_clips`;
   - `transcribe_other_calls`;
   - the label;
   - starting linking again.

   They must be refused unless the request comes from 127.0.0.1 **and** carries a per-run
   token that the app injects into its window. The Linux status page is reachable on the LAN
   without a login, so on Linux these endpoints are off unless a setting enables them.
3. **Dashboard updates:**
   - add a settings view to `dashboard.html`;
   - keep it a single self-contained HTML file (no build step, no CDN), so the Linux appliance
     can still serve it offline.
4. **Done when:** the CLI uses the handle API, the Linux binary behaves exactly as before (all
   tests pass), and settings changes round-trip in tests.

## Phase 3: the desktop app (`apps/desktop`, Tauri 2)

1. **Scaffold** a Tauri 2 app in `apps/desktop`:
   - it depends on `railspotting-engine` (with speech and Metal);
   - its crate is in `members`, not in `default-members`.
2. **Window:**
   - it loads the engine's dashboard from `http://127.0.0.1:<port>/`, with the per-run token;
   - closing the window hides it and the app keeps running.
   - Switch the activation policy to `Accessory` (no Dock icon) while hidden, and back to
     `Regular` when the window is shown.
3. **Menu bar icon and menu:**
   - the icon shows state: receiving (normal), attention (a dongle or the scanner missing, not
     linked), offline (headend unreachable);
   - icons come from `branding/` (see `branding/logo-kit.md`): the app icon from the mark, and a
     monochrome template version of the mark for the menu bar, which macOS tints for light and
     dark menus;
   - menu items: Open Dashboard, the receiver's state as a disabled line, Pause/Resume
     Receiving, Launch at Login (`tauri-plugin-autostart`), Check for Updates, Quit.
   - Quit calls `stop()` and waits for it.
4. **Single instance** (`tauri-plugin-single-instance`): a second launch opens the window.
5. **First run:** a short setup in the dashboard.
   1. librtlsdr check. If missing, show `brew install librtlsdr` and a Check Again button.
   2. Speech model download, with progress.
   3. Link code and QR, until approved.
   4. Which dongle is which: plug and unplug to identify, using phase 1's USB presence.
5. **Notifications (optional):** a macOS notification for a detector report with
   `defects: true`, off by default.
6. **Done when:** the app runs from `cargo tauri dev`, receives on both dongles and the scanner,
   survives window close and reopen, sleep and wake, and dongle unplug and replug, and quits
   cleanly with an empty outbox.

## Phase 4: packaging and releases (`packaging/macos`)

1. **Hardened runtime, no App Sandbox.** The sandbox blocks the raw USB and serial access the
   receiver needs. This is also why the Mac App Store is out of scope.
2. **Signing and notarization:**
   - sign with a Developer ID Application certificate;
   - notarize with `notarytool`;
   - staple.
   - Credentials go in GitHub Actions secrets, never in the repo.
3. **`.github/workflows/release.yml`:** on a `v*` tag, build:
   - the Linux binaries (x86_64, and aarch64 if cheap);
   - the signed `.dmg` for Apple Silicon.
   - Attach them to the GitHub Release.
4. **Tauri updater:** points at the release's `latest.json`. The updater signing key goes in
   secrets.
5. **librtlsdr:** v1 requires Homebrew. Bundling the dylib inside the app later is allowed under
   the GPL only with its license text and a source link shipped alongside. Decide with the
   developer before doing it.
6. **Done when:** a tagged release produces a `.dmg` that installs on a clean Mac without
   Gatekeeper warnings, and the installed app updates itself to the next tag.

## Phase 5: hardware checklist on the Mac

- [ ] Both dongles stream. The NOAA reference shows a sensible SNR. No overruns over an hour.
- [ ] Unplugging a dongle shows it as unplugged in the dashboard and the menu bar within a few
      seconds. Replugging recovers it without a restart.
- [ ] The SR30C is found, plans are synced, and auto EOT focus works.
- [ ] The receiver links as its own device, and the headend accepts its events and heartbeats.
- [ ] A detector announcement (a template such as `ns-475.8`, if in range) is matched, the
      milepost is read by speech, and the timing is logged.
- [ ] Sleep and wake: streaming resumes, and no events are lost from the outbox.
- [ ] CPU and memory while idle are reasonable for a laptop app. Record the numbers in this
      file.

## Out of scope for now

Intel Macs (a universal build can come later), Windows, the Mac App Store, and any change to the
headend.

## Status

Update this section as phases complete: what was done, what was measured, what is left.

- Phase 0: done, except the SDR checks (incomplete: no dongles on the development Mac).
  - Done: `cargo test --workspace` passes on an Apple Silicon Mac (305 passed, 2 ignored), and
    the release build with `--features speech` works. CI is green on `macos-14`.
  - Fixed: an unused `mut` warning on macOS, and `sdr-list` no longer gives the Proxmox hint on
    a Mac.
  - Found: a Mac with both an Intel (`/usr/local`) and an Apple Silicon (`/opt/homebrew`)
    Homebrew needs librtlsdr and cmake from `/opt/homebrew`. Noted in the README.
  - Found: the SR30C cable (CP2104) appears as `/dev/cu.usbserial-*` with the driver built into
    macOS, so the Silicon Labs VCP driver isn't needed.
  - Checked: the scanner code finds the SR30C on its own and it answers at 57600 baud (model,
    firmware, signal and frequency, volume, squelch). Read-only queries only.
  - Incomplete (SDR): the development Mac has no RTL-SDR dongles attached. Still to do when
    they are: `sdr-list` showing both, and a terminal `run` that receives from both dongles and
    the scanner. Phase 5 repeats these checks in the app.
- Phase 1: done, except USB presence and the Metal timing.
  - Done: app data folders and file logging (`common::paths`), the app dashboard on 127.0.0.1
    and a free port, the speech model download (`engine::models`, checked against the size and
    SHA-256 Hugging Face lists, since whisper.cpp publishes no SHA-1 for the q8_0 file), and a
    `metal` feature that CI builds on macOS.
  - Left: USB presence through IOKit (needs a dongle to capture test data), and per-clip speech
    timings with Metal (needs the model and real detector clips).
- Phase 2: done.
  - `handle::Engine` (start, status, subscribe, apply_settings, relink, stop). The CLI's `run`
    uses it. Settings endpoints need the per-run token and 127.0.0.1. Only the app has a
    token, and it gives it to its own window only; the console receiver has none, so a page
    served by a receiver is never editable (owner decision, October 2026). The dashboard has a Settings card when it has the token.
  - Tested: settings round trip over HTTP with an agent restart on the same port, and by hand
    in a browser.
- Phase 3: in progress; the app runs and is ready for testing.
  - Done: the Tauri 2 app in `apps/desktop` (see its README). Menu bar icon in three states
    (receiving, attention, offline) as template images made from the mark; Open Dashboard,
    state line, Pause/Resume, Launch at Login, Quit. The window loads the dashboard with the
    token; closing it hides it and drops the Dock icon. Links that leave the dashboard open in
    the browser. Single instance. First-run speech model download with progress in the menu,
    then a restart. librtlsdr missing shows in the menu with the brew command.
  - Checked: launch, linking (fake headend), the dashboard and Settings card in the window, the
    menu bar icon, a second launch opening the first, and Quit through macOS's terminate path
    stopping the receiver cleanly.
  - Done: dongle setup in the Settings card. A role order list (move roles up and down, untick
    one to switch it off; `sdr.role_order`), a job for each connected dongle (Automatic, a role,
    or Off; `sdr.assign`, where a job named on the website wins), all four roles (EOT, ITC, VHF,
    HOT) in the fixed dongle rows, and an editor for the whole receiver.json
    (`GET`/`POST /api/config-file`, same token gate). A file must be a JSON object that loads as
    settings; when the receiver won't start with it, the old file is put back.
  - Done: the dashboard has tabs (Status, EOT, Defect detectors, ITC, HOT, Voice) with numbers
    over time for tuning; the window shows them as the browser does, and Settings stay on the
    Status tab.
  - Tested: validation of the new keys, `sdr.assign` replaced whole on save, receiver.json jobs
    with the website winning, and the whole-file round trip with the restore.
  - Left: first-run steps inside the dashboard (librtlsdr Check Again, model progress, dongle
    identification beyond the existing chips), notifications, sleep and wake, and window
    close and Pause/Resume tested by hand.
- Phase 4: in progress; releases build and publish, without Apple signing yet.
  - Done: `.github/workflows/release.yml` builds the app on a `v*` tag (`tools/release.sh`) and
    publishes the `.dmg`, the update bundle signed with the updater key, and `latest.json` to
    GitHub Releases. The version comes from the tag. The app checks for updates a minute after it
    starts and every six hours, offers a found one in the menu, and on Install stops the receiver
    as on Quit and restarts on the new version. See `packaging/macos/README.md`.
  - Checked: the build on this Mac with the updater key gives the ad-hoc signed app, the `.dmg`,
    the update bundle and its signature, and `release-files.sh` writes `latest.json`.
  - Fixed in 2.0.2: 2.0.1 could not load librtlsdr (the hardened runtime's library validation
    refuses Homebrew's ad-hoc signed library). `packaging/macos/entitlements.plist` allows it.
  - Left: a Developer ID certificate and notarization (the Apple secrets; the workflow uses them
    once set), the Linux binaries in the release, and an update from one release to the next
    tested on a real install.
