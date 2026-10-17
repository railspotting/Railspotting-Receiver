# Detector voice profiles

Each file describes how one style of defect detector speaks, so the receiver can read its
announcements: the wording (with placeholders such as `{milepost}`), how each value is spoken
and its limits, the audio filter, what counts as an all-clear, and which words mean a defect.

Voice profiles are set on the headend and sent to receivers in `/config` (`voice_profiles`, and
`channels[].profile` for which channel uses which; see `docs/contract-v1.9.md`). The
files here are built into the receiver only as a fallback until the headend serves profiles, and
are the seed for the headend's own list. They will be removed from this repository once headends
serve them. A profile describes a style of detector voice, not a place: keep site names and
mileposts out of it.

| Field | Meaning |
|---|---|
| `id` | unique, lowercase, ends in a version (`-v1`); never reuse an id for different wording |
| `name`, `description` | shown on the status page |
| `railroad` | reporting mark; the default profile for that railroad's channels (empty: any railroad) |
| `template` | the announcement as spoken, with `{name}` for each value. Also the speech recognition prompt, with the values left out |
| `variables` | each value: `type` (`decimal` or `integer`), `decimals`, `min`, `max`, and `spoken` (`digits`: read digit by digit; `words`: as words, "track one") |
| `audio.filter` | `voice` (voice band and noise from the pauses) or `none` |
| `all_clear` | `phrase`, how many `times` it is said, `min_wording`: the share of the template's words that must be heard, and `heard_as`: how speech recognition mishears the phrase in this voice (read as the phrase, never as a defect). When the phrase starts with "no", a value given more decimals than its `decimals` allows, ending in a 0 before the rest of the phrase, is read as the value then "no": "milepost 475.80 defects" is milepost 475.8, no defects |
| `defect_words` | words that mean a defect report when heard outside the all-clear phrase |
| `trigger` | for detectors without a sound template: call length (`min_s`, `max_s`) and `max_other_channels` that may open with it before it counts as interference |

Every profile needs a recording and its expected result in the tests before it is merged.
