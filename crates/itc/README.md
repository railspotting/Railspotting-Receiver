# railspotting-itc

ITC (Interoperable Train Control) decoding for the RailSpotting receiver: the 220 MHz radio system
that carries positive train control (PTC) messages. Receive only.

| Module | What |
|---|---|
| `dsp` | the channel plan (217.5875 MHz + n x 25 kHz), a channelizer, burst finding, pi/4-DQPSK demodulation with per-symbol confidence |
| `link` | unique word, convolutional header (Viterbi), RS(16,12) de-interleaving and correction with erasures, CRC-16/ARC |
| `msg` | fields of the message types that are understood (beacon, TIME, CNTL, wayside status) |
| `decode` | `Decoder`: blocks of samples in, bursts out; `decode_burst`: a burst's messages as JSON |
| `recording` | `.cu8` recordings with a `.json` sidecar |
| `feed` | messages over TCP for a monitor |
| `survey` | the tally and report behind `railspotting-receiver itc survey` |

The receiver uses it in two places: `dsp::itc_rx` (a dongle with the `itc` role) and
`engine::itc` (the `itc` commands).

- `messages.md`: the decoded messages' fields, read by RailSpotting-ITC-Monitor.
- `findings.md`: how the protocol was worked out, with the evidence, and how this decoder compares
  with a reference decoder on the same recording (98.9% of its messages).

Tests use synthetic signals only, never real recordings: decoded traffic is kept off git.
