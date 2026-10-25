---
title: "Railroad radio basics"
permalink: /radio-basics/
---

North American railroads run their operations over radio. A receiver near the tracks can hear
four kinds of transmissions, and each tells RailSpotting something about a train.

> This page is general background. The frequencies the receiver actually listens to come from
> RailSpotting's channel plan for its area (see [How the receiver identifies signals]({{ '/identifying-signals/' | relative_url }})).

## Voice channels (VHF, around 160 to 161.6 MHz)

The AAR (Association of American Railroads) channel plan sits in the VHF band between about
159.8 and 161.6 MHz, narrow-band FM. Each railroad and territory uses a few of them:

| Channel use | What you hear |
|---|---|
| **Road** | crews on the move: calling signals, talking to other trains and to maintenance crews. Also where **defect detectors** announce. |
| **Dispatcher** | crews and the dispatcher, often through a repeater, so it can be heard far from the train. |
| **Yard** | switching moves in a yard. |

A call on a **road** channel is a good sign a train is close: road channels are simplex and
low power, so you hear them over a few miles. RailSpotting's plans mark these channels as
**proximity** channels.

## End-of-train devices (EOT, 457.9375 MHz)

Freight trains no longer have a caboose. The last car carries an **end-of-train device** (EOT, or
ETD), a small box on the rear coupler with a red flashing marker light. It measures the **brake
pipe pressure** at the end of the train and sends it, with a few status flags, to the locomotive:

- on **457.9375 MHz**, as short data bursts (FFSK at 1200 bits per second);
- about **once a minute**, and whenever something changes;
- with its **unit number**, so the locomotive knows it is hearing its own train's device.

Because every freight train has one and it transmits all the time, the EOT is the most reliable way
to know a train is near. Each burst lasts a fraction of a second and the receiver decodes it. See
[EOT and HOT]({{ '/eot-hot/' | relative_url }}) for the signal and what each packet says.

## Head-of-train devices (HOT, 452.9375 MHz)

The locomotive answers on **452.9375 MHz**: its head-of-train device (HOT, or HTD) can ask the
EOT for its status and, in an emergency, command it to vent the brake pipe from the rear. HOT
transmissions are less frequent than EOT ones; hearing one means a lead locomotive is near.

## Defect detectors

Along the line, **wayside defect detectors** check passing trains for hot bearings, dragging
equipment, high or wide loads and the like. After the train passes, the detector **speaks** on the
road channel in a synthetic voice:

> "Detector, milepost 123.4. No defects. Total axles 312. Temperature 61 degrees."

When it finds something, it names the defect and its position instead. Each railroad's detectors
have their own wording and voice. An announcement tells RailSpotting **where** a train was (the
detector's milepost), **when**, and whether it was all clear.

## Positive train control (ITC, 220 MHz)

Many main lines also run **positive train control** (PTC), a safety system that can stop a train
before it passes a red signal or overspeeds. Its radio network, **ITC**, sits around **220 MHz**
and is digital: base stations, locomotives, and **wayside units** at signals and switches, each
reporting its state every few seconds. A receiver with a dongle for it can follow the signals
along the line as trains pass. See [ITC]({{ '/itc/' | relative_url }}).

## What RailSpotting makes of it

Put together, these let RailSpotting say "a train passed this crossing a few minutes ago, moving,
unit 76520 on its rear, all clear at the detector". The receiver's job is to hear all of it,
recognise which is which, and report it.
