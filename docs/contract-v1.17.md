# Contract v1.17 (proposed): the dongle role order, and ITC and HOT dongles

Builds on v1.11 (`sdr_roles`: the owner picks a job for each dongle on the website) and v1.16. The
receiver first proposed this as v1.13; the headend numbered it v1.17. Proposed on both sides, not
final. A receiver or headend that ignores the new fields works as before.

| Endpoint | Change | Who |
|---|---|---|
| `GET /api/v1/config` | `sdr_roles` may name `itc` and `hot` | headend sends, receiver reads |
| `GET /api/v1/config` | `sdr_role_order`, only when the owner has set one | headend sends, receiver reads |
| `POST /api/v1/heartbeat` | `role: "itc"`; `capabilities.sdr_role_order`; `features.itc_decode` | receiver sends, headend reads |

## 1. `sdr_roles`: two more jobs

The values are `eot`, `itc`, `vhf`, `hot` or `off`. The rest of v1.11 holds. A value the receiver
does not know leaves that dongle not named.

## 2. `sdr_role_order` in `GET /api/v1/config`

```json
"sdr_role_order": ["eot", "itc", "vhf", "hot"]
```

The order in which dongles not named in `sdr_roles` fill jobs. A job left out gets no dongle; an
empty list switches every job off for dongles not named.

- **Sent only when the owner has set an order.** When it is absent the receiver keeps the order
  it has: receiver.json's `sdr.role_order`, or the default `eot, itc, vhf, hot` (an owner
  decision of October 2026).
- Unknown jobs, and repeats after the first, are ignored. Anything other than a list of strings
  is ignored.
- The receiver applies a new order at once, writes it into receiver.json so it holds when the
  headend is out of reach, and sends a heartbeat at once.
- A dongle doing a job keeps it while that job is still in the order. A job switched off gives its
  dongle back, and the next job in the order without a dongle takes it.
- A dongle named in `sdr_roles` keeps its job whatever the order.
- Receivers with dongles fixed by serial in receiver.json (`sdr.devices`) ignore the order, as
  they ignore `sdr_roles`.

## 3. In the heartbeat

**The order in force**, in `capabilities`:

```json
"sdr_role_order": {"order": ["eot", "itc", "vhf"], "from": "website"}
```

| `from` | When |
|---|---|
| `website` | the order is the last `sdr_role_order` from /config (kept in `state/sdr_role_order.json`) |
| `local` | receiver.json sets another order (by hand, or in the Mac app) |
| `default` | neither |

Left out by receivers with dongles fixed by serial.

**An ITC dongle** appears in `capabilities.receivers` with `"role": "itc"` and `"audio": false`.
It leaves out `channels`, `uncovered_channels` and `reference_in_window`: those hold
channel-plan channel numbers, and ITC channels are numbered apart. A HOT dongle appears with
`"role": "hot"`.

**`features.itc_decode`** is `true` while a dongle doing ITC is connected, and absent otherwise.

## 4. ITC messages are not uploaded

What a receiver decodes from ITC describes railroad operations. It stays on the receiver: a daily
log under the state directory and a feed on the receiver's own machine. Uploading ITC state
changes is proposed separately (v1.18) and is not built.
