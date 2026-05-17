# Tauri update signing

The Tauri shell signs every updater bundle with a private key so that users'
installed apps can verify a downloaded update is authentic before running it.
Without this, a compromised release channel could deliver arbitrary code to
every Beam install. The signing keypair is project-specific and separate from
any Apple or code-signing certificate.

## Prerequisites

Before following this runbook, confirm you have:

- [ ] Write access to the GitHub repository (to add secrets)
- [ ] A password manager (1Password, Bitwarden, or equivalent) — the private
      key and password MUST be stored there, not on disk
- [ ] `cargo` installed locally (`rustup` default toolchain is sufficient)
- [ ] `cargo-tauri` CLI — install once with `cargo install tauri-cli`

You do NOT need an Apple Developer account for Tauri signing. That is covered
separately in `desktop-electron/NOTARIZATION.md`.

## What is already done (public side)

The public key is already committed in
[`tauri.conf.json`](tauri.conf.json) under `plugins.updater.pubkey`. You do
not need to touch that file. The key id printed by `cargo tauri signer
generate` (`62 81 E5 13 B6 B4 07 74`) matches the committed value.

Only the **two private secrets** need provisioning before CI can sign builds.

## Current state

- **Public key**: committed in `tauri.conf.json` — already in place.
- **Private key**: should live in your password manager. If a file
  `~/.beam-tauri-signing-key` exists on your workstation, move it to the
  password manager and `rm -P ~/.beam-tauri-signing-key` (secure-erase).
- **Private-key password**: similarly in the password manager; remove
  `~/.beam-tauri-signing-key.password` from disk after importing.

## First-time setup walkthrough

Follow these steps in order if no keypair exists yet (fresh repo clone or
rotation).

**Step 1 — Install the Tauri CLI**

```bash
cargo install tauri-cli
```

**Step 2 — Generate the keypair**

Run this from the repo root. Replace the password with a 40-char random
string (the command below generates one automatically):

```bash
SIGNING_PASSWORD="$(openssl rand -base64 30 | tr -d '/=+')"
cd desktop-tauri
cargo tauri signer generate \
  --ci \
  --password "$SIGNING_PASSWORD" \
  --write-keys ~/.beam-tauri-signing-key
echo "Password: $SIGNING_PASSWORD"
```

Two files are created:
- `~/.beam-tauri-signing-key` — private key (base64 minisign format, ~348 bytes)
- `~/.beam-tauri-signing-key.pub` — public key (already matches `tauri.conf.json` if this is a first run)

Copy both the private key content and the password into your password manager
before proceeding.

**Step 3 — Add secrets to GitHub**

Navigate to:

```
GitHub → [your repo] → Settings → Secrets and variables → Actions → New repository secret
```

Add these two secrets:

| Secret name | Value |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Full contents of `~/.beam-tauri-signing-key` (paste verbatim, including the `untrusted comment:` header line) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The 40-char password from Step 2 |

**Step 4 — Remove on-disk copies**

```bash
rm -P ~/.beam-tauri-signing-key ~/.beam-tauri-signing-key.pub ~/.beam-tauri-signing-key.password 2>/dev/null || true
```

**Step 5 — Verify**

Push a tagged release (e.g. `git tag v0.1.0 && git push origin v0.1.0`) and
watch the `build-tauri` job in GitHub Actions. A successful signing step prints
a line containing `sign` and the bundle path. No `TAURI_SIGNING_PRIVATE_KEY`
error means the secrets are wired correctly.

## CI secrets the workflow needs

For reference — these are the two secrets the `build-tauri` matrix job reads:

| Secret name | Value |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Contents of `~/.beam-tauri-signing-key` (base64 minisign secret key). Paste verbatim. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The password (40 char random ASCII). |

The build workflow reads them as env vars during `cargo tauri build`; no
file-system staging required on the CI runner.

## FAQ

**Q: What if I want to test signing locally?**

Set the env vars in your shell before running `cargo tauri build`:

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.beam-tauri-signing-key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="your-password-here"
cd desktop-tauri
cargo tauri build
```

The build output will include a `.sig` file alongside each updater bundle.

**Q: What if the build job says the pubkey is the placeholder?**

The `tauri.conf.json` ships with a placeholder pubkey (`dW50cnVzdGVkIGNvbW1l...`
or similar). If CI prints an error like "public key mismatch" or "updater
pubkey is placeholder", the keypair generated in Step 2 was not committed.
Run `cargo tauri signer generate --ci ...` again, copy the `.pub` output,
and update `plugins.updater.pubkey` in `tauri.conf.json`, then commit.

**Q: How do I confirm a bundle is signed correctly after a local build?**

```bash
cargo tauri signer verify \
  --pubkey "$(grep pubkey desktop-tauri/tauri.conf.json | awk -F'"' '{print $4}')" \
  path/to/bundle.tar.gz
```

A successful run exits 0 and prints `Signature valid`.

## Rotation procedure

A new keypair invalidates every existing installation's auto-update path —
users on the old pubkey must manually install a build signed with the new
one. Treat rotation as a coordinated release, not an emergency patch.

1. Tag a release-candidate version on the **old** key. Verify the existing
   install path still works.
2. Generate the new keypair:
   ```bash
   cd desktop-tauri
   cargo tauri signer generate --ci --password "$(openssl rand -base64 30 | tr -d '/=+')" --write-keys ~/.beam-tauri-signing-key.new
   ```
3. Replace the pubkey in `tauri.conf.json` with the contents of
   `~/.beam-tauri-signing-key.new.pub`.
4. Rotate the two GitHub Actions secrets to the new private key + password.
5. Tag and ship one final release on **both** keys (dual-signed) so existing
   installations can still auto-update to a release that itself prompts the
   user to manually install the next one. (Tauri's updater plugin does not
   currently support dual-signing — in practice, this means the rotation
   release ships through both the auto-updater on the old key AND a manual
   download on the new key, with a release-notes call-out.)
6. Archive the old private key in cold storage (offline encrypted volume)
   for 6 months in case of urgent re-signing needs.

## If the private key leaks

1. Pull every release artefact from GitHub Releases to prevent further auto-update fanout.
2. Generate a fresh keypair immediately (step 2 above).
3. Tag a force-version bump (e.g. `0.3.22 → 0.4.0`) so existing installs are
   pinned to the last known-good version and won't auto-pull anything else.
4. Notify users via the README, release notes, and any deployment channel
   (Slack, GitHub Discussions) that auto-update is suspended pending the
   forced re-install.
5. Ship a manual-install release on the new key. Document the migration
   path. Forensically review how the leak happened before re-enabling
   auto-update.

## Security constraints

- The pubkey in `tauri.conf.json` is **public by design** — committing it is
  correct.
- The private key (`~/.beam-tauri-signing-key`) MUST NOT be committed.
  Check `git status` before every commit involving this directory.
- Password length: 40 chars minimum. Generated via `openssl rand -base64`
  (CSPRNG). Don't pick a human-memorable password.
