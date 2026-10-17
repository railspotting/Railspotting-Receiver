# RailSpotting brand guide

How RailSpotting looks, sounds and names things. Everything here describes what the site already
does; the source of truth for colors is `web/static/css/base.css`, and for the mark the files in
`branding/logos/` (see [logo-kit.md](logo-kit.md) for every file and where it goes). When this
guide and the code disagree, fix one of them.

## 1. What RailSpotting is

**One line:** Live and historical train activity at rail crossings, estimated from railroad radio
traffic.

**A little longer:** Volunteers run small radio receivers near the tracks. They listen to crew
radio, end-of-train devices and wayside defect detectors, and RailSpotting turns what they hear
into a map of where trains probably are, which crossings may be blocked, and what passed when.

Three ideas shape everything else:

- **Estimate, never promise.** RailSpotting cannot say a crossing is clear. Every surface that
  could be read as safety information says so (see "Not for safety" in the terms).
- **Community-run.** Receivers belong to people. Credit them, protect their privacy, and earn
  their trust before posting what their receivers report.
- **Plain and local.** It is a tool for people who live near the line and people who love
  trains, not a dashboard for railroads.

## 2. Name

| Use | Not |
|---|---|
| RailSpotting | Railspotting, Rail Spotting, RAILSPOTTING (in running text), Rail-Spotting, RS |
| railspotting.com | RailSpotting.com in a link or address |

- One word, capital R and capital S, in all running text, titles and page names.
- The wordmark is set in capitals (`RAILSPOTTING`) as a typographic treatment only, from the
  outlined logo files.
- Do not shorten it to "RS" for people. `rs-` appears in device IDs and `X-RS-` in API headers;
  those are technical identifiers, not the name.
- Do not use the name for anything that is not ours: forks of the receiver software and other
  sites may not call themselves RailSpotting or use the mark.

## 3. The mark: waveform track

Track seen from above, with ties that rise and fall like a radio signal. One shape says what
RailSpotting does: the line, heard by radio. The rails run off both edges of the tile, so the track
carries on past the frame.

| Part | Color |
|---|---|
| Tile | `#16191c`, corner radius 7 of 32 |
| Ties | `#e0a100` signal amber, five round-ended ties of different heights, stroke 2.8 of 32 |
| Rails | `#f1efe9`, two lines running edge to edge, stroke 1.5 of 32 |
| Dark edge | `#2f353b` hairline, only where the tile sits on a near-black background |

**Files.** All in `branding/logos/`; [logo-kit.md](logo-kit.md) says which to use where.

| File | Use |
|---|---|
| `favicon.svg` | The mark: browser tab icon and the master for 24 px and up |
| `favicon-16.svg` | Below 24 px: four heavier ties and thicker rails |
| `mark-dark.svg` | The mark with its dark edge, for the site header on phones |
| `icon-square.svg`, `icon-maskable.svg` | App icon sources (iOS, Android) |
| `lockup-light.svg`, `lockup-dark.svg`, `lockup-auto.svg` | Mark and wordmark side by side: the default logo |
| `stacked-*.svg` | Mark above wordmark, for square spaces |
| `wordmark-*.svg` | The name alone where the mark is already nearby |
| `banner-1200x630.svg` | Link previews |
| `png/` | Rendered PNG and ICO files for the web app manifest, Home Screens and link previews |

**In the header.** The dark lockup (`lockup-dark.svg`), so the mark is 30 px tall; on phones, where
the wordmark is hidden, the mark alone (`mark-dark.svg`) at 28 px. The admin portal adds the word
"admin" after the lockup in signal amber.

**Use it**

- At 16 px or larger, with the small-size file below 24 px.
- On its own tile. The tile is part of the mark.
- With clear space of at least a quarter of its width on every side.

**Do not**

- Recolor the ties or rails, outline the mark, or put it on a busy photo.
- Rotate, flip or stretch it, or add a train.
- Change the number or heights of the ties, or animate them.
- Reset the wordmark in another typeface. Use the outlined files (the wordmark is outlined from Inter
  Bold, SIL Open Font License).

The `-auto` files follow the device's light or dark setting, not the theme picked in the site's
profile menu.

## 4. Color

Colors are CSS tokens on `:root`, each with a dark theme value. Use the token, never the hex
value, in code. The site follows the device's light or dark setting, and the profile menu lets
people choose.

### 4.1 Brand and interface

| Token | Light | Dark | Use |
|---|---|---|---|
| header | `#16191c` | `#16191c` | Top bar, in both themes; also the mark's tile, the phone status bar and the app background |
| `--focus` | `#e0a100` | `#e0a100` | Signal amber: the 4 px stripe under the header, the current nav item, focus rings |
| `--accent` | `#0d5c63` | `#4fb3bb` | Deep teal: primary buttons, links, the account avatar |
| `--accent-ink` | `#ffffff` | `#0b1416` | Text on the accent |
| `--bg` | `#f4f2ee` | `#121416` | Page background, a warm paper tone |
| `--surface` | `#ffffff` | `#1b1e21` | Cards |
| `--surface-2` | `#ebe8e2` | `#24282c` | Code blocks, hover states, secondary fills |
| `--ink` | `#1d1f22` | `#e9e7e3` | Body text |
| `--ink-2` | `#4d525a` | `#b4b2ad` | Secondary text |
| `--ink-3` | `#737982` | `#8c8a86` | Muted text, table headers |
| `--line` | `#d8d4cc` | `#33383d` | Borders and rules |

### 4.2 What the map means

These colors carry meaning. Use each only for its meaning, everywhere: map, track chart, history,
legends and notifications.

| Token | Light | Dark | Means |
|---|---|---|---|
| `--talk` | `#b8389e` | `#e37fd0` | A train placed by crew radio (magenta) |
| `--tel` | `#f0a000` | `#ffc233` | A train placed by an end-of-train or head-of-train burst (amber) |
| `--det` | `#1f6feb` | `#6ea8ff` | A wayside defect detector (blue diamond) |
| `--traffic` | `#12988c` | `#3fd1c2` | Weak radio traffic, no train placed (teal ring) |
| `--quiet` | `#8a9099` | `#8a9099` | Monitored, nothing heard |
| `--unmonitored` | `#b4b8be` | `#5d636a` | No receiver covers it |
| `--offline` | `#8a9099` | `#8a9099` | Its receivers are offline |

### 4.3 Status

| Token | Light | Dark | Use |
|---|---|---|---|
| `--ok` | `#2f8f4e` | `#4cc274` | Passed, online, approved |
| `--warn` | `#a86a00` | `#ffb627` | Needs attention, provisional, waiting |
| `--warn-bg` | `#fff4d6` | `#3a3014` | Background of a warning callout |
| `--danger` | `#b3261e` | `#ff6b61` | Failed, quarantined, destructive buttons, unread count |

Status is never shown by color alone: badges carry a word ("provisional", "quarantined"), and map
markers have labels and a legend.

## 5. How time and state are shown

Crossing status always uses these words, from `STATUS_LABEL` in `web/static/js/common.js` (the
minute values follow the settings):

| Status | Label | Look |
|---|---|---|
| nearby | Train within 5 min | Fast blink (0.5 s) in the train's color |
| recent | Train 5 to 30 min ago | Solid in the train's color |
| activity | Weak radio traffic | Teal ring |
| quiet | Quiet | Gray |
| offline | No receiver online | Gray, coverage washed out |
| unmonitored | Not monitored | Light gray |

Detectors use one color only: faded when quiet, solid when one reported a train in the last 30
minutes, flashing within 5 minutes.

**Motion.** Blinking is the only animation, and only for "now". People who ask their device for
reduced motion get a steady ring instead of a blink. Never add motion for decoration.

## 6. Type

| Role | Setting |
|---|---|
| Text | System sans stack: `system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif` |
| Codes and identifiers | System mono stack (`--mono`): link codes, device IDs, frequencies in tables, API names |
| Page title (h1) | 1.45 rem, bold |
| Section (h2) | 1.1 rem, bold, letter spacing 0.01 em |
| Table headers | 0.78 rem, bold, capitals, letter spacing 0.05 em, `--ink-3` |
| Link codes | Mono, bold, letter spacing 0.08 em, in two groups: `WDJB-MJHT` |

No web fonts: pages load fast on a phone at the trackside and look native on every device.

## 7. Layout and components

- **Header.** Near-black bar, 52 px tall (48 px on phones), with the 4 px amber stripe beneath.
  The current page's nav item is amber with dark text.
- **Cards.** White surface (`--surface`) with a 1 px `--line` border, 6 px radius, a soft two-layer
  shadow, 14 px by 16 px padding.
- **Buttons.** Primary actions are filled teal; ordinary ones outlined; destructive ones red text
  and border, and they ask for confirmation. One primary button per form.
- **Badges.** Small outlined pills with a word: ok, warn, danger, or neutral.
- **Flashes.** A bordered line at the top of a page after an action: green text and border for
  done, red for errors, always with what happened in words.
- **Phones.** Below 900 px the map takes the screen and lists live in a bottom sheet; below 640 px
  the header compacts. Everything must work at 360 px wide with a 16 px gutter.

## 8. Voice

RailSpotting talks like a knowledgeable neighbor who listens to the scanner: plain, specific,
friendly, never breathless.

**Do**

- Say what happened, then what to do: "Main Street is offline. It stopped checking in. Check its power
  and internet connection."
- Be specific with numbers and places: "MP 475.8", "160.950 MHz", "4 of 5 confirmed trains",
  "about 70 a minute".
- Say how sure we are: "estimated", "probably", "unverified", "Possible defect (unverified)".
- Use sentence case for headings, buttons and titles: "Apply to go live", not "Apply To Go Live".
- Write "you" to the person reading, and name things the way the interface does.
- Keep sentences short. One idea each.

**Do not**

- Use em dashes. Use a comma, a colon, parentheses or a new sentence.
- Use filler and stock phrases ("seamless", "leverage", "unlock", "robust", "simply", "just",
  "it's worth noting", "in today's world"), exclamation marks, or emoji in the interface.
- Overstate: never "the crossing is clear", "the train will arrive", "real-time" (it is near
  live, from radio, and can miss trains).
- Blame people: "That code didn't work" rather than "You entered an invalid code".
- Leak detail to the wrong reader: owners hear that a receiver was taken off the map, not the
  findings behind it; the public never sees exact receiver locations.

**Safety line.** Wherever train information could be read as a go-ahead, say: RailSpotting is an
estimate, for interest only, and cannot say a crossing is clear.

## 9. Words we use

| Word | Means | Notes |
|---|---|---|
| receiver | A volunteer's radio listening post | Not "sensor", "station" or "node" |
| owner | The person whose account a receiver is on | |
| crossing | A public or private grade crossing from the FRA list | |
| train | A train placed by what receivers heard | Not "consist" unless it is one |
| EOT / HOT | End-of-train / head-of-train device | Spell out on first use on a page |
| detector | A wayside defect detector | "Defect detector" on first use |
| crew radio | Voice traffic on the road channel | |
| link code | The code a receiver shows to be added (`WDJB-MJHT`) | Not "pairing code" (retired) |
| provisional | Added, not yet public | For receivers and detectors |
| go live | A receiver's alerts become public | "Apply to go live" |
| quarantined | Pulled from everything public for review | |
| milepost, MP | Railroad milepost | "MP 475.8" in tables and alerts |

Railroads are named in full on first use ("Norfolk Southern", "CSX Transportation") and by
reporting mark after (NS, CSX).

## 10. Imagery and photos

- RailSpotting never copies people's photos or videos: notes link to YouTube and Google Photos,
  and YouTube plays in YouTube's own player.
- Do not use railroad logos, heralds or trademarks as decoration, or imply a railroad endorses or
  operates RailSpotting.
- Maps use OpenStreetMap tiles, credited on the map.

## 11. Keeping the brand in one place

Everything brand lives in `branding/`: this guide, [logo-kit.md](logo-kit.md) and the logo files.
The site serves copies from `web/static/img/`; when a logo changes, change it in `branding/logos/`
first, then copy it over (logo-kit.md lists the files the site uses).
