# Beam

GPU-accelerated remote desktop for Linux, streaming to any browser via WebCodecs.

Fully open source, GPU-accelerated, sub-30ms latency. Built for developers who want to access their Linux workstation from any browser.

<!-- TODO: Add screenshot or demo GIF here showing a desktop session with performance overlay (F9) -->
<!-- ![Beam desktop session](docs/screenshot.png) -->

## Features

- **GPU-accelerated encoding** — NVIDIA NVENC, AMD/Intel VA-API, or x264 software fallback
- **Low-latency streaming** — sub-30ms on LAN, WebCodecs hardware decode in the browser
- **Zero-install client** — works in Chrome 94+, Firefox 130+. No plugins, no app
- **Multi-user** — isolated virtual desktop sessions with PAM authentication
- **Audio streaming** — PulseAudio capture with Opus encoding
- **Clipboard sync** — copy/paste between local and remote desktops
- **Cursor shape passthrough** — remote cursor shape (text, pointer, resize, etc.) reflected locally
- **Dynamic resolution** — desktop resizes to match your browser viewport
- **Session persistence** — sessions survive server restarts (zero-downtime deploys)
- **Reconnect without re-login** — connection loss shows a reconnect overlay, no need to re-enter credentials
- **Mac keyboard support** — Cmd-to-Ctrl remapping, smooth trackpad scrolling
- **Auto keyboard layout** — detects your keyboard layout and syncs to the remote desktop
- **Performance overlay** — press F9 to see FPS, decode time, bitrate, and resolution
- **120fps default** — smooth desktop experience at high frame rates

## Install (Ubuntu 24.04 / 26.04 / Debian 13+)

> **Ubuntu 26.04 note**: Beam now picks the right runtime stack automatically.
> On 26.04 the installer uses `pipewire-pulse` instead of `pulseaudio` and
> pre-installs `cage` + `xdg-desktop-portal-wlr` so a future switch to
> `backend = "wayland"` is a config edit, not a fresh apt run. The default
> backend on 26.04 stays `xorg` until the Wayland code path is fully wired
> (see *Choosing a backend* below).


### APT Repository (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/frecar/beam/gh-pages/gpg/beam.gpg | gpg --dearmor | sudo tee /etc/apt/keyrings/beam.gpg > /dev/null
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/beam.gpg] https://raw.githubusercontent.com/frecar/beam/gh-pages stable main" | sudo tee /etc/apt/sources.list.d/beam.list
sudo apt-get update && sudo apt-get install beam
```

Or use the one-liner:
```bash
curl -fsSL https://raw.githubusercontent.com/frecar/beam/gh-pages/install | sudo bash
```

After installation, open `https://<your-server>:8444` and log in with any Linux user account.

### Build from Source

```bash
git clone https://github.com/frecar/beam.git
cd beam
sudo make install
sudo systemctl enable --now beam
```

Requires Ubuntu 24.04+ or Debian 13+ (x86_64 or ARM64). Rust and Node.js are installed automatically if not present.

### GPU Support

Beam auto-detects your GPU and selects the best encoder:
- **NVIDIA** — requires drivers 535+ (`nvidia-smi` to check). Uses NVENC for lowest latency
- **AMD / Intel** — uses VA-API (`vainfo` to check)
- **No GPU** — falls back to x264 software encoding (higher CPU usage, still functional)

## Verify

```bash
beam-server --version
beam-doctor
```

## Configuration

Edit `/etc/beam/beam.toml` (installed) or `config/beam.toml` (development):

```toml
[server]
bind = "0.0.0.0"
port = 8444
# web_root = "/usr/share/beam/web/dist"  # set automatically on install
# tls_cert = "/etc/beam/cert.pem"        # auto-generated if absent
# tls_key = "/etc/beam/key.pem"

[video]
bitrate = 5000      # kbps (initial target)
framerate = 120
# encoder = "nvh264enc"  # auto-detected: nvh264enc > vah264enc > x264enc
# max_width = 3840       # clamp resolution (default: 3840, 0 = unlimited)
# max_height = 2160      # clamp resolution (default: 2160, 0 = unlimited)

[audio]
enabled = true
bitrate = 128       # kbps (Opus)

[session]
max_sessions = 8
# idle_timeout = 3600  # seconds (0 = disabled, default: 3600)
```

### TLS Certificate

Beam auto-generates a self-signed certificate on first run. Browsers will show a security warning — click through it or set up a trusted certificate:

**Option A: mkcert (recommended for LAN/dev)**
```bash
# Install mkcert (creates a local CA trusted by your browser)
sudo apt install libnss3-tools
curl -JLO "https://github.com/FiloSottile/mkcert/releases/download/v1.4.4/mkcert-v1.4.4-linux-amd64"
sudo mv mkcert-v1.4.4-linux-amd64 /usr/local/bin/mkcert && sudo chmod +x /usr/local/bin/mkcert
mkcert -install

# Generate cert for your hostname
mkcert -cert-file /etc/beam/cert.pem -key-file /etc/beam/key.pem "$(hostname)" "$(hostname -I | awk '{print $1}')"
sudo systemctl restart beam
```

**Option B: Let's Encrypt (internet-facing servers)**
```bash
sudo apt install certbot
sudo certbot certonly --standalone -d beam.example.com
# Update /etc/beam/beam.toml:
# tls_cert = "/etc/letsencrypt/live/beam.example.com/fullchain.pem"
# tls_key = "/etc/letsencrypt/live/beam.example.com/privkey.pem"
sudo systemctl restart beam
```

**Option C: Existing certificate** — set `tls_cert` and `tls_key` in `/etc/beam/beam.toml`.

### Choosing a backend

Beam supports two virtual-display backends, selected by `[session] backend`
in `/etc/beam/beam.toml`:

| Value | What runs | When to use |
|---|---|---|
| `"auto"` (default) | Resolves to `wayland` on Ubuntu 26.04+, `xorg` everywhere else | Recommended. Set-and-forget. |
| `"xorg"` | `Xorg` with the `dummy` or `nvidia` driver | Stable, fastest path today. Required on any host without `cage`/`wlroots`. |
| `"wayland"` | Headless wlroots compositor (`cage` or `sway --backend=headless`) + PipeWire screencast capture | Future-proof on 26.04+. **Not yet implemented in the agent runtime** — currently exits with a clear error on startup. The installer prepares the system for it so flipping the value is a one-line change later. |

The auto-detection reads `/etc/os-release` once at server start. On Ubuntu
26.04 with the `wayland` backend selected today, `beam-agent` will refuse
to start and the doctor (`beam-doctor`) prints a warning telling you to
override with `backend = "xorg"`.

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| F11 | Toggle fullscreen |
| F9 | Toggle performance overlay (FPS, decode time, bitrate, resolution) |
| Esc | Exit fullscreen |

## Production Deployment

### Network

Beam needs only **port 8444/tcp** open (HTTPS + WebSocket). All video, audio, and input traffic flows over a single TLS WebSocket connection — no additional ports or UDP required.

- Beam binds to `0.0.0.0` by default — restrict with `bind = "10.0.0.1"` in `beam.toml` if needed

### Browser Requirements

- **Chrome / Edge**: 94+
- **Firefox**: 130+

Beam uses the WebCodecs API for hardware-accelerated video decoding in the browser.

### Reverse Proxy

Beam's IP-based rate limiting uses the direct TCP peer address. If running behind a reverse proxy (nginx, Caddy), all clients share the proxy's IP. Configure `bind = "127.0.0.1"` and handle TLS termination at the proxy level.

## Troubleshooting

Run the diagnostic tool:
```bash
beam-doctor
```

### Server starts but browser shows blank page
- Check that the web client is built: `ls web/dist/index.html`
- For installed systems, verify `web_root` in `/etc/beam/beam.toml` points to the right directory
- Run `make doctor` to check encoder availability

### Black screen after login
- Check agent logs: `journalctl -u beam -f` and `/var/log/beam/agent-*.log`
- Press F9 to open the performance overlay and check if frames are arriving
- This usually means H.264 frames aren't reaching the browser — force a reconnect (refresh the page)

### High latency or choppy video
- Press F9 to open the performance overlay and check RTT, FPS, and encoder
- High RTT (>50ms on LAN) may indicate network congestion
- Low FPS with high CPU may mean software encoding — install GPU drivers for hardware acceleration
- Try reducing resolution or bitrate in `/etc/beam/beam.toml`

### Non-US keyboard layout
- Beam auto-detects your keyboard layout in Chrome/Edge using the Keyboard Layout Map API
- If auto-detection doesn't work (Firefox, Safari), use the layout selector in the status bar
- Your layout choice is saved automatically for future sessions

View logs:
```bash
journalctl -u beam -f
```

## Uninstall

**APT package:**
```bash
sudo apt-get remove beam        # keep configuration
sudo apt-get purge beam         # remove everything
```

**Source install:**
```bash
sudo make uninstall
```

---

## Contributing

### Development Setup

```bash
./scripts/dev-setup.sh    # install Rust, Node, system deps
make install-hooks        # install the pre-push hook (fast fmt+tsc checks)
make doctor               # verify system readiness
```

### Build and Run

```bash
make dev                  # builds everything, starts server in debug mode
```

The server runs at `https://localhost:8444`. Log in with your Linux credentials.

### Make Targets

```
make dev            Build and run server (debug)
make build          Build everything (debug)
make build-release  Build everything (release)
make test           Run Rust tests + TypeScript type check
make lint           Run clippy + tsc
make check          Full pre-commit check (fmt + lint + test)
make doctor         Check system readiness
make build-release && sudo make deploy
                    Build as user, deploy as root
```

### Architecture

```
Browser (TypeScript)         Server (Rust/Axum)           Agent (Rust, per-user)
+-----------------+          +------------------+         +------------------+
| WebCodecs decode|<--WSS--->| HTTPS + WS       |<-spawn->| XCB/SHM capture  |
| Canvas render   |          | PAM auth + JWT   |         | GStreamer encode  |
| Input capture   |          | Session persist  |         | XTEST injection  |
| Cursor shape    |          | Binary frame     |         | Clipboard bridge |
| Clipboard sync  |          |   relay          |         | Cursor monitor   |
| Mac Cmd remap   |          | Rate limiting    |         | Audio capture    |
| Reconnect UI    |          +------------------+         +------------------+
| Perf overlay    |                                              |
+-----------------+                                        Virtual Display
                                                           (Xorg + dummy driver)
                                                           + XFCE4 desktop
                                                           + PulseAudio
```

The server handles authentication and signaling. When a user logs in, it spawns a per-user agent process that creates an isolated virtual display, captures the screen via XCB shared memory, encodes with GStreamer (NVENC/VA-API/x264), and streams to the browser over a WebSocket connection. The browser decodes frames using the WebCodecs API with hardware acceleration.

### Commit Conventions

`feat:` / `fix:` / `refactor:` / `chore:` / `docs:` / `test:` / `ci:` / `perf:` / `style:` / `build:` / `revert:` / `deps:` / `release:` (reserved for version bumps)

The pre-commit hook rejects commits that don't match these prefixes.

### Project Structure

```
beam/
  crates/
    server/     # HTTPS server, auth, session management, binary frame relay
    agent/      # Screen capture, encoding, WebSocket streaming, input injection
    protocol/   # Shared message types and config
  web/          # TypeScript browser client (Vite)
  config/       # Default configuration
  scripts/      # Install/uninstall/dev-setup scripts
  systemd/      # Service file
```

## Security

**Reporting**: Do not open public issues for security vulnerabilities. Use [GitHub private vulnerability reporting](https://github.com/frecar/beam/security/advisories/new). We aim to acknowledge within 48 hours.

**Security model**: PAM authentication, per-user isolated virtual displays, all traffic over a single TLS WebSocket (no P2P), JWT session tokens (24h), rate limiting (5 failures/60s per username + 20/60s per /64 prefix). See `CLAUDE.md` Security Decisions for settled implementation choices.

This project has not undergone a formal security audit.

## License

MIT
