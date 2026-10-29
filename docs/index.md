---
title: "RailSpotting receiver"
permalink: /
---

<div class="hero" markdown="1">

# RailSpotting receiver

The RailSpotting receiver listens to railroad radio and reports what it hears to
[RailSpotting](https://railspotting.com), where trains appear on the live map and in alerts. It
runs on small Linux machines (and as a macOS menu bar app) with one to four **RTL-SDR dongles**
and, optionally, a **Uniden SR30C** scanner.

It recognises:

- **Voice calls** on the railroad's road, dispatcher and yard channels.
- **End-of-train (EOT) devices**, decoded: unit number, brake pipe pressure, motion and more.
- **Head-of-train (HOT) transmissions**, detected and timed.
- **Defect detector announcements**: which detector, and whether it reported "no defects".
- **ITC (220 MHz PTC)**, decoded: base stations, and the state of the signals along the line as
  trains pass. It stays on the receiver unless the owner turns upload on.

</div>

<figure class="shot">
  <a href="{{ '/status-page/' | relative_url }}"><img src="{{ '/assets/screenshots/console-detectors.png' | relative_url }}" alt="The receiver's status page, Defect detectors tab: announcements per hour and recent announcements, with all clear, defect and unconfirmed results" loading="lazy"></a>
  <figcaption>The receiver's status page, with made-up data. <a href="{{ '/status-page/' | relative_url }}">A tour of the status page</a>.</figcaption>
</figure>

<div class="cards">
<a class="card" href="{{ '/status-page/' | relative_url }}"><b>The status page</b><span>What the receiver shows on your network: dongles, signal, detector results, EOT units and ITC signals.</span></a>
<a class="card" href="{{ '/radio-basics/' | relative_url }}"><b>Railroad radio basics</b><span>How trains use radio: road and dispatcher channels, EOT, HOT and defect detectors.</span></a>
<a class="card" href="{{ '/identifying-signals/' | relative_url }}"><b>How the receiver identifies signals</b><span>Watching many channels at once, decoding EOT, recognising detectors.</span></a>
<a class="card" href="{{ '/reporting/' | relative_url }}"><b>How reports reach RailSpotting</b><span>Linking, the signed API, the offline outbox, and what is sent.</span></a>
<a class="card" href="{{ '/eot-hot/' | relative_url }}"><b>EOT and HOT (457 and 452 MHz)</b><span>The end-of-train and head-of-train devices: the signal, the EOT packet and its fields, and how they are decoded.</span></a>
<a class="card" href="{{ '/itc/' | relative_url }}"><b>ITC (220 MHz PTC)</b><span>The train control radio network: channels, the protocol, wayside signal states, and what is shared.</span></a>
<a class="card" href="{{ '/capabilities/' | relative_url }}"><b>SDR and receiver capabilities</b><span>Dongle jobs, windows, radio checks, and the SR30C scanner.</span></a>
<a class="card" href="{{ '/installing/' | relative_url }}"><b>Installing</b><span>Debian or Ubuntu, a Proxmox container, or the macOS app.</span></a>
<a class="card" href="{{ '/antennas/' | relative_url }}"><b>Antennas and RF</b><span>Antennas, diplexers, splitters, coax, preamps and FM filters.</span></a>
<a class="card" href="{{ '/hardware/' | relative_url }}"><b>Tested hardware</b><span>Dongles, scanners and computers in use today.</span></a>
</div>


## Good to know

- A receiver reports to RailSpotting, a service that is **not open source**. You link a receiver
  to your RailSpotting account with a code it shows; see [Installing]({{ '/installing/' | relative_url }}).
- Everything on the radio side is in this repository and can be tried without hardware:
  `railspotting-receiver run --simulate` feeds it synthetic radio traffic.
