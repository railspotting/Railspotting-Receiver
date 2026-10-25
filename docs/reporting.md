---
title: "How reports reach RailSpotting"
permalink: /reporting/
---

## Linking a receiver to your account

A new receiver makes its own signing key (Ed25519) on first start and shows a **link code**, for
example `WDJB-MJHT`, in its log and on its status page (with a QR code). Open
[railspotting.com/link](https://railspotting.com/link), sign in, enter the code, and choose to add
a new receiver or replace the hardware of an existing one; then set its location on the website.
Once approved, the receiver gets its device ID (`rs-` and 12 hex digits) and its channel plan.
Codes expire after 15 minutes and are replaced automatically.

## The API

The receiver talks to `https://api.railspotting.com/api/v1` over HTTPS. Every request is
**signed** with the receiver's key: the method, path, a timestamp, a random nonce and a SHA-256 hash
of the body, signed with Ed25519. RailSpotting refuses requests whose clock is more than five
minutes off, so the machine needs a working clock (NTP).

| Call | What for |
|---|---|
| `GET /config` | the channel plan, detector lists, voice profiles, dongle and scanner jobs chosen on the website |
| `POST /events` | calls, bursts and announcements, in batches of up to 200 |
| `POST /heartbeat` | every minute: "I'm here", the kit (dongles and scanner) and what it covers |
| `POST /reports` | radio checks against the reference station, scanner setup |
| `GET /commands` | tests the owner or an admin asks for from the website (radio check, reception test) |

## Events

Each channel opening is sent at once as an `open` event, and again as `closed` when it ends, with
its times, frequency, channel, kind and signal strength. A closed event can carry:

- `telemetry`: the decoded EOT packet;
- `transcript`: what speech recognition heard, with its confidence;
- `detector`: which detector, the result (no defects, defects, incomplete or a different message),
  how it was decided, and the milepost.

## When the network is down

Events wait in a local **outbox** (SQLite) and go when the network is back, oldest first, with
their own times. It holds up to 50,000 events; if it fills, the oldest closed events are dropped
first and the next heartbeat says how many. Failed uploads retry with growing delays (2 s up to 5
minutes), respecting RailSpotting's rate limits.

## What is sent about the receiver

- At **linking**: the public key, the machine's hostname, platform, receiver version, the scanner's
  model and firmware, and the network card's **MAC address** (for information).
- In **heartbeats**: the dongles and scanner by model and role. **Dongle and scanner serial
  numbers, USB ports and device paths are never sent**: each dongle goes up as an anonymous
  `dongle_id` (`sdr-` and 8 hex digits) and the scanner as a `scanner_id`, so the website can
  choose their jobs without learning their serials.
- The receiver's **location** is set by the owner on the website; the receiver itself sends none.
- **ITC** (220 MHz PTC) stays on the receiver unless the owner turns ITC upload on for it on the
  website. Then it sends wayside units' state changes and the base stations it hears, never the
  repeats, and RailSpotting never shows them publicly. See [ITC]({{ '/itc/' | relative_url }}).

The local status page (port 8080 on your network) shows more, including dongle serials, but it is
only reachable on your own network.
