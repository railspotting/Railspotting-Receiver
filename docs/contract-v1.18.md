# Contract v1.18 (proposed): ITC wayside state changes

Status: **proposed by the headend; built on the receiver, not yet on the headend.** Builds on
v1.17 (`contract-v1.17.md`). A receiver uploads nothing under this contract until its `/config` says `itc.upload: true`
(section 4), so a receiver that knows v1.18 is safe against any headend.

| Endpoint | Change | Who |
|---|---|---|
| `POST /api/v1/itc` | **new**: wayside state changes and base stations, in batches (sections 1 to 3) | receiver sends, headend stores |
| `GET /api/v1/config` | `itc` (section 4), fixed receivers only | headend sends, receiver reads |
| `POST /api/v1/heartbeat` | `itc` summary (section 5) | receiver sends, headend reads |
| `GET /api/v1/config` | `itc_units`: where known wayside units are (section 7, later) | headend sends, receiver reads |

## Why

ITC carries about 35,000 messages an hour on one receiver, of which about 43 are wayside state
changes. The receiver keeps the changes and drops the repeats, so the headend stores what changed
and when, not the traffic. Signal states reveal train movements, so this data is never public
(section 6).

## 1. `POST /api/v1/itc`

A new endpoint rather than a new `kind` on `/events`:

- Events are radio transmissions: a start, an end, a frequency and RSSI. ITC records are a
  different kind of thing.
- Events feed train alerts, the risk profiler, commissioning and coverage. ITC records feed none
  of them.
- ITC gets its own quota, switch and retention, and a headend without v1.18 answers 404 instead
  of rejecting a whole events batch.

Signed exactly as every v1 request. The body is up to 256 KB and holds up to 200 records:

```json
{"records": [ {"id": "…", "type": "wayside", …}, {"id": "…", "type": "base", …} ]}
```

The response is `200`:

```json
{"accepted": ["…"], "duplicates": ["…"], "rejected": [{"id": "…", "reason": "codes must be 1 to 64 integers 0-31"}]}
```

- `accepted`: stored.
- `duplicates`: this receiver already sent that `id`. This is a success: the receiver drops the
  record from its outbox.
- `rejected`: one record broke the schema. The receiver drops it, logs it, and does not resend
  it. One bad record never holds up the rest of the batch. This differs from `/events`, where
  one bad event fails the batch.

The whole request fails only for a bad signature, a body that is not JSON of this shape, more
than 200 records (`413`), or uploads switched off. Uploads switched off returns `403 itc_off`:
the receiver stops uploading, keeps the data local, and fetches `/config` again.

**Rate.** ITC has its own per-receiver bucket, separate from events. It refills at 20 records a
minute and bursts to 600, so a start-up that sends a first state for every unit gets through at
once. Over the limit the response is `429` with `Retry-After`, and records stay in the outbox.
The receiver sends at most one ITC request every `itc.flush_interval_s` (section 4), which also
keeps it inside the device's request quota.

**Age.** A record older than `itc.max_age_s` (section 4; 7 days) is rejected with reason
`too_old`. A record dated more than 5 minutes in the future is rejected with `clock`. The
receiver drops older records from its outbox instead of sending them.

## 2. Wayside records

```json
{"id": "9b2f0c1e-5d7a-5e3b-8c41-2f6a9d0e7b13", "type": "wayside", "wiu": "1a2b3c0417",
 "t": 1793012345.512, "first": false,
 "codes": [3, 15], "previous_codes": [8, 15],
 "indicators": "18f0", "source": "status", "channel": 438, "level_db": -61.5}
```

| Field | Type | Rule |
|---|---|---|
| `id` | UUID | the record's identity (below) |
| `type` | `"wayside"` | |
| `wiu` | string `^[0-9a-f]{10}$` | the wayside unit ID, lowercase. The headend derives the 6-digit group itself, so `group` is not sent |
| `t` | number, Unix seconds with milliseconds | when the receiver first heard the new state: the first message that showed it |
| `first` | bool | `true`: a starting state (after the receiver starts, or after a gap in reception of this unit of 60 s or more). `false`: a change from `previous_codes` |
| `codes` | list of 1 to 64 integers, 0 to 31 | the device codes, one per field, in field order |
| `previous_codes` | the same, or `null` | the last state this receiver sent for this unit. `null` exactly when `first` is true |
| `indicators` | string, hex, up to 64 characters, or absent | the raw indicator bits, kept so codes can be read again once railroad device tables are known |
| `source` | `status` or `reply` | `status`: from the unit's own message (0x33). `reply`: from a relayed reply (0x34), used only when no status message is recent |
| `channel` | integer 1 to 999 | the ITC channel it was heard on |
| `level_db` | number, -150 to 20 | the signal level of the message that showed the state |

`changed_fields` is not sent, because the headend compares `codes` with `previous_codes` itself.

**What to send.**
- Every change, however short. A signal that clears for under a second and goes back is two
  records. The headend stores both and decides what to show.
- A starting state for each unit when it is first heard after a start or a gap.
- No repeats: a state the same as the last one sent for that unit is not sent.

**Identity.** `id` is a UUID v5 under the receiver's event namespace, of the string
`itc-wayside:` + `wiu` + `:` + `t` in whole milliseconds + `:` + `codes` joined with commas (for
example `itc-wayside:1a2b3c0417:1793012345512:3,15`). A retry or outbox replay of the same record
has the same `id`, and the headend stores each receiver's `id` once.

## 3. Base station records

```json
{"id": "4c7e…", "type": "base", "base": "c4a1", "t": 1793012300.0,
 "lat": 40.5, "lon": -90.25, "local_channel": 438, "neighbour_channels": [412, 466],
 "channel": 102, "level_db": -70.2}
```

| Field | Type | Rule |
|---|---|---|
| `base` | string, hex, 1 to 16 characters | the base radio ID |
| `t` | Unix seconds | when this version was first heard |
| `lat`, `lon` | numbers, or `null` | the position the beacon gives. Base stations are fixed railroad infrastructure |
| `local_channel` | integer 1 to 999 | |
| `neighbour_channels` | list of up to 32 integers | |
| `channel`, `level_db` | as for wayside records | where and how strongly the beacon was heard |

Sent when a base is first heard after a start, and when its position, local channel or neighbour
list changes. Utilisation is not sent here: it goes in the heartbeat. `id` is a UUID v5 of
`itc-base:` + `base` + `:` + `t` in whole milliseconds.

## 4. `itc` (`GET /api/v1/config`)

```json
"itc": {"upload": true, "max_batch": 200, "flush_interval_s": 10, "max_age_s": 604800}
```

Fixed receivers only. It is present whenever the headend knows v1.18, so absence means a headend
without it: keep ITC local.

- **`upload`** is `false` until the receiver's owner turns ITC upload on, on the receiver's page.
  The website offers the switch only when the heartbeat reports `features.itc_decode` (v1.17).
- Only the owner can turn it on. An admin can turn it off for one receiver, or for every receiver
  in the settings, without the owner.
- With `upload: false` the receiver keeps ITC local (its daily log and its own feed) and sends
  nothing to `/itc`.
- When `upload` goes from false to true, the receiver starts with a starting state for every unit
  it can hear. It does not replay changes from before the switch, which stay local.
- Any change changes `config_version`.

## 5. The heartbeat's `itc` summary

```json
"itc": {
  "channels": [102, 438],
  "bases": [{"base": "c4a1", "channel": 438, "utilisation_pct": 37, "level_db": -61.2, "last_s": 4}],
  "units_heard": 38,
  "messages": {"WIU": 520, "WIU_REPLY": 31, "BEACON": 6, "TIME": 6, "CNTL": 60, "other": 14},
  "crc_fail": 9,
  "outbox": 0
}
```

| Field | Rule |
|---|---|
| `channels` | the ITC channels being watched now |
| `bases` | each base heard in the last 5 minutes. `channel` is its local channel; `last_s` is the seconds since it was last heard |
| `units_heard` | distinct wayside units heard in the last 60 s |
| `messages` | decoded messages by type in the last 60 s. T07, QSTAT, ACK, ACQ and UNICAST are summed in `other` |
| `crc_fail` | frames that failed CRC in the last 60 s, so a weak antenna shows up |
| `outbox` | ITC records waiting to upload |

Sent while a dongle is doing ITC, whether or not upload is on, so the owner and admins can see
that ITC is alive. It is absent when no dongle is doing ITC. A headend without v1.18 drops it.
The headend keeps it with the heartbeat (24 hours at full rate, then hourly for 30 days).

## 6. What the headend does with it

**Merging receivers.** Every record is kept per receiver: who heard what, when and how strongly.
From these the headend builds one timeline per unit:

- A change is identified by unit, `codes` and time. Changes from different receivers to the same
  `codes` for the same unit within 5 s of each other are one change, at the earliest `t`, heard by
  each of those receivers.
- A starting state adds nothing when it matches the unit's state at that time. When it differs,
  it is stored as an observed state with no known previous state.
- Records are placed by `t`, not by arrival, so an outbox replay slots into the right place.
- When receivers disagree (one missed a short change), both are kept. The timeline marks which
  receivers heard each change.
- A receiver whose heartbeat says its clock is not synced has its records stored but left out of
  merging.

**Who can see it.** Never public: not on the map, history, trains, the YouTube stream, `/embed`,
`/api/public` or the CSV.
- A receiver's owner sees what their own receivers reported.
- Admins see the merged timelines and every receiver's records.
- Regional admins see units heard by receivers in their regions.

Anything wider (for example a public "signal at Clear" layer) would need its own decision, with
the owners' agreement.

**Retention.** 45 days, the same as trains, as a setting. Base station records are kept until 45
days after a base was last heard.

## 7. Wayside unit positions (later; receivers may ignore)

The headend will hold where each unit is, so every receiver and the website share one answer:

```json
"itc_units": [{"wiu": "1a2b3c0417", "pair": "1a2b3c0416", "railroad": "NS", "subdivision": "Example",
  "milepost": 488.4, "lat": 40.51, "lon": -90.24, "signal": "OSM node 123456789", "confirmed": true}]
```

- `/config` lists the units this receiver has heard, and units within its reach.
- A receiver's monitor may propose a placement with a `type: "place"` record on `/itc` (`wiu`,
  plus `milepost` and `subdivision`, or `lat` and `lon`, plus `basis`: `osm_signal` or
  `detector_pass`).
- An admin confirms a placement on the website. Until then it is used but marked unconfirmed.

This part is built after the change stream works. Its exact fields will be fixed then, as v1.19
if they change. The receiver does not build it yet.

## Compatibility

| Receiver | Headend before v1.18 | Headend with v1.18 |
|---|---|---|
| before v1.18 | as before | as before: no ITC, and the website shows no ITC switch |
| v1.18 | no `itc` in `/config`, so nothing is uploaded and ITC stays local. A heartbeat `itc` block is dropped | uploads once the owner turns it on |

## How the receiver does it

- `railspotting_itc::changes::Tracker` turns decoded messages into records. A relayed reply
  counts only when the unit's own status message has not been heard for 20 s. Codes, channels and
  IDs outside the rules above make no record.
- `level_db` is the message's peak relative to full scale (the channel's noise floor plus the
  burst's peak above it), clamped to -150 to 20.
- `utilisation_pct` is the beacon's utilisation byte, read as a percentage; a value above 100 is
  sent as `null`.
- The tracker exists only while upload is on, so turning upload on gives a starting state for
  every unit, and nothing from before the switch is queued.
- Records wait in their own outbox (`state/itc_outbox.db`, at most 20,000, oldest first out),
  apart from events. They go in their own thread, one request every
  `flush_interval_s`, with their own backoff, so ITC never holds up events.
- When upload turns off (in `/config`, or `403 itc_off` or `404` from `/itc`), the queued records
  are dropped. A receiver that starts with upload off drops them too.
- A `400` for a whole batch drops that batch, since sending it again would fail the same way.
- The status page shows, on the ITC dongle's panel, whether state changes go to the headend.
