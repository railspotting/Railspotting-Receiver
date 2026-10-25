# Protocol findings

What has been worked out from recordings, with the evidence. Public background: channels are 25 kHz
on the grid 217.5875 MHz + n x 25 kHz, the band in use is 220.1125 to 221.9875 MHz, and the
modulation is pi/4-DQPSK at 8 or 16 kilosymbols per second.

## Symbols

The demodulator gives one digit per symbol, the phase change from the previous symbol:
`0` = +45, `1` = +135, `2` = -135, `3` = -45 degrees. Settled (section 3): Gray coded, `0` = 00,
`1` = 01, `2` = 11, `3` = 10, bits sent first to last, bytes MSB first.

## 1. Rates and frequency offset (first recording, 60 s, two busy channels)

- A busy channel was mostly 16 ksym/s (278 of 331 bursts); a quiet one 8 ksym/s (5 of 5).
- Good bursts reach a quality of about 0.9 (1 is a perfect fit to pi/4-DQPSK).
- Every burst on both channels showed the same frequency offset, about -164 Hz: the dongle's own
  tuning error, which confirms the demodulator is locked.

## 2. Preamble and sync word

Across 54 good 16 ksym/s bursts and 36 good 8 ksym/s bursts, every one starts the same way:

    222222222 020212232332130212131300131203
    preamble  sync (30 symbols)

- **Preamble:** 9 symbols of `2` (52 of 54 and 36 of 36; the rest had a weak start).
- **Sync:** the same 30 symbols in 100% of bursts, at both rates.
- After these 39 symbols the bursts differ: the header and data start there.

## 3. The link layer, decoded

Public notes on the protocol (the OpenITC project's documentation, GPL-2.0; facts only, none of
its code was read or used) describe the layers below. Each was confirmed on our recordings, then
implemented from scratch in this crate (`link`).

- **Unique word:** 64 bits, `f337eeb637660672`. In our symbols it starts at symbol 7, so the
  preamble is 7 symbols of `2` and the 30-symbol "sync" of section 2 is the unique word's tail.
  It matched with 0 bit errors once the Gray map above was used (no other map matches).
- **Header:** 24 bits as 48, K=7 rate 1/2 convolutional code, taps 0x6D and 0x4F, the newest bit
  entering at the bottom of the register. The encoder does not start from zero (it runs on from
  the unique word), so the Viterbi decoder starts with every state equal. Fields: length (9 bits),
  mode (4, always 1), type (8), and 3 check bits: the 24-bit value divides by x^3 + x + 1.
- **Data:** `length` bytes (message, then CRC-16/ARC, low byte first) in ceil(length/12) RS(16,12)
  blocks over GF(256) (polynomial 0x11d, roots alpha^0..alpha^3), sent byte-interleaved: column by
  column across the blocks, skipping positions past the end; the short last block's parity
  follows its data. RS corrects up to 2 bytes a block.
- **Message:** types 5 and below carry `length - 2` bytes; above 5 a 4-byte trailer (thought to
  be an authentication code) comes before the CRC.
- **Many frames per burst:** a TDMA burst can hold several frames back to back. The decoder looks
  for the unique word again after each one. Frames in a long burst can sit best at different
  sampling phases, so every phase is tried and good frames are merged.

Message fields understood so far (`msg`): beacon (base radio, position, local channel,
utilisation, nearby channels), TIME (UTC date and time, checked against the clock), CNTL (base
radio, sequence), WIU status 0x33 and 0x34 (wayside radio and indicator bits).

## 4. Against the reference decoder

ITCMON's decoder was run in a Linux container, fed through a tap that saved exactly what it received, so both decoders saw the
same samples (185 s, two channels):

| | ITCMON | ours, first | ours, now |
|---|---|---|---|
| messages | 1732 | 1825 | 1924 |
| of ITCMON's, also ours | | 1642 (94.8%) | 1713 (98.9%) |

ITCMON's listener started a few seconds after the recording, which accounts for part of the
messages only we have. Every message type matched.

What closed most of the gap (each measured on the same recording):

| Change | ITCMON's we missed |
|---|---|
| first version | 90 |
| gate samples against the channel's noise floor, not 30% of the burst's peak | 53 |
| demodulate each transmission in a long burst on its own (rate, timing, frequency), as well as the whole burst | 33 |
| time frames from the sample of their first symbol (they were off by the lead-in) | 32 |
| allow 8 bit errors in the unique word, not 4 | 26 |
| soft decisions: each symbol's confidence; RS blocks that will not correct try their least reliable bytes as erasures | 19 |

A long burst is often several wayside units in consecutive TDMA slots, at different levels and
with different frequency errors; most misses came from treating it as one transmitter. Of the
frames rescued with three or more repaired bytes, all but one came from known units with the
usual label byte; that one had a correct payload under a damaged header type (reported as
OTHER). The header's 3-bit check is its only protection.

The 19 left: 14 bursts with no frame found, 5 frames that fail the CRC.

## 5. Wayside status layout

Public notes describe a WIU message as a 24-bit radio ID and indicator bits. On our recordings
(1452 messages, types 0x33 and 0x34) the payload is laid out differently:

| Bytes | What | Evidence |
|---|---|---|
| 0-2 | a group, the same for every unit heard | constant across 31 units |
| 3-4 | the unit | 31 values, each with its own steady indicator bytes |
| 5 | a label | always 0x82 |
| 6-7 | a counter | different in nearly every message from the same unit |
| 8- | indicator bytes, 1 to 5 depending on the unit | steady per unit; changed once in 185 s on one unit (two bits) |

`msg` reports `wiu` (bytes 0-4), `group`, `label`, `seq` and `indicators`.

### Indicator bytes are 5-bit device codes

Read as 5-bit fields, each field last bit first (as public notes describe for signals), every
unit's indicators become a short list of codes plus a few spare bits, and every change seen
(9 in about 20 minutes, 8 units) changes whole fields:

| Unit kind | Codes seen | Changes seen |
|---|---|---|
| 2-byte indicators | 8, 13 in the first two fields; 0 or 1 in the third | 13 to 8 (both fields together, twice), 8 to 13 (one field) |
| 3 to 5 bytes | 15, 3, then a last field of 6 (or 26, 10, 7) that never changed | 3 to 15 only (four units) |

Neighbouring units changed one after another (13 to 8 on one unit, the same 48 s later on the
next), as signals do when a train passes.

### What the codes mean

A railroad's Wayside Message Tester manual (v1.6, public in FRA docket FRA-2010-0028-0035; read
for facts, not kept in this repository) shows the status numbers a wayside unit reports:

| Device | Codes shown |
|---|---|
| Signal | 2 Medium Clear, 3 Clear, 6 Approach Medium, 8 Approach (examples; not the full table) |
| Switch | 0 Out of Correspondence, 1 Reverse, 2 Normal |
| Detector | 0 Off, 1 On |

Every unit also has a device status table ID, so the full meaning is per unit. With the
5-bit fields read last bit first, the two codes that come and go on recordings are exactly the
documented 3 (Clear) and 8 (Approach); read first bit first they would be 24 and 22, which are
not. That supports the bit order. It also explains code 3 resting for minutes: automatic signals
show Clear while the track ahead is empty and drop when a train passes, and every change seen in
the longer units is 3 to 15 (Clear to, probably, Stop; 15 is not in the examples). In the 2-byte
units 13 to 8 and 8 to 13 fit Stop and Approach as a train moves on, with 13 unnamed.

Not settled: the size of the fields after the signals (the last field never changes; it may be
switches, 2 bits, or detectors, 1 bit, which a 5-bit split cannot show), and codes 13, 15, 26, 10
and 7. `msg::signal_aspect` names only the documented codes; the page adds "?" on
fields not yet seen to change.

The 2 bytes after the label change in every message; their low byte rises roughly with time,
but they are not a plain clock.

## Next

- The last 1%: soft-decision Viterbi for the header, timing tracking within long frames.
- Work out what the WIU indicator bits mean, CNTL slot data and type 0x07.
