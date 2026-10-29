---
title: "The status page"
permalink: /status-page/
---

Every receiver serves a status page on your own network, at `http://<machine>:8080/` (the Mac app
shows the same page in its window). It is read only and updates every two seconds. It shows what
the dongles are doing, what the receiver has heard, and how each detector announcement was read.
Tabs along the top give each kind of signal its own page: **EOT**, **Defect detectors**, **ITC**,
**HOT** and **Voice**.

> **The screenshots use made-up data.** They come from a receiver running in simulation
> (`run --simulate`) against a test headend. The receiver, its channel plan, the detectors, the
> trains and the ITC signals are all invented, so they don't match any real place.

## Status

<figure class="shot">
  <img src="{{ '/assets/screenshots/console-status.png' | relative_url }}" alt="The Status tab: account link, reference station, headend and scanner cards, three dongle panels, and charts of the reference signal and noise floor over 24 hours" loading="lazy">
  <figcaption>The Status tab: the link, the headend, each dongle and the signal over the last day.</figcaption>
</figure>

- **Account link:** whether the receiver is linked to a RailSpotting account. Before it is, this
  card shows the link code and a QR code to scan.
- **Reference station:** how strongly the receiver hears its NOAA weather station. A steady signal
  means the antenna and the VHF dongle are fine.
- **Headend:** the channel plan version, the last heartbeat, events waiting in the outbox, and the
  clock's offset from RailSpotting's.
- **Scanner:** the Uniden SR30C, if one is connected, and the job it was given.
- **Dongles:** one panel per dongle, with its job, the antenna to connect, the frequencies it
  covers, and health counters. Overruns, short reads and stalls should stay at 0, and clipping
  under 0.1%.
- **Signal:** the reference station's signal and each dongle's noise floor, over 1 hour to 7 days.
  A sudden change in the noise floor usually means new interference near the antenna.

<figure class="shot">
  <img src="{{ '/assets/screenshots/console-activity.png' | relative_url }}" alt="The Activity card: the last EOT packet with unit number, brake pipe pressure and motion, and a table of recent events" loading="lazy">
  <figcaption>Activity: the last EOT packet, the last transcript, and every event heard, newest first.</figcaption>
</figure>

## Defect detectors

The receiver reads every detector announcement it hears, by sound and by words, and gives each one
of three results:

| Result | Means |
|---|---|
| **All clear** | the detector said "no defects": its sound matched the detector's template, or the transcript has the all-clear phrase as often as the detector says it |
| **Defect** | defect words were heard outside the all-clear phrase, such as "hot box, axle 112, south rail" |
| **Unconfirmed** | neither sound nor words settled it, for example when the radio was too weak to hear the end |

The receiver only reports what it heard. RailSpotting decides what to show on the live map and
which alerts to send.

<figure class="shot">
  <img src="{{ '/assets/screenshots/console-detectors.png' | relative_url }}" alt="The Defect detectors tab: totals for all clear, defects and unconfirmed, announcements per hour, detectors heard, and recent announcements including a defect report" loading="lazy">
  <figcaption>The Defect detectors tab. The two red rows are one detector reporting a hot box. This detector sends every message twice, about 20 seconds apart.</figcaption>
</figure>

- **Totals:** announcements in the chosen range, and the share of each result.
- **Announcements per hour:** green for all clear, red for defects, amber for unconfirmed.
- **Detectors heard:** each detector, how often it was all clear, how its last announcement was read
  (**sound** or **words**), and its last words. A detector that is often unconfirmed may need a
  sound template, or its voice profile's wording checked.
- **Recent announcements:** each announcement with its result and what speech recognition heard.

Lower on the tab, **Detector profiles** shows which voice profile reads each channel, and **Test
transcription** reads a saved recording with any profile. See
[How the receiver identifies signals]({{ '/identifying-signals/' | relative_url }}).

## EOT

<figure class="shot">
  <img src="{{ '/assets/screenshots/console-eot.png' | relative_url }}" alt="The EOT tab: bursts heard and decoded, bursts per hour, decoding by signal level, corrected bits, and a table of EOT units heard with brake pipe pressure and motion" loading="lazy">
  <figcaption>The EOT tab: every end-of-train burst, how many decoded, and each train's EOT unit.</figcaption>
</figure>

- **Bursts per hour:** decoded in blue, not decoded in orange. A train passing shows as a cluster.
- **Decoding by signal level:** where decoding starts to fail. If weak bursts rarely decode, the
  antenna or its height is the place to improve.
- **Units heard:** each EOT unit, so each train, with its brake pipe pressure, motion and marker
  light.

The **HOT** tab works the same way for head-of-train bursts, which are detected and timed but not
decoded. See [EOT and HOT]({{ '/eot-hot/' | relative_url }}).

## ITC

<figure class="shot">
  <img src="{{ '/assets/screenshots/console-itc.png' | relative_url }}" alt="The ITC tab: a rail layout with four signal locations in line order, and a chart of each signal's aspect over 24 hours, mostly clear with short stop and approach periods as trains pass" loading="lazy">
  <figcaption>The ITC tab: signal locations in the order trains pass them, and each signal's aspect over the day.</figcaption>
</figure>

With a 220 MHz dongle, the receiver decodes ITC and shows the state of the wayside signals around
it. Each signal drops to **stop** (red) as a train passes, then steps through **approach** (amber)
back to **clear** (green) behind it.

- **Rail layout:** the signal locations, in the order trains pass them, worked out from the order
  they change. Twin units are the two directions at one location.
- **Aspects over time:** each signal's state over the chosen range. A diagonal of red marks is one
  train moving along the line.

ITC stays on the receiver unless the owner turns upload on. See [ITC]({{ '/itc/' | relative_url }}).

## On a phone

<figure class="shot shot-phone">
  <img src="{{ '/assets/screenshots/console-phone.png' | relative_url }}" alt="The status page on a phone, with the cards stacked in one column" loading="lazy">
  <figcaption>The same page on a phone: the cards stack in one column.</figcaption>
</figure>

The page works on any device on the same network as the receiver. It shows dongle serial numbers
and ITC signal states, so don't open port 8080 to the internet.
