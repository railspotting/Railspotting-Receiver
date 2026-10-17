# Logo kit: waveform track

Track seen from above, with ties that rise and fall like a radio signal. One shape says what
RailSpotting does: the line, heard by radio. The rails run off both edges of the tile, so the track
carries on past the frame.

Tile `#16191c` · Ties `#e0a100` · Rails `#f1efe9` · Dark edge `#2f353b`

All files are in [logos/](logos/). The rules for using them are in the
[brand guide](brand-guide.md#3-the-mark-waveform-track).

## Icons

The mark on its own, for browser tabs and home screens.

| File | Use |
|---|---|
| [favicon.svg](logos/favicon.svg) | Browser tab icon in modern browsers, and the master vector for every size from 24 px up. Source for icon-192.png and icon-512.png. |
| [favicon-16.svg](logos/favicon-16.svg) | Below 24 px: the 16 px tab icon, bookmarks and history lists. Four heavier ties and thicker rails so the shape survives at 16 px. favicon.ico uses this art for its 16 px size. |
| [icon-square.svg](logos/icon-square.svg) | Source for apple-touch-icon.png (180 px). Square corners because iOS and iPadOS round the icon themselves. |
| [icon-maskable.svg](logos/icon-maskable.svg) | Source for icon-maskable-512.png. Android crops it to a circle or squircle, so the ties sit inside the safe zone while the rails still run edge to edge. |
| [mark-dark.svg](logos/mark-dark.svg) | The mark with the hairline dark edge (as in the dark lockup), for the near-black site header on phones. Made from favicon.svg. |

## Horizontal lockups

Mark and wordmark side by side. The default logo for anything wider than it is tall.

| File | Use |
|---|---|
| [lockup-light.svg](logos/lockup-light.svg) | Light pages: email headers, documents, partner pages. |
| [lockup-dark.svg](logos/lockup-dark.svg) | The site header and any dark background. Includes the hairline tile edge so the tile shows on the near-black bar. |
| [lockup-auto.svg](logos/lockup-auto.svg) | README files and docs that follow the reader's light or dark setting. |

## Stacked lockups

Mark above wordmark, for square spaces.

| File | Use |
|---|---|
| [stacked-light.svg](logos/stacked-light.svg) | Square or tall spaces on light backgrounds: stickers, receiver enclosure labels, printed flyers, profile images. |
| [stacked-dark.svg](logos/stacked-dark.svg) | Square or tall spaces on dark backgrounds: social profile images, splash screens. |
| [stacked-auto.svg](logos/stacked-auto.svg) | Square spaces in docs that follow the reader's light or dark setting. |

## Wordmark

RAILSPOTTING alone, outlined from Inter Bold (SIL Open Font License) so it looks the same on every
device.

| File | Use |
|---|---|
| [wordmark-light.svg](logos/wordmark-light.svg) | The name alone where the mark already appears nearby, or where space is too short for the tile: footers, credits, the end of a video. |
| [wordmark-dark.svg](logos/wordmark-dark.svg) | The name alone on dark backgrounds. |
| [wordmark-auto.svg](logos/wordmark-auto.svg) | The name alone in docs that follow the reader's light or dark setting. |

## Banner

| File | Use |
|---|---|
| [banner-1200x630.svg](logos/banner-1200x630.svg) | Source of the Open Graph image shown when someone shares a railspotting.com link. Use the PNG in `og:image`; most sites do not accept SVG there. |

## PNG and ICO files

Rendered from the SVGs above at exact sizes, with transparent corners, in [logos/png/](logos/png/).

| File | Size | Made from | Use |
|---|---|---|---|
| favicon.ico | 16, 32, 48 px | favicon-16.svg (16), favicon.svg (32, 48) | Older browsers and Windows shortcuts |
| apple-touch-icon.png | 180 px | icon-square.svg | iPhone and iPad Home Screen |
| icon-192.png | 192 px | favicon.svg | Web app manifest, push notifications |
| icon-512.png | 512 px | favicon.svg | Web app manifest, install screens |
| icon-maskable-512.png | 512 px | icon-maskable.svg | Web app manifest, Android adaptive icon |
| banner-1200x630.png | 1200 by 630 | banner-1200x630.svg | `og:image` for link previews |

## On the site

The site serves copies of these files from `web/static/img/`:

| Site file | From |
|---|---|
| favicon.svg, favicon-16.svg, mark-dark.svg, lockup-dark.svg | logos/ |
| favicon.ico, apple-touch-icon.png, icon-192.png, icon-512.png, icon-maskable-512.png, banner-1200x630.png | logos/png/ |

Where they are used:

- **Page head** (public, account and admin pages): `favicon.ico` (48x48), `favicon.svg`,
  `apple-touch-icon.png`, theme color `#16191c`. Public pages also carry the Open Graph tags with
  `banner-1200x630.png`.
- **Header:** `lockup-dark.svg` at 45 px tall (the mark is then 30 px; negative margins take back
  the lockup's own padding), and `mark-dark.svg` at 28 px alone on phones.
- **Web app manifest** (`/manifest.webmanifest`): icon-192, icon-512 and icon-maskable-512;
  theme and background `#16191c`.
- **Push notifications:** `icon-192.png`.

To change a logo: update it in `branding/logos/`, render the PNGs again at the sizes above, and copy
the files listed here to `web/static/img/`.
