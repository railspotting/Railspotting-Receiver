---
title: "SDR and receiver capabilities"
permalink: /capabilities/
---

## RTL-SDR dongles

A receiver uses up to three RTL-SDR dongles, each with a **job**:

| Job | Listens to | Sample rate | Default gain | Antenna |
|---|---|---|---|---|
| **EOT** | the end-of-train channel (457.9375 MHz), decoded | 2.4 MS/s | 40.2 dB | UHF (the shorter one) |
| **VHF** | every voice channel in the plan, plus the reference station | 2.88 MS/s | 20.7 dB | VHF (the longer one) |
| **HOT** | the head-of-train channel (452.9375 MHz) | 2.4 MS/s | 40.2 dB | UHF |

- Each dongle sees a window about **2.2 MHz** (at 2.4 MS/s) or **2.65 MHz** (at 2.88 MS/s) wide,
  and hears **every plan channel in it at once**. The receiver centres the window to fit the
  channels with the most room, avoiding the centre spike every RTL-SDR has. A channel outside the
  window is listed as "uncovered".
- **Jobs are automatic:** the first dongle plugged in takes EOT, the second VHF, the third HOT. The
  receiver remembers which dongle (by serial) has which job. When a dongle is gone for more than a
  minute (thrown away, or swapped), its job goes to the next free dongle, so a replacement takes
  over within a minute without any setting.
- **Or chosen on the website:** the receiver's page on RailSpotting lists its dongles (by an
  anonymous ID) and lets the owner pick EOT, VHF, HOT or off for each.
- **Health checks:** clipping (gain too high), a stalled dongle (reopened after 10 s), and a
  "stuck" dongle sending random data (unplug it for 10 s) are all detected and shown.

## The reference station and radio checks

Each plan names a reference station: a **NOAA weather radio** transmitter that is always on. The
VHF dongle measures it continuously; every 30 minutes the receiver reports a **radio check** (it
passes at 10 dB or more above the noise), and warns if the signal stays weak. The status page
graphs it over days, which shows quickly when an antenna or cable goes bad.

## The Uniden SR30C scanner (optional)

A scanner connected with its USB cable (the CP2104 programming cable) adds a fourth "ear":

| | SDR dongle | Scanner |
|---|---|---|
| Channels at once | every channel in its window | one at a time, stepping through its memory |
| Detects | by signal level above the noise | by squelch (and level, when parked) |
| Audio, transcripts, EOT decoding | yes | no: only when and how strong |

The scanner's job is automatic too: it takes whatever no dongle covers. **EOT** first, then
**HOT** if a dongle has EOT, otherwise it **scans** the voice channels. In the scan job, a HOT
burst or a strong call on a road channel makes it park on EOT for 30 minutes. The website can set
its job instead (scan, EOT, HOT or off). The receiver programs the scanner's channel memory with
the plan.

## A receiver at a glance

| Kit | Covers |
|---|---|
| 1 dongle | EOT (decoded) |
| 2 dongles | EOT + all voice channels, detectors, radio checks |
| 3 dongles | EOT + voice + HOT |
| + scanner | fills what the dongles don't: EOT, HOT or the voice channels |

The receiver needs little computer: a 4-core Celeron with 2 GB RAM runs two dongles with speech
recognition. Speech recognition is the heaviest part; turn `transcription` off on very small
machines.
