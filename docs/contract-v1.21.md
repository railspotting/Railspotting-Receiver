# Shared API contract: v1.21 additions (ITC locomotives, train passes, wayside fields)

Status: **headend side built (decision 294); receiver side built.** This copy was written from the
proposal and the headend's notes before the headend published its own; replace it with the
headend repository's `docs/contract-v1.21.md` when that is pushed. Builds on v1.18 (`contract-v1.18.md`, ITC) and
v1.20. A receiver sends the new record types only when `/config` lists them (section 7), so a
receiver that knows v1.21 is safe against any headend, and a headend that knows v1.21 is safe
against older receivers.

| Endpoint | Change | Who |
|---|---|---|
| `POST /api/v1/itc` | base records: what `neighbour_channels` means (section 1, no schema change) | receiver sends |
| `POST /api/v1/itc` | **new** `loco` records: a locomotive radio heard (section 2) | receiver sends, headend stores |
| `POST /api/v1/itc` | **new** `pass` records: one train passing a receiver (section 3) | receiver sends, headend stores |
| `POST /api/v1/itc` | wayside records: optional `fields` (section 4) | receiver sends, headend stores |
| `POST /api/v1/itc` | place records: basis `closest_approach` (section 5) | receiver sends, headend stores |
| `POST /api/v1/heartbeat` | `itc` summary: `channels_outside`, `locos_heard` (section 6) | receiver sends, headend reads |
| `GET /api/v1/config` | `itc.records`, `itc.code_tables` (section 7) | headend sends, receiver reads |

## Why

- A base station's beacon lists the channels its waysides and locomotives use. Receivers now
  decode all of those inside their window; one receiver found a wayside it had never heard.
- Locomotives check in with the base by radio. Each check-in names the locomotive's radio and
  the base. Across receivers that places locomotives roughly (by base and by which receiver
  hears them loudest), and on one receiver it ties a train together: its locomotives, the
  signals it drops, its end-of-train unit and its detector call.
- A wayside's status bits hold signals, switches and hazard bits. Decoded per unit, the headend
  can show switch positions, dark signals and power alarms, not only "codes".
- The moment a train's locomotive radios are loudest at a receiver marks the signals nearest
  that receiver. That is a placement anchor from every train, not only detector-announced ones.

## 1. Base records: `neighbour_channels`

No schema change. The list is the other channels **that base's** waysides and locomotives may
use, not neighbouring bases' channels. The headend shows it as "channels its units use".

## 2. `loco` records

A locomotive radio heard by this receiver, sent once per sighting: when it has not been heard for
`itc.loco_gap_s` (section 7; 1800 s), or every hour while it is still heard.

```json
{"id": "…", "type": "loco", "radio": "1a2b3c", "base": "123456", "t": 1793012345.5,
 "first": 1793011800.0, "last": 1793012345.5, "peak_t": 1793012100.2, "peak_level_db": -42.0,
 "messages": 14, "channel": 114}
```

| Field | Type | Meaning |
|---|---|---|
| `id` | UUID | UUID v5 of `itc-loco:` + `radio` + `:` + `first` in whole milliseconds |
| `type` | `"loco"` | |
| `radio` | string `^[0-9a-f]{6}$` | the locomotive radio's ID, lowercase hex. The headend derives the decimal form itself |
| `base` | string `^[0-9a-f]{6}$`, or `null` | the base station the radio was talking through |
| `t` | number | when the record was made (for the outbox and `max_age_s`) |
| `first`, `last` | numbers | first and last message of the sighting |
| `peak_t`, `peak_level_db` | number, number -150 to 20 | the strongest message: when, and its peak relative to full scale (as `level_db` in v1.18) |
| `messages` | integer 1 to 100000 | messages from or to it in the sighting |
| `channel` | integer 1 to 999 | the channel of the strongest message |

What counts as a locomotive radio: a sender of check-in or acknowledgement messages, or the
destination of a base's unicast, that is not a base station and not listed in a base's wayside
radio list. The receiver decides; the headend stores what it is sent.

The headend:
- keeps one row per radio per receiver per sighting, and per radio a timeline across receivers;
- shows the decimal form and its last four digits labelled "possible road number". Whether the
  ID holds the road number differs by railroad and is not to be presented as fact;
- places a radio, for signed-in admins only, at the base it talks through and the receivers
  hearing it, strongest first.

## 3. `pass` records

One train passing a receiver, sent when the pass has ended (no drop and no locomotive message for
600 s).

```json
{"id": "…", "type": "pass", "t": 1793012700.0, "closest": 1793012100.2,
 "locos": [{"radio": "1a2b3c", "peak_t": 1793012100.2, "peak_level_db": -42.0}],
 "drops": [{"wiu": "1a2b3c0417", "t": 1793011900.4}, {"wiu": "1a2b3c0419", "t": 1793012105.1}],
 "eot_unit": 12345, "detector_event_id": "…"}
```

| Field | Meaning |
|---|---|
| `id` | UUID v5 of `itc-pass:` + `closest` in whole milliseconds |
| `closest` | when the train's locomotive radios were loudest at this receiver, or `null` |
| `locos` | its locomotive radios, strongest first (at most 8) |
| `drops` | the waysides it dropped to stop, in order (at most 64) |
| `eot_unit` | the end-of-train unit heard during the pass, or `null` |
| `detector_event_id` | the `event_id` of a detector event this receiver already sent for this train, or `null` |

The headend links the pass to its own train (decided train, EOT presence, detector report) by
time and receiver, and uses it for train identity (which locomotives), direction (the order of
`drops`) and placement (section 5).

## 4. Wayside records: `fields`

Optional. Present when the receiver knows the unit's layout:

```json
"fields": [{"kind": "signal", "code": 15}, {"kind": "signal", "code": 3},
           {"kind": "switch", "state": "normal"}, {"kind": "hazard", "on": false}],
"layout": "inferred"
```

- `kind`: `signal` (a 5-bit code 0 to 31), `switch` (`state`: `normal`, `reverse`, `moving` or
  `error`), `hazard` or `bit` (`on`: true or false).
- `layout`: `inferred` (worked out from the states seen) or `configured` (set by the owner).
- `codes` (v1.18) stays and is unchanged; `fields` is in addition.
- The headend shows switches and hazards on the unit's page, names signal codes with the
  railroad's table (section 7), and treats code 30 as dark and 31 as invalid.

## 5. Place records: basis `closest_approach`

`basis` may also be `closest_approach`: the unit dropped within 60 s of the moment a train's
locomotive radios were loudest at this receiver, on at least `itc.place_min_passes` passes
(section 7; 3). The position sent is the receiver's own position if the owner allows it, else a
mapped signal's position, as for `osm_signal`. Admins confirm it like any suggestion.

## 6. Heartbeat `itc` summary

Two new fields:

| Field | Meaning |
|---|---|
| `channels_outside` | channels heard bases list that this dongle's window can't reach (owners can move the centre) |
| `locos_heard` | distinct locomotive radios heard in the last 60 s |

## 7. `/config`

```json
"itc": {"upload": true, "records": ["wayside", "base", "place", "loco", "pass"],
        "loco_gap_s": 1800, "place_min_passes": 3,
        "code_tables": {"550": {"3": "Clear", "8": "Approach", "15": "Stop"}}}
```

- `records`: the record types this headend takes. A receiver sends only these. Absent: `wayside`,
  `base` and `place`, as in v1.18.
- `code_tables`: signal code names by railroad number (the 3 digits after the leading 7 in a
  wayside's decimal ID), kept by admins on the headend so every receiver names codes alike.
  Absent or empty: receivers name only the codes they already know.

## Who can see it

As v1.18 section 6: never public. Locomotive radio IDs and passes identify specific locomotives
and trains, so they are for the receiver's owner (their own receivers) and admins only, and
never on the public map, history, streams, `/embed`, `/api/public` or the CSV. A rendered live
dispatcher-style view is not to be made public either.

## Compatibility

| Receiver | Headend before v1.21 | Headend with v1.21 |
|---|---|---|
| before v1.21 | as before | as before; no `loco` or `pass` records, no `fields` |
| v1.21 | no `itc.records`, so it sends only wayside, base and place records; `fields` is ignored | sends what `itc.records` lists |


## Headend notes

- **Placements go on the map at once** (decision 293): a `place` record whose position is on a
  line the headend knows places that unit right away, whatever its basis. A later record from the
  same receiver moves it; an admin's placement is never moved. Positions off any known line stay
  suggestions for an admin. So a receiver sends only positions it is confident in, on the track (a
  mapped signal, or the receiver itself when its owner allows), sends again when it finds a better
  one, and after a restart only if it has changed.
- **Rejected one at a time, with a reason** (log it and drop it):
  - `radio`, and `base` in `loco` records: exactly 6 lowercase hex digits; send `null` for a base
    that isn't.
  - `loco`: `first` <= `peak_t` <= `last` <= `t`, each to within 1 s; `messages` 1 to 100000.
  - `pass`: `closest` not after `t`; at most 8 `locos` and 64 `drops`; `eot_unit` 0 to 131071 or
    `null`; `detector_event_id` the same UUID as the detector event's `event_id`, or `null`.
  - wayside `fields`: one malformed entry rejects the whole wayside record (and that state change
    is lost), so send `fields` only when every entry is valid, and `layout` only as `inferred` or
    `configured`.
- **Linking passes**: the headend keeps only a trip pseudonym for `eot_unit` (decision 279). It
  links a pass by `detector_event_id` first, then by the end-of-train trip, then by time (within
  10 minutes of `closest`), and takes the train's direction from the order of `drops`.
- **Code tables**: keyed by the railroad number (the 3 digits after the leading 7 of a unit's
  12-digit decimal ID). 30 is Dark and 31 Invalid. Re-read when `config_version` changes.
- **Privacy on the receiver**: locomotive radio IDs and passes identify locomotives and trains;
  keep them out of anything the receiver publishes itself (its local feed, a public dashboard,
  logs shared off the device).

## How the receiver does it

- `railspotting_itc::locos` tracks locomotive radios (check-in and acknowledgement senders and
  unicast destinations that are neither a base nor in a base's wayside list); a sighting is sent
  after `loco_gap_s` of quiet, or hourly while heard. Its peak is from its own transmissions.
- `railspotting_itc::passes` builds a pass around a train's locomotive radios (those starting
  within 3 minutes of each other), ended when they have been quiet 600 s; `closest` is the
  loudest one's peak. It gathers the drops, EOT unit and detector event within 10 minutes of the
  radios, lists the radios peaking within 8 dB of the loudest, and is kept only when a wayside
  dropped within 5 minutes of `closest`. (Built around drops instead, a busy line never ends a
  pass: waysides there drop every 2 minutes, with only 5% of gaps over 600 s.) Passes are also
  kept on the receiver (`state/itc/passes/`).
- `loco` and `pass` records are queued only while upload is on and `itc.records` lists them.
  `fields` is not sent yet (the per-unit layout is not worked out yet).
- Placement runs 15 minutes after a start and every 6 hours: positions from mapped signals once
  the last 3 runs agree, and `closest_approach` when the owner has set `itc_place.receiver_position`;
  each is sent when new or moved (`state/itc/place_sent.json`).
- Check-ins, acknowledgements, ACQ and unicasts are left out of the local feed.

## Headend work

1. Accept and validate `loco` and `pass` records (rejected one at a time like any record).
2. Store them with retention as v1.18 (45 days), never public.
3. Wayside `fields`: store, and show on the unit's page; code names from `code_tables`.
4. Place basis `closest_approach` in `placeProposal` and the automatic placement job.
5. `/config` `itc.records`, `loco_gap_s`, `place_min_passes`, `code_tables` (admin page to edit
   the tables).
6. Heartbeat `channels_outside` and `locos_heard` on the receiver's page.
7. Base station pages: call `neighbour_channels` "channels its units use".
