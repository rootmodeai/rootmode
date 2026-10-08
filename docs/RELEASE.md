# Desktop releases

A tag `vX.Y.Z` builds the Tauri app on macOS (signed and notarized), Windows,
and Linux, then publishes installers to GitHub Releases. The website
**Download rootmode** button hits `/download`, which redirects to the
installer that matches the visitor's OS.

```sh
# version is apps/desktop/src-tauri/tauri.conf.json + workspace Cargo.toml
git tag v0.1.0
git push origin v0.1.0
```

GitHub → Settings → Actions → Workflow permissions: **Read and write**.

## What gets published

Versioned Tauri artifacts *and* stable names the site uses:

| File | Who gets it |
|---|---|
| `rootmode-macos-arm64.dmg` | Mac (Apple Silicon) — the Download button |
| `rootmode-macos-x64.dmg` | Intel Mac |
| `rootmode-windows-x64.msi` | Windows |
| `rootmode-linux-x86_64.AppImage` | Linux |

The Linux AppImage bundles GStreamer (`bundleMediaFramework`) so the intro
film and in-app video can play. The desktop binary also repairs an empty
`GST_PLUGIN_SYSTEM_PATH_1_0` (linuxdeploy sets this even when no plugins
were copied) and, on NVIDIA, sets `WEBKIT_DISABLE_DMABUF_RENDERER=1`.

`https://github.com/<org>/rootmode/releases/latest/download/<file>` always
points at the newest tag.

## Apple signing and notarization

Without these secrets the macOS job fails. An unsigned or unstapled `.dmg`
is what Gatekeeper rejects when someone downloads it, and the app inside
never gets the chance to run.

`tauri-action` signs and notarizes the `.app`. That is not the file a
browser saves. The workflow then submits the `.dmg` itself to the notary
service and staples the ticket onto the image before uploading the stable
name the Download button serves. A release that went out without that
ticket can be repaired without a rebuild:

```sh
gh workflow run release.yml -f repair_tag=v0.1.25
```

1. Apple Developer Program. Create a **Developer ID Application** certificate.
2. Keychain Access → export that cert + private key as a `.p12`.
3. Apple ID → App-specific password (for notarization).
4. GitHub repo secrets:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | `base64 < cert.p12` (one line) |
| `APPLE_CERTIFICATE_PASSWORD` | password you set on the `.p12` |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID` | Apple ID email |
| `APPLE_PASSWORD` | app-specific password |
| `APPLE_TEAM_ID` | 10-character team id |

macOS:

```sh
base64 -i DeveloperID.p12 | pbcopy   # paste into APPLE_CERTIFICATE
```

## Website

`/download` is served by the stats process (same box as the pages). After a
release, redeploying the site is **not** required for the files — they live
on GitHub. Redeploy only if you changed the download button or redirect
logic.

The GitHub repo the redirect uses is `rootmodeai/rootmode`. Override with
`ROOTMODE_GITHUB_REPO` on the stats container if the origin moves.
