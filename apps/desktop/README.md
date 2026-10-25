# RailSpotting for macOS

The receiver as a menu bar app for Apple Silicon Macs. It runs the same engine as the Linux
receiver, in-process, and shows its dashboard in a window. The plan and the status are in
[MACOS_APP.md](MACOS_APP.md).

## Build and run

You need the Rust toolchain, Xcode's command line tools, librtlsdr and cmake from the Apple Silicon
Homebrew (`/opt/homebrew`), and the Tauri CLI:

    brew install librtlsdr cmake
    cargo install tauri-cli --version "^2" --locked

From this folder:

    cargo tauri dev                      # run it from the source tree
    cargo tauri build --bundles app      # RailSpotting.app in target/release/bundle/macos

To build it and put it in /Applications (Launchpad and Spotlight find it there), quit the app
if it runs, then from the repository root:

    packaging/macos/install-local.sh

The app is not in the workspace's `default-members`, so a plain `cargo build` at the root never
builds it. On Linux this crate builds an empty binary that says the app is for macOS.

## Installing a release

Download `RailSpotting-<version>-arm64.dmg` from the repository's GitHub Releases, open it and
drag RailSpotting to Applications. Until releases are notarized, the first open needs System
Settings, Privacy & Security, **Open Anyway**. The app then updates itself from the menu (Check
for Updates). How releases are made is in [packaging/macos/README.md](../../packaging/macos/README.md).

## Where it keeps things

| What | Where |
|---|---|
| `receiver.json`, `credentials.json`, `state/`, `models/` | `~/Library/Application Support/RailSpotting/` |
| Log | `~/Library/Logs/RailSpotting/receiver.log` |
| Launch at Login | `~/Library/LaunchAgents/` (only when turned on in the menu) |

The Mac is its own receiver. On first start it makes its own key and device ID and shows a link
code in the dashboard. Never copy `credentials.json` or `state/` from another receiver.

## How it behaves

- The window has two tabs:
  - **Receiver**: the engine's status page on a free port on 127.0.0.1. The app gives the page a
    token for this run only, which turns on its Settings card.
  - **Live Map**: the headend's page made for the app, `https://railspotting.com/embed/map`, for
    this receiver once it is linked. It loads only that page and its video frames; every link
    opens in the browser.

  They are separate web views, so the token never reaches railspotting.com.
- Closing the window keeps the receiver running in the menu bar, with no Dock icon.
- The menu bar icon shows receiving, attention (a dongle or the scanner missing, or not linked) or
  offline (headend unreachable, or paused). The menu has Open Dashboard, Live Map, Open
  RailSpotting.com in Browser, the state, Pause or Resume Receiving, Launch at Login and Quit.
- On first start it downloads the speech model (264 MB) and restarts the receiver when it is in.
  The menu shows the progress.
- Quit, Cmd-Q, logging out and shutting down all stop the receiver properly: queued events get a
  few seconds to upload, and the dongles and the scanner port are let go.
- Opening the app again while it runs brings up the window. `open -a RailSpotting --args --live-map`
  opens it on the Live Map tab.

## Trying it without touching anything real

Start the fake headend, then run the app with a scratch home folder, so it uses its own
Application Support and identity. Turn the scanner off with `"serial_port": "none"`, or the
receiver will program the SR30C's channels.

    cargo run -p railspotting-engine --example fake_headend -- /tmp/rs-url
    mkdir -p "/tmp/rs-home/Library/Application Support/RailSpotting"
    echo "{\"server_url\": \"$(cat /tmp/rs-url)\", \"serial_port\": \"none\"}" \
        > "/tmp/rs-home/Library/Application Support/RailSpotting/receiver.json"
    open --env HOME=/tmp/rs-home ../../target/debug/bundle/macos/RailSpotting.app

The fake headend approves every link code by itself.
