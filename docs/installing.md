---
title: "Installing"
permalink: /installing/
---

There are no prebuilt packages yet: the receiver is built from source with Rust. Pick the setup
that fits:

| Setup | For |
|---|---|
| [Debian or Ubuntu machine](#debian-or-ubuntu) | a small PC or mini server next to the antennas |
| [Proxmox container](#proxmox-lxc-container) | a home server already running Proxmox |
| [macOS app](#macos) | an Apple Silicon Mac (in development) |

Whatever you choose, the machine needs **a working clock** (NTP) and internet access to
`api.railspotting.com`.

## Debian or Ubuntu

```bash
sudo apt install -y git curl build-essential cmake clang pkg-config
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
git clone https://github.com/railspotting/Railspotting-Receiver.git
cd Railspotting-Receiver
cargo build --release -p railspotting-receiver --features speech
sudo bash packaging/linux/install.sh
```

- `--features speech` adds speech recognition for defect detectors (needs cmake and a C/C++
  compiler). Leave it out for a lighter build without it. On a small machine the first build takes
  10 to 20 minutes; `CARGO_BUILD_JOBS=2` keeps memory use down.
- The installer:
  - installs librtlsdr;
  - creates a `railspotting` service user;
  - installs to `/opt/railspotting-receiver`;
  - copies `receiver.json` from the example;
  - downloads the speech model (about 265 MB);
  - starts the `railspotting-receiver` systemd service.

  Running it again later updates the program and keeps settings, identity and history.
- Logs: `journalctl -u railspotting-receiver -f`. Status: `railspotting-receiver status`, or the
  status page at `http://<machine>:8080/`.

Then **link it**: the log and the status page show a link code. Enter it at
[railspotting.com/link](https://railspotting.com/link) (see [How reports reach RailSpotting]({{ '/reporting/' | relative_url }})).

## Proxmox LXC container

The full guide is [packaging/linux/lxc-setup.md](https://github.com/railspotting/Railspotting-Receiver/blob/main/packaging/linux/lxc-setup.md).
In short:

1. Create an unprivileged Debian 12 container: 2 cores, 2 GB RAM, 8 GB disk, started at boot.
2. On the **Proxmox host**, run `bash packaging/linux/proxmox-host-setup.sh <ctid> [--scanner]`.
   It keeps the kernel's DVB-TV driver off the dongles, adds udev rules, and passes the USB devices
   (and the scanner's serial cable, with `--scanner`) into the container.
3. In the container, build and install as above.
4. Keep the container's MAC address if you ever recreate its network interface.

Within five minutes the log should show a `signal: NOAA ... +36 dB` line: above +20 dB means the
radio path is healthy.

## macOS

The macOS menu bar app is in development and runs on Apple Silicon (macOS 13 or later). Build it:

```bash
brew install librtlsdr cmake
cargo install tauri-cli --version "^2" --locked
git clone https://github.com/railspotting/Railspotting-Receiver.git
cd Railspotting-Receiver
packaging/macos/install-local.sh
```

It installs `RailSpotting.app` to /Applications. It shows the status page and the live map, downloads the
speech model on first start, and can start at login. Its files are in
`~/Library/Application Support/RailSpotting/` and its log in `~/Library/Logs/RailSpotting/`. The
Mac links as its own receiver: never copy `credentials.json` or `state/` from another one.

## Settings

Most receivers need no settings: dongle jobs are assigned automatically (or chosen on the
website), and the channel plan comes from RailSpotting. `receiver.json` holds the rest; see
[receiver.json.example](https://github.com/railspotting/Railspotting-Receiver/blob/main/receiver.json.example).
Common ones:

| Key | Default | |
|---|---|---|
| `transcription` | `true` | speech recognition for defect detectors |
| `transcribe_other_calls` | `false` | transcribe every voice call (slow on small CPUs) |
| `sdr.margin_db` | `6.0` | how far above the noise a channel must rise to open |
| `sdr.devices` | `[]` | pin dongles to jobs by serial, with gain and name |
| `scanner_role` | `"auto"` | `scan`, `eot` or `hot` to fix the scanner's job |
| `web.port` | `8080` | the status page |

## Troubleshooting

| Symptom | Fix |
|---|---|
| "no RTL-SDR dongles found" | run the Proxmox host script, or check the USB cable |
| `usb_claim_interface error -6` | the DVB-TV kernel driver has the dongle: blacklist it (the host script does) |
| "cannot load the RTL-SDR driver" | install librtlsdr (`librtlsdr0` or `librtlsdr2`; `brew install librtlsdr` on a Mac) |
| short reads, "dongle lost" | USB power: use a powered hub |
| two dongles "can't be told apart" | give each its own serial: `rtl_eeprom -d <index> -s <serial>` with the service stopped, then replug |
| clipping above 0.1% | gain too high, or strong FM stations: lower the gain or add an FM filter ([Antennas and RF]({{ '/antennas/' | relative_url }})) |
| upload timeouts | DNS or routing to api.railspotting.com |
