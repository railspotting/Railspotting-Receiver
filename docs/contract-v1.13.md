# Contract v1.13 (proposed): the dongle role order, and the ITC role

Builds on v1.11 (`sdr_roles`: the owner picks a job for each dongle on the website) and v1.12.
Proposed by the receiver; the headend does not send it yet. Nothing here changes earlier behaviour
for a headend that ignores it.

Receivers can now decode ITC, the 220 MHz radio system that carries positive train control (PTC)
messages, on a fourth kind of dongle. Owners decide which jobs their dongles do, and in what order
the jobs take dongles that the website has not named.

## 1. `GET /api/v1/config`: `sdr_role_order`

An ordered list of roles. Dongles the website has not named in `sdr_roles` fill the roles in this
order as they are plugged in; a role left out of the list takes no dongle (it is switched off).

```json
"sdr_role_order": ["eot", "itc", "vhf", "hot"]
```

| Role | Job |
|---|---|
| `eot` | UHF: end-of-train telemetry |
| `itc` | 220 MHz: ITC (PTC) messages, kept on the receiver (section 3) |
| `vhf` | VHF: voice, that is defect detector announcements and road calls (they share the channels) |
| `hot` | UHF: head-of-train telemetry |

- The default, when the headend sends nothing, is the order above (an owner decision of October
  2026).
- An empty list switches every role off for dongles the website has not named.
- Unknown roles and repeats are ignored; anything other than a list of strings is ignored.
- A dongle already doing a role keeps it when the order changes; a role switched off gives its
  dongle back, and the next role in the order takes it. A dongle named in `sdr_roles` keeps the
  job named there.
- The receiver writes the order into its `receiver.json` (`sdr.role_order`), so it holds when the
  headend is out of reach. An owner can also set it there without the headend.

## 2. `itc` in `sdr_roles` and in capabilities

- `sdr_roles` (v1.11) may name `itc` as a dongle's job.
- In the heartbeat's `capabilities.receivers`, a dongle doing ITC is reported with
  `"role": "itc"`, like any other SDR. Headends that do not know the role should list it and
  otherwise ignore it.

## 3. ITC messages are not uploaded

(Contract v1.18, `contract-v1.18.md`, adds the upload of wayside state changes, when the owner
turns it on. The messages themselves still stay on the receiver.)

What a receiver decodes from ITC describes railroad operations (base station positions, wayside
signal and switch states). It stays on the receiver: a daily log under the state directory and a
feed on the receiver's own machine. Nothing in this version sends it to the headend, and no
endpoint is added for it.
