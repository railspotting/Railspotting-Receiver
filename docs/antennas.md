---
title: "Antennas and RF"
permalink: /antennas/
---

> The receiver's code says little about antennas: this page is **general RF guidance** for the
> frequencies it uses, not tested recommendations. Results depend a lot on where you are.
> Reports of what works for you are welcome as issues.

The receiver listens on two bands:

| Band | Frequencies | Used for |
|---|---|---|
| **VHF** | about 160 to 162.6 MHz | voice channels, defect detectors, the NOAA reference station |
| **UHF** | 452.9375 and 457.9375 MHz | HOT and EOT |

## Antennas

- **Height beats everything.** An antenna outside and above the roofline hears trains several
  times further than one indoors. EOT devices transmit from track level with little power, so
  every metre of height helps.
- **Vertical polarisation.** Railroad radios use vertical antennas; mount yours vertically.
- **Two antennas** are simplest: a VHF one for the voice dongle (the longer one) and a UHF one for
  the EOT and HOT dongles (the shorter one). A quarter-wave whip is about **46 cm at 160 MHz** and
  about **16 cm at 457 MHz**; it needs a ground plane (a metal roof, or radials).
- **One dual-band antenna** also works: amateur 2 m / 70 cm dual-band verticals cover the area
  well enough, though they are tuned a little lower (146 and 440 MHz).
- The small whips that come with dongles work on a desk for testing, poorly in practice.

## Sharing one antenna: diplexer or splitter

The receiver's dongles are named for which antenna to connect: "the VHF antenna (the longer one,
or the **diplexer's low-band port**)" and "the UHF antenna (the shorter one, or the **diplexer's
high-band port**)".

| | Loss | Use it for |
|---|---|---|
| **Diplexer** (VHF/UHF) | well under 1 dB per port | one dual-band antenna feeding the VHF dongle and a UHF dongle. Check the low-band passband reaches **162.6 MHz**: some amateur diplexers stop at 148 to 150 MHz and lose a few dB at railroad frequencies. |
| **Passive splitter** (2-way) | about 3.5 dB per port | two dongles on the same band, for example EOT and HOT from one UHF antenna. A 4-way splitter loses about 7 dB. |

A splitter in front of a diplexer, or a long run after it, adds up: keep the chain short.

## Coax

Loss grows with frequency and length, and it hurts most at UHF:

- keep runs short;
- use low-loss coax (LMR-240 or LMR-400 class, or RG-6 for short runs) rather than thin RG-58 or
  RG-174;
- RTL-SDR dongles usually have an SMA (or MCX) connector, so plan the adapters.

## Preamps (LNAs)

A low-noise amplifier **at the antenna** makes up for a long coax run. It can also overload the
dongle near strong transmitters. The receiver does not switch dongles' bias-tee power on, so
power an LNA separately, or turn the dongle's bias tee on with its own tools if it has one.

## FM broadcast filters

Strong FM radio stations (88 to 108 MHz) can overload the VHF dongle, raising its noise and
causing false openings. The receiver reports **clipping**: above about 0.1%, lower the gain or add
an **FM broadcast band-stop filter** in front of the VHF dongle. Pagers and other strong
transmitters near 150 to 160 MHz can do the same; a better antenna position often helps more than
a filter.

## Gain

| Dongle | Default | Lower it when | Raise it when |
|---|---|---|---|
| VHF | 20.7 dB | clipping is reported, or many channels open at once | the weather station's signal is weak and the antenna is small |
| UHF (EOT, HOT) | 40.2 dB | the noise floor jumps, or there are false bursts | bursts are heard but rarely decoded |

Set gains per dongle in `receiver.json` (`sdr.devices`) or on the Mac status page.

## USB

- **Power:** dongles draw a few hundred milliamps each. Two or three on one computer often need a
  **powered USB hub**; "short reads" or dongles dropping off are the usual sign.
- **Noise:** computers and USB 3 ports are noisy at radio frequencies. A short USB extension cable
  that moves the dongles away from the computer, and ferrite clips on cables, can lower the noise
  floor noticeably.
- **Serials:** dongles from the same batch often share a serial number. The receiver can't tell
  them apart; give each its own with `rtl_eeprom -d <index> -s <serial>`.
