# Contract v1.9: voice profiles from the headend, detector results from words

Builds on the headend's contract v1.8. Anything here that v1.8 already provides (such as the `words`
method) stays as v1.8 defines it.

Receivers read every defect detector announcement in full with a **voice profile** (how one style
of detector speaks) and decide the result from sound and words together. Profiles and which channel
uses which are **set on the headend** and sent in `GET /api/v1/config`; the receiver doesn't change
them. Until a headend sends profiles, receivers use the copies built into them (the repository's
`profiles/` folder), which can be removed once headends serve them.

Nothing here changes v1.5 or v1.6 behaviour for a headend that ignores it.

## 1. `GET /api/v1/config`: `voice_profiles`

A new top-level list of voice profiles. Send every profile any of this receiver's channels uses,
plus each railroad's default.

```json
"voice_profiles": [
  {
    "id": "csx-edd-v1",
    "name": "CSX equipment defect detector",
    "railroad": "CSX",
    "description": "CSX equipment defect detectors: the all-clear is said twice, then the total axle count, then \"End of transmission\".",
    "template": "CSX equipment defect detector, milepost {milepost}. No defects. No defects. Total axle: {axles}. End of transmission.",
    "variables": [
      {"name": "milepost", "type": "decimal", "decimals": 1, "min": 0, "max": 3000, "spoken": "digits"},
      {"name": "axles", "type": "integer", "min": 4, "max": 1000, "spoken": "digits"}
    ],
    "audio": {"filter": "voice"},
    "all_clear": {"phrase": "no defects", "times": 2, "min_wording": 0.6, "heard_as": []},
    "defect_words": ["defect detected", "hot box", "hot wheel", "dragging", "temperature", "high load", "wide load"],
    "trigger": {"min_s": 5, "max_s": 45, "max_other_channels": 1},
    "revision": 3
  }
]
```

| Field | Rules |
|---|---|
| `id` | `^[a-z0-9.-]{1,60}$`, unique. Never reuse an id for different wording; publish a new version (`-v2`) or bump `revision` for corrections. |
| `name` | 1 to 80 characters. No site names: a profile describes a style of voice. |
| `railroad` | reporting mark, or `""` for any railroad. The **first** profile in the list with a railroad's mark is that railroad's default. |
| `description` | free text, no site names or mileposts. |
| `template` | 1 to 300 characters: the announcement as spoken, with `{name}` where a value is spoken. Values that change per train are always placeholders. Also the speech prompt (receivers leave the values out). |
| `variables` | one per placeholder: `name`; `type` `decimal` or `integer`; `decimals` (decimals only); optional `min`, `max` (min <= max); `spoken` `digits` ("four seven five point eight") or `words` ("track one"). Values outside the limits are treated as misheard. |
| `audio.filter` | `voice` (voice band, noise measured in the call's pauses) or `none`. |
| `all_clear` | `phrase`; `times` >= 1 it is said; `min_wording` 0 to 1, the share of the template's words that must be heard; `heard_as`: how speech recognition mishears the phrase in this voice (receivers correct it in the transcript, never read it as a defect). Receivers also use a decimal value's `decimals` here: with a phrase starting "no", an extra trailing 0 ("oh") before the rest of the phrase is the "no", so "milepost 475.80 defects" becomes "milepost 475.8, no defects". |
| `defect_words` | words that mean a defect report when heard outside the all-clear phrase. |
| `trigger` | for detectors without a sound template on the receiver: call length `min_s` to `max_s` (0 < min_s < max_s <= 120) and `max_other_channels` that may open at the same moment before the call counts as interference. |
| `revision` | optional integer the headend bumps on each change; receivers ignore it but it helps tracing. Receivers ignore any other unknown field. |

A profile that breaks these rules is skipped by the receiver (and logged); validate on save.

## 2. `GET /api/v1/config`: `channels[].profile`

Each plan channel may name the profile for detector announcements on it:

```json
{"ch": 7, "freq": 161.1, "railroad": "CSX", "label": "Belton Sub Road", "kind": "voice", "proximity": true, "profile": "csx-edd-v1"}
```

- Optional, `voice` channels only. Absent or null: the receiver uses the default for the channel's
  railroad (section 1), then the generic profile.
- A profile id the receiver doesn't have (not in `voice_profiles`, not built in) also falls back to
  the default.

Any change to `voice_profiles` or a channel's `profile` changes `config_version` like other config.

## 3. `POST /api/v1/events`: `transcript.profile`

When the config carries `voice_profiles`, transcripts of detector announcements gain
`"profile": "<id>"`, the profile that read them. Store it with the transcript. Receivers never send
the field to a headend that doesn't send `voice_profiles`.

## 4. Detector results from words: `method: "words"`

- New `detector.method` value `words`, only with `result: "no_defects"`: the opening matched by
  sound and the transcript has the profile's all-clear (phrase said `times`, `min_wording` met, no
  defect words), although the sound match missed the all-clear ending.
- The headend lists the methods it accepts in `GET /api/v1/config`:
  `"detector_methods": ["sound", "rhythm_and_length", "opening", "words"]`.
  Receivers send `words` only when the list includes it; until then such all-clears go as
  transcripts only (`parsed.defects: false`, no `detector` object).
- `POST /events` must accept `words` (only with `no_defects`), or it refuses the batch.
- `different_message` results carry the words heard in `heard_text` (up to 300 characters) when
  the transcript names a defect.

Why: on 24 recordings of one Norfolk Southern detector, reading the whole announcement got every
milepost right (number-only reading got 22) and showed that two `different_message` reports with
`defects: true` were all-clears with noise after them.

## Superseded

The earlier draft's `nearby_detectors[].transcript.variable_types` is not needed: typed variables
live in the voice profiles.
