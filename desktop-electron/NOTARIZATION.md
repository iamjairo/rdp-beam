# macOS Notarization — Beam Desktop (Electron + Tauri)

Notarization lets macOS users open Beam without a "unidentified developer"
warning. Apple scans the signed app, returns a ticket, and that ticket is
stapled to the build artifact. Gatekeeper checks it at launch. Without it,
macOS 10.15+ blocks the app by default. Both the Electron build (`build-electron`)
and the Tauri build (`build-tauri`) require the secrets in this document.

## Prerequisites

Before following this runbook, confirm you have:

- [ ] **Paid Apple Developer Program membership** — individual or organization,
      active. Cost: $99/yr at developer.apple.com. Membership must be active at
      build time; an expired membership causes notarization submissions to fail.
- [ ] **A Mac** — certificate export and Keychain operations require macOS.
- [ ] Write access to the GitHub repository (to add secrets).
- [ ] A password manager — five credentials need safe storage.

## What is notarization

Apple Notarization is a security scan run by Apple's automated service before
a Mac app can be distributed outside the App Store. The developer submits a
signed `.dmg` or `.app` to Apple, Apple scans it for malware and policy
violations, and Apple returns a notarization ticket. The ticket is then
"stapled" to the artifact. When a user opens the app, Gatekeeper verifies the
ticket (online or via staple) and permits launch without a "unidentified
developer" warning. Without notarization, macOS 10.15+ blocks the app by
default.

## First-time setup walkthrough

Follow these steps in order. Each step produces one or more values you will
paste into GitHub Actions secrets at the end.

**Step 1 — Enroll in the Apple Developer Program**

Go to developer.apple.com → Account → join the Apple Developer Program.
Complete enrollment and payment. Approval can take minutes to a day.

**Step 2 — Create a Developer ID Application certificate**

In the Apple Developer portal:

1. Go to Certificates, Identifiers & Profiles → Certificates → (+).
2. Select **Developer ID Application** (not Distribution, not Mac App Store).
3. Follow the CSR creation flow (Keychain Access → Certificate Assistant →
   Request a Certificate from a Certificate Authority, save to disk).
4. Upload the CSR; download the resulting `.cer` file.
5. Double-click the `.cer` to install it into Keychain Access.

**Step 3 — Export the certificate as .p12**

In Keychain Access:

1. Find the "Developer ID Application: [Your Name]" certificate.
2. Expand it; select both the certificate row and the private key row beneath it.
3. Right-click → Export 2 Items → save as `DeveloperIDApplication.p12`.
4. Set a strong password. Record this password — it becomes
   `MAC_CERTIFICATE_PASSWORD`.

**Step 4 — Base64-encode the certificate**

```bash
base64 -i DeveloperIDApplication.p12 | pbcopy   # macOS — copies to clipboard
```

The clipboard content becomes `MAC_CERTIFICATE_BASE64`. Paste it into your
password manager before the clipboard is cleared.

**Step 5 — Generate an app-specific password**

1. Go to appleid.apple.com → Sign-In and Security → App-Specific Passwords → (+).
2. Label it `beam-notarization`.
3. Copy the generated password. This becomes `APPLE_APP_PASSWORD`.
   This is NOT your iCloud account password.

**Step 6 — Find your Team ID**

In the Apple Developer portal, click the account name in the top-right corner.
The 10-character Team ID (e.g. `AB12CD34EF`) is shown beneath the account name.
It also appears on any certificate detail page.

**Step 7 — Add secrets to GitHub**

Navigate to:

```
GitHub → [your repo] → Settings → Secrets and variables → Actions → New repository secret
```

Add all five secrets:

| Secret name | Value |
|---|---|
| `APPLE_ID` | Your Apple ID email (e.g. `dev@example.com`) |
| `APPLE_TEAM_ID` | 10-char Team ID from the Developer portal (e.g. `AB12CD34EF`) |
| `APPLE_APP_PASSWORD` | App-specific password from Step 5 |
| `MAC_CERTIFICATE_BASE64` | Base64-encoded `.p12` from Step 4 |
| `MAC_CERTIFICATE_PASSWORD` | `.p12` export password from Step 3 |

The Tauri job also reads `APPLE_CERTIFICATE` and `APPLE_CERTIFICATE_PASSWORD`
— the workflow maps the same `.p12` secrets to both naming conventions, so no
additional secrets are needed.

**Step 8 — Trigger a build**

Push a tagged release (e.g. `git tag v0.1.0 && git push origin v0.1.0`) and
watch both `build-electron` and `build-tauri` in GitHub Actions. Both jobs
print a notarization submission ID and a final "Notarization successful" line
when the secrets are wired correctly.

## Required Apple Developer account state

1. **Paid Apple Developer Program membership** — individual or organization,
   active and not expired.
2. **Developer ID Application certificate** — issued in Xcode or the Apple
   Developer portal (Certificates, Identifiers & Profiles). This is the
   certificate used for distribution outside the App Store. Export it as a
   `.p12` with a strong password.
3. **App-specific password** — generated at appleid.apple.com under
   "Sign-In and Security → App-Specific Passwords". Use a label like
   `beam-notarization`. This is NOT your iCloud account password.
4. **Team ID** — the 10-character identifier visible in the Apple Developer
   portal next to the account name (e.g. `AB12CD34EF`).

## Required GitHub Actions secrets

These secrets must be added to the repository under
Settings → Secrets and variables → Actions. Both workflow jobs read them
as environment variables at build time.

| Secret name | Contents |
|---|---|
| `APPLE_ID` | The Apple ID email address of the developer account (e.g. `dev@example.com`). |
| `APPLE_TEAM_ID` | The 10-character Team ID from the Apple Developer portal (e.g. `AB12CD34EF`). |
| `APPLE_APP_PASSWORD` | The app-specific password generated at appleid.apple.com. Not the iCloud password. |
| `MAC_CERTIFICATE_BASE64` | The Developer ID Application `.p12` file, base64-encoded (`base64 -i cert.p12`). |
| `MAC_CERTIFICATE_PASSWORD` | The password chosen when exporting the `.p12` from Keychain or the Developer portal. |

To base64-encode the certificate locally:

```bash
base64 -i DeveloperIDApplication.p12 | pbcopy   # macOS — copies to clipboard
```

Paste the result as the `MAC_CERTIFICATE_BASE64` secret value.

## How env vars map to CI jobs

The `build-electron` and `build-tauri` jobs consume these secrets under
different env-var names. The workflow handles the mapping; no manual wiring is
needed beyond the five secrets above.

| Secret name | Electron env var | Tauri env var | Purpose |
|---|---|---|---|
| `APPLE_ID` | `APPLE_ID` | `APPLE_ID` | Notarization API login |
| `APPLE_APP_PASSWORD` | `APPLE_APP_SPECIFIC_PASSWORD` | `APPLE_PASSWORD` | Notarization API password |
| `APPLE_TEAM_ID` | `APPLE_TEAM_ID` | `APPLE_TEAM_ID` | Identifies which team's cert |
| `MAC_CERTIFICATE_BASE64` | `CSC_LINK` (after import) | `APPLE_CERTIFICATE` | Signing identity |
| `MAC_CERTIFICATE_PASSWORD` | `CSC_KEY_PASSWORD` | `APPLE_CERTIFICATE_PASSWORD` | Unlocks the .p12 |

Note the name difference for the app password: electron-builder uses
`APPLE_APP_SPECIFIC_PASSWORD`; Tauri's toolchain uses `APPLE_PASSWORD`.

electron-builder's `mac.hardenedRuntime: true` (already set in `package.json`)
combined with the entitlement files and the three notarization env vars is
sufficient for electron-builder to sign, submit for notarization, wait for
approval, and staple the result — no additional scripting required.

The `build-electron` job also imports `MAC_CERTIFICATE_BASE64` into a temporary
Keychain before `electron-builder` runs:

```bash
echo "$MAC_CERTIFICATE_BASE64" | base64 --decode > /tmp/cert.p12
security create-keychain -p "" build.keychain
security import /tmp/cert.p12 -k build.keychain -P "$MAC_CERTIFICATE_PASSWORD" \
  -T /usr/bin/codesign
security list-keychains -d user -s build.keychain $(security list-keychains -d user | tr -d '"')
security set-key-partition-list -S apple-tool:,apple: -s -k "" build.keychain
```

## How to verify notarization worked

After a signed build lands in GitHub Releases, download the `.dmg`, mount it,
and run:

```bash
spctl -a -vvv -t install /Volumes/Beam/Beam.app
```

Expected output (success):

```
/Volumes/Beam/Beam.app: accepted
source=Notarized Developer ID
origin=Developer ID Application: Your Name (AB12CD34EF)
```

If the output says `rejected` or `source=no usable signature`, the notarization
secrets were not active during the build that produced this artifact.

You can also verify the stapled ticket directly:

```bash
xcrun stapler validate /Volumes/Beam/Beam.app
```

Expected: `The validate action worked!`

## Windows builds — SmartScreen warning (no EV signing yet)

Windows EV code signing is explicitly deferred with no timeline set. Windows
builds currently produce unsigned `.exe` and `.msi` artifacts. Users will see a
SmartScreen warning ("Windows protected your PC") on first launch. This is
expected behavior until EV signing is wired. Do not work around it by disabling
SmartScreen or by instructing users to do so.

## Rotation procedure

**App-specific password** — rotate every 6 months:
1. Generate a new app-specific password at appleid.apple.com.
2. Update the `APPLE_APP_PASSWORD` GitHub Actions secret.
3. Old password is immediately invalidated on regeneration.

**Developer ID Application certificate** — expires yearly:
1. Renew or reissue in the Apple Developer portal before expiry.
2. Export the new `.p12`, base64-encode it, and update `MAC_CERTIFICATE_BASE64`
   and `MAC_CERTIFICATE_PASSWORD` in GitHub Actions secrets.
3. All new builds after the secret update will use the new cert automatically.

Calendar reminder: set a repeating alert 30 days before the cert expiry date
(visible in Keychain Access or the Developer portal).

## Leak response

If `APPLE_APP_PASSWORD` is exposed:
1. Immediately revoke it at appleid.apple.com → App-Specific Passwords.
2. Generate a replacement and update the GitHub Actions secret.
3. Audit recent CI runs for unexpected notarization submissions.

If `MAC_CERTIFICATE_BASE64` / `MAC_CERTIFICATE_PASSWORD` is exposed:
1. **Revoke the certificate** in Apple Developer portal →
   Certificates, Identifiers & Profiles → Certificates → Revoke.
   Revocation propagates to Gatekeeper within minutes and invalidates all
   binaries signed with that cert — users will see Gatekeeper blocks again.
2. Issue a new Developer ID Application certificate. Export and update secrets.
3. **Force a version bump** and cut a new release immediately so users download
   a binary signed with the replacement cert.
4. Notify users if the revocation window means existing installed binaries are
   now blocked (Gatekeeper checks revocation online).

If `APPLE_ID` credentials are compromised more broadly, rotate the Apple ID
password and all app-specific passwords, then enable two-factor authentication
if not already active.

## Out of scope (deferred)

- **Windows EV code signing** — deferred, no timeline set. See the SmartScreen
  section above.
- **Linux code signing** — not applicable; Linux package integrity is handled
  via the APT repo GPG key (`docs/` or the gh-pages branch).
