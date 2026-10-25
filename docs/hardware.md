---
title: "Tested hardware"
permalink: /hardware/
---

What RailSpotting receivers run on today. If you've tried something else, please report it (as an
issue) with what worked and what didn't.

## Radios

| Hardware | Status | Notes |
|---|---|---|
| **Nooelec NESDR SMArt v5** (RTL2832U, R820T2) | in use | EOT, ITC, VHF and HOT jobs; the reference dongle |
| Other RTL2832U dongles with an R820T, R820T2 or R828D tuner | expected to work | the receiver supports any dongle librtlsdr does; tuner names E4000, FC0012, FC0013 and FC2580 are recognised too |
| **Uniden SR30C** scanner, with its **CP2104** USB programming cable | in use | optional. Works with Linux's and macOS's built-in drivers (no extra driver) |

## Computers

| Hardware | System | Notes |
|---|---|---|
| Intel Celeron J4125 mini PC (4 cores, 2 GB RAM for the container) | Proxmox VE, Debian 12 LXC | two dongles and speech recognition, in production since October 2026 |
| Apple Silicon Mac | macOS 13 or later | the menu bar app (in development) |
| Google Pixel 9a | Android 17 | the [Android app](https://railspotting.github.io/Railspotting-Android), one dongle over USB OTG |

## Not supported

- **Intel Macs**: the macOS app is for Apple Silicon only.
- **32-bit systems**: not tested.
- **Raspberry Pi**: not tested yet. A Pi 4 or 5 should run the receiver, but speech recognition
  will be slow on it.
