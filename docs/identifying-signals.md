---
title: "How the receiver identifies signals"
permalink: /identifying-signals/
---

## The channel plan

The receiver doesn't guess frequencies. RailSpotting sends each receiver a **channel plan** for
its area (`GET /api/v1/config`): every channel's frequency, railroad, label (road, dispatcher,
yard, EOT, HOT), kind (`voice` or `telemetry`) and whether it is a **proximity** channel. The plan
also names a **reference station** (a NOAA weather radio transmitter) used to check that the
radio works. The receiver keeps the last plan it got, so it works through a restart without the
network, and picks up a new one when RailSpotting changes it. The website's receiver page lets the
owner turn plan channels off or add a few of their own.

## Hearing many channels at once

An RTL-SDR dongle receives a slice of spectrum about **2.2 to 2.6 MHz** wide. The receiver turns
it into a spectrum every 20 ms or so (a 4096-point FFT) and measures the power in each plan
channel (±5 kHz). One dongle therefore watches **every channel in its window at the same time**: a
VHF dongle covers all of an area's road, dispatcher and yard channels plus the weather station;
a UHF dongle covers EOT (or HOT).

For each channel the receiver tracks the **noise floor** (the quiet level over the last 30 s).

- A channel **opens** when it is at least **6 dB** above its floor for two windows in a row.
- It **closes** after about 0.3 s below that.
- Each open and close is an **event**, with its start and end time and signal strength. A call is
  cut at 120 s, a data burst at 3 s.

The receiver keeps the 0.3 s before each opening, so recordings and decodes include the start.

## Voice calls

On voice channels the receiver demodulates narrow-band FM to audio (300 to 3400 Hz) for the
calls it needs to listen to: defect detector announcements, and, if turned on, other calls for
transcription. Every call is reported as an event either way.

**Interference** that opens many channels at the same instant (a nearby transmitter, a noisy USB
port) looks like calls everywhere at once. On channels watched for detectors, a call that opens
together with more than one other channel within a second is dropped as interference.

## End-of-train (EOT)

[EOT and HOT]({{ '/eot-hot/' | relative_url }}) has the full story: the devices, the signal, the
packet and its fields. In short, when a burst on the EOT channel closes, the receiver **decodes** it:

1. FM demodulation, then the two tones of 1200-baud FFSK (1200 Hz for a 1, 1800 Hz for a 0).
2. It looks for the **sync word** after the alternating preamble, at eight timing offsets.
3. The 63-bit packet is a BCH(63,45) code word: 45 data bits and 18 check bits.
   - A packet that checks is taken as is.
   - One wrong bit is corrected behind a clean preamble.
   - Two wrong bits are corrected only when two different timing offsets read the same packet,
     which keeps false decodes out of noise.
4. The fields: **unit ID**, **brake pipe pressure** (psi), **motion**, marker light, marker battery,
   battery condition and charge, valve circuit, confirmation, turbine, message type.

A burst can hold several packets; the event carries the newest good one and the packet count.
Every EOT burst is also **saved as a recording** (the radio signal itself, `state/bursts/`), so a
missed decode can be looked at later. Weak bursts often can't be decoded: the signal needs to be
roughly 9 dB or more above the noise.

## Head-of-train (HOT)

HOT transmissions on 452.9375 MHz are **detected, timed and recorded**, but not decoded. They are
heard by a third dongle in the HOT role, or by the scanner. A HOT burst is a strong hint that a
lead locomotive is near, and can make the scanner watch EOT for a while.

## Defect detectors

Detectors are recognised in layers:

1. **By sound.** For detectors the receiver has a **template** for (built from a few recordings),
   it compares each call's sound: the opening identifies the detector, the ending must match its
   "no defects" all-clear, and the rhythm and length must fit. A clear match is an all-clear
   (method `sound`).
2. **By words.** With speech recognition built in (whisper.cpp, an English model of about 265 MB),
   the receiver transcribes the whole announcement, using the detector style's **voice profile**
   (its wording and how its voice is usually misheard). It reads the **milepost** from the words:
   within 7 miles of the template's milepost it is that detector, otherwise it looks the milepost up
   in RailSpotting's list of detectors. Defect words outside the all-clear phrase make it a
   **defect** report.
3. **Watched channels.** For railroads with no template yet, the receiver watches the channels
   RailSpotting says nearby detectors use, transcribes calls of 5 to 45 seconds, and reports them as
   transcripts. Their recordings are kept to build a template later.

A detector usually repeats itself 10 to 40 seconds later; the second announcement is marked as a
repeat of the first.

Without speech recognition, the old rule applies: a call longer than the all-clear whose ending
didn't match is reported as a possible defect.
