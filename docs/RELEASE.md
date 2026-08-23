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

`https://github.com/<org>/rootmode/releases/latest/download/<file>` always
points at the newest tag.

## Apple signing and notarization

Without these secrets the macOS job still builds, but Gatekeeper will block
the `.dmg` for people who did not right-click → Open. With them,
`tauri-action` signs, notarizes, and staples.

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
