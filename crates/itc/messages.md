# Decoded messages

Decoded ITC messages are JSON objects, one per line. The receiver writes them to
`state/itc/YYYY-MM-DD.jsonl` and its local feed when a dongle has the `itc` role, and
`railspotting-receiver itc decode` writes them to stdout (and to `--log` and `--serve`). This is the interface other
programs build on, such as RailSpotting-ITC-Monitor. Field names and meanings here are kept
stable; new fields may be added, so readers should ignore fields they do not know.

By default only messages that pass the CRC are written. With `--all`, failed ones are written too,
with `"crc_ok": false` and no further guarantees about their fields.

Decoded messages describe railroad operations. Keep them on machines you control.

## Every message

| Field | Type | Meaning |
|---|---|---|
| `t` | number | Unix seconds (UTC) the frame started, to the millisecond |
| `ch` | integer | ITC channel number: 217.5875 MHz + `ch` x 25 kHz (114 is 220.4375 MHz) |
| `ch_mhz` | number | The channel's frequency, MHz |
| `rate` | string | `"FR"` (16 ksym/s) or `"HR"` (8 ksym/s) |
| `type` | string | `QSTAT`, `BEACON`, `ACK`, `ACQ`, `CNTL`, `TIME`, `T07`, `WIU`, `WIU_REPLY`, `UNICAST` or `OTHER` |
| `kind` | integer | The header's message type byte (`type` is its name) |
| `len` | integer | The header's length: message bytes plus CRC (and trailer) |
| `crc_ok` | boolean | The CRC-16 checked out |
| `corrected` | integer | Bytes the RS(16,12) code corrected |
| `level_db` | number | The burst's peak above the channel's noise floor, dB |
| `hex` | string | The message's bytes, without CRC or trailer, in hex |
| `trailer` | string | Types above 5 only: the 4 bytes before the CRC, hex (thought to be an authentication code) |
| `radio` | string | A base or radio ID, 6 hex digits, where the type carries one (see below) |

## By type

**BEACON** (0x02), sent by a base station on its own channel and the national channel:
`radio`, `flags`, `lat` and `lon` (degrees), `channel` (the base's local channel), `beacon_rate`,
`util`, `channels` (nearby bases' channels).

**TIME** (0x06): `radio`, `utc` (an ISO 8601 time from the base's clock).

**CNTL** (0x05): `radio`, `seq`, `data` (hex; slot allocation, not yet understood).

**WIU** (0x33, a wayside unit's own status) and **WIU_REPLY** (0x34, a status relayed in reply to
a request; it can arrive a cycle late, so a unit's own recent WIU message outranks it):

| Field | Meaning |
|---|---|
| `wiu` | The unit, 10 hex digits: `group` then the unit's own 4 |
| `group` | 6 hex digits shared by many units |
| `label` | A byte, always 0x82 so far |
| `seq` | A 16-bit value that changes every message |
| `indicators` | The indicator bytes, hex |
| `bits` | The indicator bits as a string of `0` and `1` |
| `codes` | The indicator bits as 5-bit device codes, each read last bit first |
| `aspects` | For each code, its signal aspect name where public material gives one (2 Medium Clear, 3 Clear, 6 Approach Medium, 8 Approach), otherwise `null` |
| `spare` | Bits left over after the last whole code |

On recordings, code 15 is a control point's stop and 13 an intermediate signal's (section 5 of
`findings.md`); meanings are set per railroad and are not in the message.

**QSTAT, ACK, ACQ, T07, UNICAST, OTHER**: `radio` (the first 3 bytes) and `hex`.

## Example

Made-up values:

    {"t":1792892880.935,"ch":114,"ch_mhz":220.4375,"rate":"FR","type":"WIU_REPLY","kind":52,
     "len":17,"crc_ok":true,"corrected":0,"level_db":19.8,"wiu":"1234560ff0","group":"123456",
     "label":130,"seq":1872,"indicators":"b600","bits":"1011011000000000","codes":[13,3,0],
     "aspects":[null,"Clear",null],"spare":"0","hex":"1234560ff0820750b600","trailer":"a1b2c3d4"}
