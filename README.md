# RailSpotting receiver

Listens to railroad radio with RTL-SDR dongles (and optionally a Uniden SR30C scanner), detects
voice calls, end-of-train (EOT) telemetry and defect detector announcements, and reports them to
the RailSpotting headend.

This is the Rust receiver, version 2.x.

## Layout

| Path | What |
|---|---|
| `crates/` | shared libraries for every platform: `common` (settings), `proto` (signed API, linking), `dsp` (channelizer, bursts, EOT decoder, simulator), `detect` (detector templates), `store` (outbox, history, identity, roles), `scanner` (SR30C), `engine` (the receiver: agent, dongles, speech, dashboard) |
| `apps/receiver` | the headless `railspotting-receiver` binary (Linux service, or a terminal on macOS) |
| `apps/desktop` | the macOS menu bar app (in development; see `apps/desktop/MACOS_APP.md`) |
| `packaging/linux` | systemd unit, installer, Proxmox LXC guide |
| `packaging/macos` | signing, notarization and `.dmg` settings for the desktop app |
| `fixtures/` | recorded reference outputs the tests compare against |
| `profiles/` | detector voice profiles: how each style of defect detector speaks (see `profiles/README.md`) |
| `branding/` | logos, colors and voice (a copy from the headend repository; see `branding/README.md`) |

Platform differences stay inside the crates behind `#[cfg(target_os = ...)]`; there are no
per-platform branches.

## Build

    cargo build --release -p railspotting-receiver                     # without speech
    cargo build --release -p railspotting-receiver --features speech   # with speech (whisper.cpp)
    cargo build --release -p railspotting-receiver --features metal    # speech on the GPU (Apple Silicon)

The speech build needs a C/C++ toolchain and cmake. RTL-SDR dongles need librtlsdr installed
(`apt install librtlsdr2`, or `librtlsdr0` on Debian 12; `brew install librtlsdr` on macOS). It
is loaded at run time, not linked.

On an Apple Silicon Mac, use the Apple Silicon Homebrew in `/opt/homebrew`. A librtlsdr from an
Intel Homebrew in `/usr/local` is x86_64 and can't be loaded by an arm64 receiver. The same goes
for cmake when building with speech. The SR30C cable (Silicon Labs CP210x) works with the driver
built into macOS and shows up as `/dev/cu.usbserial-*`; no extra driver is needed.

## Install (Debian or Ubuntu)

For a Proxmox container, follow `packaging/linux/lxc-setup.md`. Otherwise, after building:

    sudo bash packaging/linux/install.sh

This installs the binary in `/opt/railspotting-receiver/bin`, creates `receiver.json` from
`receiver.json.example` if there is none, downloads the speech model, and starts the
`railspotting-receiver` systemd service. Running it again updates the binary and keeps settings,
identity and history. On first start the receiver shows a link code (in the log and on the status
page at port 8080) to add it to an account.

## Files

| File | What it is | In git |
|---|---|---|
| `receiver.json` | settings; start from `receiver.json.example` | no |
| `credentials.json` | the receiver's private key, created on first run (mode 0600); see `credentials.json.example` for its shape. Never share it. | no |
| `state/` | identity, cached channel plan, event history, upload queue, detector templates, recordings | no |
| `models/` | speech model, `ggml-small.en-q8_0.bin` (or `ggml-small.en.bin`) from huggingface.co/ggerganov/whisper.cpp | no |

## Settings (`receiver.json`)

Anything left out takes the default shown in `receiver.json.example`.

| Key | Meaning |
|---|---|
| `server_url` | the headend |
| `label` | a name for this receiver on the headend |
| `state_dir` | where state lives, relative to `receiver.json` |
| `serial_port`, `baud`, `scanner_role` | the SR30C scanner: `"auto"` finds it, `"none"` turns it off. With `scanner_role` `"auto"`, EOT comes first: the scanner holds EOT, or HOT when both an EOT and a VHF dongle are connected |
| `transcription` | speech recognition of detector mileposts (needs the speech build and model) |
| `transcribe_other_calls` | also transcribe calls that are not detector announcements (slow on small CPUs) |
| `web` | the status page: `enabled`, `bind`, `port`. `settings_api: true` lets a tool on the same machine change settings through it, with the token the receiver writes to `state/web_token` |
| `auto_eot_focus` | when the scanner turns to EOT telemetry by itself |
| `sdr.devices` | dongles by serial with role (`vhf`, `eot`), gain and sample rate; empty means each role takes a connected dongle by itself, EOT first, then VHF (voice: detectors and road calls) |
| `sdr.margin_db`, `sdr.min_windows` | how far above the noise, and for how long, a signal must be to count |
| `sdr.record_bursts`, `sdr.record_keep`, `sdr.voice_clip_keep` | recordings kept in `state/` |
| `watch_clips` | channels (MHz) whose every recording is kept in `state/watch_clips`, for detectors that have no template yet |

## Tests

    cargo test -p <crate>

Fixture tests compare with recorded reference outputs in `fixtures/`. Tests that need private
recordings in `fixtures/local/` skip with a note when it is missing.

## License

Licensed under either of Apache License 2.0 (`LICENSE-APACHE`) or MIT (`LICENSE-MIT`) at your
option. librtlsdr is a separate GPL program used at run time and is not distributed with this
code.
