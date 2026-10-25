---
title: "ITC (220 MHz PTC)"
permalink: /itc/
---

**ITC** (Interoperable Train Control) is the 220 MHz radio network that carries **positive train
control** (PTC) messages on North American freight railroads. A receiver with a dongle in the
`itc` job decodes it: the base stations around it, and the **wayside units** at signals and
switches reporting their state every few seconds.

> ITC describes railroad operations: signal states show where trains are. What the receiver
> decodes stays on the receiver unless its owner turns ITC upload on, and RailSpotting never shows
> it publicly (see [What is shared](#what-is-shared)).

## What is on the air

PTC is a safety system that can stop a train before it passes a red signal, overspeeds or enters
work limits. Its radio link, ITC, connects four kinds of station:

| Station | What it sends |
|---|---|
| **Base stations** | fixed towers that run the network: beacons naming themselves, the time, and slot control for everyone else on their channel |
| **Wayside interface units** (WIUs) | boxes at signals, switches and detectors. Each reports the state of its devices every few seconds, whether or not anything changed |
| **Locomotives** | the train's PTC computer asking for wayside states and talking to the back office |
| **Back office** | the railroad's servers, reached through the bases |

A receiver hears the bases and the wayside units best: they are fixed, and they transmit all the
time. Locomotive traffic comes and goes with trains.

## Channels

- Channels are **25 kHz** apart on a grid: channel *n* is at **217.5875 MHz + *n* x 25 kHz**.
  Channel 114, for example, is 220.4375 MHz. The band in use runs from about 220.1 to 222.0 MHz.
- **Channel 102 (220.1375 MHz) is the national channel.** Every base station sends a beacon there
  about every 10 seconds, naming itself, its position and its **local channel**, where its own
  traffic (and its wayside units) are.
- So a receiver only needs to listen to 102 to find the local channels around it, then add them.

## The radio layer

| | |
|---|---|
| Modulation | pi/4-DQPSK: each symbol is a phase change of +45, +135, -135 or -45 degrees, carrying 2 bits (Gray coded: 00, 01, 11, 10) |
| Rates | **16,000 symbols/s** (full rate, 32 kbit/s) on busy channels, **8,000** (half rate) on quiet ones |
| Access | TDMA: stations take turns in time slots, so one burst on the air can hold several stations' messages back to back |
| Start of a burst | a short preamble, then a 64-bit **unique word** (`f337eeb637660672`) that marks where a frame begins |

## The link layer

After the unique word, each frame has:

1. **A header**, 24 bits sent as 48 with a convolutional code (constraint length 7, rate 1/2) for
   protection. It holds the frame's **length** (9 bits), a mode (4 bits), the **message type** (8
   bits) and a 3-bit check.
2. **The data**, `length` bytes: the message, then a **CRC-16** (CRC-16/ARC, low byte first). It is
   sent in blocks of a **Reed-Solomon RS(16,12)** code, which can repair up to 2 bad bytes in
   every 12, byte-interleaved across the blocks so a short burst of noise is spread over several.
3. For message types above 5, **a 4-byte trailer** before the CRC, thought to be an authentication
   code.

A burst can hold several frames, so the decoder looks for the unique word again after each one.

## Message types

| Type | Name | From | What the receiver reads |
|---|---|---|---|
| 0x02 | `BEACON` | base | base radio ID, position, local channel, neighbouring bases' channels, how busy the channel is |
| 0x05 | `CNTL` | base | base radio ID and a sequence number (slot control, not yet understood) |
| 0x06 | `TIME` | base | the base's UTC clock |
| 0x33 | `WIU` | wayside unit | the unit's ID and its device states |
| 0x34 | `WIU_REPLY` | wayside unit, relayed | the same, sent in reply to a request; it can arrive a cycle late |
| others | `QSTAT`, `ACK`, `ACQ`, `T07`, `UNICAST` | various | the sender and the raw bytes |

## Wayside status

A wayside unit's ID is **10 hex digits**: a 6-digit group shared by a railroad's units in an
area, then the unit's own 4 digits. Units numbered one apart usually share a location, often the
two directions at one signal.

Its message carries **indicator bits**. Read as **5-bit device codes** (each field last bit
first), they become one code per device:

| Code | Meaning |
|---|---|
| 3 | Clear |
| 8 | Approach |
| 2 | Medium Clear |
| 6 | Approach Medium |
| 15 | seen at control points; by observation a stop, held until the dispatcher clears the route |
| 13 | seen at intermediate (automatic) signals; by observation a stop that clears by itself as the train moves on |

Codes 2, 3, 6 and 8 are named in public railroad material. The rest are set by each railroad's
**device status table**, so the receiver shows only those four by name. Switches (normal, reverse,
out of correspondence) and detectors (on, off) report codes of their own.

A unit repeats its full status every few seconds, so almost every message is a repeat. On one
receiver over 12 hours: about **35,000 messages an hour**, of which about **43 were state
changes**. The receiver therefore tracks each unit and keeps **only its changes**:

- A unit's own status message (0x33) decides its state. A relayed reply (0x34) counts only when the
  unit's own message hasn't been heard for 20 s, because replies can arrive a cycle late and would
  make the state flicker back.
- A **starting state** is recorded when a unit is first heard, and after a gap in reception of 60 s
  or more.
- Every change counts, however short: a signal that clears for under a second and goes back is two
  changes.

## How the receiver decodes it

All of this was worked out from recordings and implemented from scratch in the receiver (the
`railspotting-itc` crate):

- A **channelizer** splits the dongle's window into its 25 kHz channels, and a **burst finder**
  per channel picks out transmissions against that channel's noise floor.
- Each transmission in a burst is **demodulated on its own** (rate, timing, frequency error),
  since consecutive TDMA slots are different stations at different levels.
- **Soft decisions:** every symbol carries a confidence. When a Reed-Solomon block won't correct,
  its least reliable bytes are tried as erasures, which doubles what it can repair.
- Against a reference decoder fed the same recording, the receiver decodes **98.9%** of its
  messages, plus some it misses.

The details, with the evidence for each step, are in
[`crates/itc/findings.md`]({{ site.repo }}/blob/main/crates/itc/findings.md). The decoded messages'
JSON fields are in [`crates/itc/messages.md`]({{ site.repo }}/blob/main/crates/itc/messages.md).

## Setting up a dongle for ITC

- **Hardware:** one more RTL-SDR dongle and a **220 MHz antenna**: a vertical whip about **33 cm**
  (a quarter wave) long, or any antenna made for 220 MHz. A VHF railroad antenna hears it poorly.
  See [Antennas and RF]({{ '/antennas/' | relative_url }}).
- **The job:** by default free dongles take jobs in the order **EOT, ITC, VHF, HOT**, so ITC is the
  second job filled. The order, and a job for each dongle, can be set on the website, in the
  receiver's console or Mac app, or in `receiver.json` (see
  [SDR and receiver capabilities]({{ '/capabilities/' | relative_url }})).
- **Defaults:** 220.3 MHz centre, 1.024 MS/s (a window about 1 MHz wide, roughly channels 90 to
  128), 40.2 dB gain. It starts on channel 102 and adds the local channel of every base it hears in
  its window. `channels` in the dongle's settings fixes the list instead.
- **Survey first.** Before choosing where the window goes, see what is on the air here (stop the
  receiver first, or name a dongle it isn't using with `--serial`):

      railspotting-receiver itc survey

  It listens to the whole band for 60 seconds and reports the bases heard (from their beacons),
  each channel with traffic (bursts, messages, decode quality, level, wayside units), clipping,
  and the command to decode what it found.
- **Other commands:** `itc decode --auto` finds the channels and prints decoded messages as JSON
  lines; `itc record --out a.cu8` saves raw samples; `itc decode --input a.cu8` decodes them later.

## Where to see it

- **The status page's ITC tab:** state changes over time, each unit and its changes, the base
  stations and how busy they are, message counts by type and channel, gaps in reception, and a
  **rail layout**: signal locations in the order trains pass them, worked out from the order
  their states change.
- **The console** on Linux (`sudo railspotting-console`) and the **Mac app** show the receiver,
  the same page and its settings.
- **On the receiver:** every decoded message goes to `state/itc/YYYY-MM-DD.jsonl` (kept 7 days),
  and each state change to `state/itc/changes/`. A local feed on `127.0.0.1:8221` serves the
  messages live to a monitor program on the same machine.
- **The Android app** has an ITC mode of its own for a dongle on a phone.

## What is shared

- **Nothing, by default.** Decoded ITC stays on the receiver.
- **When the owner turns ITC upload on** for the receiver on the website, the receiver sends each
  wayside unit's **state changes** and the **base stations** it hears, never the repeats (contract
  v1.18). Turned off again, it sends nothing more and drops what was waiting.
- The heartbeat carries a short ITC summary (channels, bases, units heard, message counts) while a
  dongle does ITC, so the owner can see it is working.
- **RailSpotting never makes ITC public**: not on the map, history, trains, streams or public
  feeds. A receiver's owner sees what their receivers reported; admins see the combined picture.
- The receiver's own status page is reachable by anyone on its network and shows ITC states. Keep
  it on a network you control.

## Credits

- The link layer follows public notes from the OpenITC project (facts only; none of its code was
  used), each confirmed on recordings before it was implemented.
- The status code names come from a railroad's Wayside Message Tester manual, public in FRA docket
  FRA-2010-0028.
