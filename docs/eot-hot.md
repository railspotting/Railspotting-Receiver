---
title: "EOT and HOT (457 and 452 MHz)"
permalink: /eot-hot/
---

Every freight train in North America carries a radio link between its two ends. The **end-of-train
device** (EOT) on the last car reports the brake pipe pressure and more to the locomotive, and the
locomotive's **head-of-train device** (HOT) can answer it, or command it to apply the brakes from
the rear in an emergency. Both send short data bursts on fixed UHF frequencies, all the time, which
makes them the most reliable sign that a train is near.

| | EOT | HOT |
|---|---|---|
| Frequency | **457.9375 MHz** | **452.9375 MHz** |
| Sends | status: unit ID, brake pressure, motion, marker light, battery | requests to the EOT, and the emergency command |
| How often | about once a minute, and whenever something changes | less often: when the locomotive asks, and in an emergency |
| What the receiver does | **decodes** every packet | **detects, times and records** each burst; doesn't decode it |

## The devices

- **EOT (also ETD, or "FRED"):** the box on the rear coupler of the last car, with a red flashing
  marker light. It replaced the caboose. It measures the train's **brake pipe pressure** at the rear,
  which tells the crew the air has reached the whole train, and reports whether the rear of the
  train is **moving**.
- **HOT (also HTD):** the unit in the lead locomotive. It is linked to one EOT by its **unit ID**,
  so a train listens only to its own rear device, even with other trains nearby. It can ask the EOT
  for its status, and in an emergency it can tell the EOT to **vent the brake pipe from the rear**,
  so the whole train brakes at once instead of from the front backwards.

Other railroad radio, and how these fit in with voice calls and defect detectors, is on
[Railroad radio basics]({{ '/radio-basics/' | relative_url }}).

## The signal

| | |
|---|---|
| Carrier | FM |
| Data | **1200 bits per second, FFSK**: an audio tone of **1200 Hz for a 1** and **1800 Hz for a 0** |
| A burst | a fraction of a second, holding one or more packets back to back |

## The EOT packet

```text
| preamble       | sync        | data (45 bits)                  | check (18 bits) |
| 0101 0101 ...  | 11100010010 | fields below, first bit first   | BCH, with a key |
```

1. **Preamble:** alternating bits (0101...) that let a receiver lock onto the timing.
2. **Sync word:** 11 bits, `11100010010`, that mark where the packet starts.
3. **Data:** 45 bits of fields.
4. **Check:** 18 bits of a **BCH(63,45)** code over the data, XORed with a fixed key. The 63-bit
   code word (data and check) can find and correct a few wrong bits.

### The fields

Numbers are sent lowest bit first.

| Bits | Field | Meaning |
|---|---|---|
| 0-1 | `chaining` | a 2-bit field, 3 in ordinary status packets |
| 2-3 | `battery_condition` | the EOT's battery state, as a code |
| 4-6 | `message_type` | the kind of packet, 0 in ordinary status packets |
| 7-23 | `unit_id` | the EOT's **unit number**, up to 131071, often written on the box |
| 24-30 | `brake_pipe_psi` | **brake pipe pressure** at the rear, psi (0 to 127) |
| 31-37 | `battery_charge_raw` | the battery's charge, as a raw value |
| 39 | `valve_circuit_ok` | the emergency valve's circuit is working |
| 40 | `confirmation` | the confirmation flag |
| 41 | `turbine` | the air turbine that charges the battery is running |
| 42 | `motion` | the rear of the train is **moving** |
| 43 | `marker_battery_weak` | the marker light's battery is low |
| 44 | `marker_light` | the marker light is on |

A typical packet from a train on the move: unit 75618, 89 psi, moving, marker light off, valve
circuit fine.

## How the receiver decodes EOT

A dongle with the **EOT job** watches 457.9375 MHz (and every other plan channel in its window).
When a burst on the EOT channel closes:

1. **FM demodulation**, then the two FFSK tones are told apart, giving a stream of bits.
2. The decoder looks for the **sync word** after the preamble, reading the bits at **eight timing
   offsets** in case the clock lands between bits.
3. The 63-bit word is checked against the BCH code:
   - a packet that checks is taken as it is;
   - **one wrong bit** is corrected when the preamble before it is clean;
   - **two wrong bits** are corrected only when two different timing offsets read the same
     packet, which keeps false decodes out of noise (two flips in pure noise "find" a packet about
     once in 130 tries).
4. The fields are read, and the event carries the **newest good packet** of the burst and how many
   packets it held.

A decode needs the burst roughly **9 dB above the noise**. Every EOT burst, decoded or not, is also
**saved as a recording** of the radio signal (`state/bursts/`), so a missed one can be looked at
later.

**How sure is a decode?** Each decode says how many bits were corrected. The headend says how many
it accepts (`eot_corrected_bits_max`, 0 or 1 today); a packet that needed more goes up as an
undecoded burst instead, so nothing doubtful reaches the map.

The decoder's check constants come from an open-source EOT decoder (github.com/ereuter); the
receiver's code is its own (`crates/dsp/src/eot.rs`), tested against recorded reference decodes.

## HOT

A dongle with the **HOT job** watches 452.9375 MHz. Each HOT burst is **detected, timed and
recorded**, with its strength, but not decoded. A HOT burst means a **lead locomotive** is near,
which an EOT alone can't tell you. Hearing both ends of one train, a few minutes apart, says which
way it was going.

## Scanner and dongle jobs

- **Dongles:** EOT is the first job filled by default, so a receiver with one dongle decodes EOT.
  HOT is the last job in the default order (EOT, ITC, VHF, HOT); a dongle can be given the HOT job
  directly. One UHF antenna serves both, through a splitter. See
  [SDR and receiver capabilities]({{ '/capabilities/' | relative_url }}) and
  [Antennas and RF]({{ '/antennas/' | relative_url }}).
- **The SR30C scanner** can listen to EOT or HOT too (it reports when and how strong, without
  decoding). With its job on automatic, it takes EOT when no dongle has it, then HOT. While
  scanning voice channels, a HOT burst or a strong call on a road channel makes it **park on EOT
  for 30 minutes** (`auto_eot_focus`), since a train is probably near.

## What goes to RailSpotting

- Each EOT burst is an **event**: when, which channel, how strong, and the decoded fields
  (unit ID, brake pressure, motion and the rest) when it decoded. HOT bursts are events with
  timing and strength. See [How reports reach RailSpotting]({{ '/reporting/' | relative_url }}).
- RailSpotting puts trains on the map from these. The unit ID helps follow one train as receivers
  along the line hear it.

## Where to see it

- The status page's **EOT** and **HOT** tabs: bursts over time, how often they decode at each signal
  level, corrected bits, and each unit heard.
- The **Events** list on the Status tab, with each decoded packet's fields.
- The console on Linux and the Mac app show the same.
