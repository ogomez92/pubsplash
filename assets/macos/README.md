# macOS packaging assets

Three files, and the two plists are the ones that decide whether the app runs.

| File | What it is |
| --- | --- |
| `Info.plist` | Extra keys merged into the bundle's own. The microphone and audio-capture usage descriptions live here. |
| `entitlements.plist` | Hardened-runtime entitlements, required for notarization. |
| `../icon/pubsplash.icns` | The app icon, generated from the same artwork as the Windows `.ico`. |

## Rebuilding the icon

`pubsplash.icns` is generated, not hand-drawn. From the repository root:

```sh
sips -s format png assets/icon/pubsplash.ico --out /tmp/pubsplash.png
mkdir -p /tmp/pubsplash.iconset
for s in 16 32 128 256 512; do
  sips -z $s $s /tmp/pubsplash.png --out /tmp/pubsplash.iconset/icon_${s}x${s}.png
  sips -z $((s*2)) $((s*2)) /tmp/pubsplash.png --out /tmp/pubsplash.iconset/icon_${s}x${s}@2x.png
done
iconutil -c icns /tmp/pubsplash.iconset -o assets/icon/pubsplash.icns
```

## Building a bundle locally

```sh
cargo install cargo-packager --locked
cargo packager --release --formats app,dmg
```

The result is `target/release/Pubsplash.app` and a `.dmg` beside it. A local
build is **ad-hoc signed**: it runs on the machine that built it, and macOS will
refuse it anywhere else. That is enough to test the microphone prompt, plugin
loading and the permissions — everything except distribution.

## Signing and notarizing

Real signing needs credentials that are not in this repository, and the release
workflow supplies them from repository secrets:

| Secret | What it is |
| --- | --- |
| `APPLE_CERTIFICATE` | The Developer ID Application certificate, as base64 of a `.p12`. |
| `APPLE_CERTIFICATE_PASSWORD` | That `.p12`'s password. |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)`. |
| `APPLE_ID` | The Apple ID that notarizes. |
| `APPLE_PASSWORD` | An **app-specific password** for that Apple ID, not the account password. |
| `APPLE_TEAM_ID` | The ten-character team id. |

Until those exist the macOS job still builds and uploads an **unsigned** bundle,
so packaging stays testable; it just cannot be distributed. Notarization is
skipped rather than failed when the secrets are absent, because a fork building
this repository should not have its release break on credentials it cannot have.

**Developer ID, not the Mac App Store.** Core Audio process taps do not work
under the App Sandbox, and an Application source is built on one.
