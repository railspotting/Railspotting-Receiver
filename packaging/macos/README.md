# macOS releases

The desktop app (`apps/desktop`) is released from GitHub: a `.dmg` for new installs, and a signed
update bundle that installed apps update themselves from.

## Publishing a release

    tools/release.sh patch "What changed, in a sentence or two"

The script tags the tip of `main` (once its CI has passed) and pushes only the tag. The Release
workflow (`.github/workflows/release.yml`) then:

1. sets the version from the tag (`v2.0.1` makes the app and the engine report 2.0.1);
2. builds `RailSpotting.app` for Apple Silicon, with the `.dmg` and the update bundle;
3. publishes a GitHub Release `v2.0.1` with:
   - `RailSpotting-2.0.1-arm64.dmg`, for new installs;
   - `RailSpotting-2.0.1-arm64.app.tar.gz`, the update bundle, signed with the updater key;
   - `latest.json`, which installed apps read (`release-files.sh` writes it).

Installed apps check a minute after they start and every six hours. A newer version shows in the
menu as **Install Update 2.0.1 and Restart**; **Check for Updates...** looks at once. The update
installs only when asked; the receiver stops as on Quit, then the app restarts on the new
version.

Use the smallest version step: `patch` for fixes and small features. `minor` only when the owner
decides a release is that big. A new major version changes `Cargo.toml` first.

## Keys and secrets

| Secret (repository, Actions) | What |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | the updater key. Set. Its public half is `plugins.updater.pubkey` in `apps/desktop/tauri.conf.json` |
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` | a Developer ID Application certificate (the `.p12`, base64) and its name. Not set yet |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | notarization: the Apple ID, an app-specific password, the team. Not set yet |

The updater key and its password are kept off the repository, with the owner's other release
keys. Losing the key means installed apps can't take updates any more (they would need a new
install with a new key), so keep a backup. A leaked key lets someone sign an update that
installed apps accept: rotate it and publish a release with the new public key at once.

**Until the Apple secrets are set,** releases are ad-hoc signed and not notarized. The workflow
warns about it and carries on. On a new install, macOS says it can't check the app: open it once
from System Settings, Privacy & Security, **Open Anyway**. Updates through the app don't ask
again. Once the Apple secrets are set, the same workflow signs with the Developer ID, notarizes
and staples, with no change to it.

## What the app needs

- **Hardened runtime, no App Sandbox.** The sandbox blocks the raw USB and serial access the
  receiver needs, which is also why the Mac App Store is out of scope.
- **librtlsdr from Homebrew** (`brew install librtlsdr`). It is GPL and loaded at run time, never
  bundled.

## Testing on this Mac

`install-local.sh` builds the app and copies it to `/Applications`, unsigned, for the Mac that
built it. Debug builds (`cargo tauri dev`) never check for updates by themselves.
