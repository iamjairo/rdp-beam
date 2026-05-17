#!/usr/bin/env bash
set -euo pipefail

# Beam Remote Desktop - Install from source
# Run from the project root: sudo ./scripts/install.sh

INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/beam"
WEB_INSTALL_DIR="/usr/share/beam/web/dist"

log() { echo -e "\033[1;34m[beam]\033[0m $*"; }
err() { echo -e "\033[1;31m[beam]\033[0m $*" >&2; }
ok()  { echo -e "\033[1;32m[beam]\033[0m $*"; }

# Check root
if [[ $EUID -ne 0 ]]; then
    err "This script must be run as root: sudo ./scripts/install.sh"
    exit 1
fi

# Must be run from the project root
if [ ! -f "Cargo.toml" ] || ! grep -q "beam" Cargo.toml 2>/dev/null; then
    err "Run this script from the Beam source directory:"
    err "  cd /path/to/beam && sudo ./scripts/install.sh"
    exit 1
fi

# Detect architecture
ARCH=$(uname -m)
log "Architecture: $ARCH"

# Detect distro + version to choose the right dependency set.
# 26.04 ships PipeWire by default and is Wayland-only on the desktop session,
# so we install the Wayland backend's runtime there. 24.04 keeps the
# Xorg/PulseAudio stack as before.
OS_ID=""
OS_VERSION=""
if [ -r /etc/os-release ]; then
    . /etc/os-release
    OS_ID="${ID:-}"
    OS_VERSION="${VERSION_ID:-}"
fi
log "Distro: ${OS_ID:-unknown} ${OS_VERSION:-unknown}"

# Returns 0 if running on Ubuntu 26.04 or newer.
is_ubuntu_2604_plus() {
    [ "$OS_ID" = "ubuntu" ] || return 1
    case "$OS_VERSION" in
        26.04|26.10|27.*|28.*|29.*|3*) return 0 ;;
        *) return 1 ;;
    esac
}

# Detect GPU
detect_gpu() {
    if command -v nvidia-smi &>/dev/null && nvidia-smi &>/dev/null; then
        echo "nvidia"
    elif [ -d /sys/class/drm ] && ls /sys/class/drm/card*/device/vendor 2>/dev/null | head -1 | xargs cat 2>/dev/null | grep -q "0x1002"; then
        echo "amd"
    elif [ -d /sys/class/drm ] && ls /sys/class/drm/card*/device/vendor 2>/dev/null | head -1 | xargs cat 2>/dev/null | grep -q "0x8086"; then
        echo "intel"
    else
        echo "none"
    fi
}

GPU=$(detect_gpu)
log "GPU: $GPU"

# Install system dependencies
log "Installing system dependencies..."
apt-get update -qq

# Common build deps + Xorg/PulseAudio runtime for the existing backend.
COMMON_PKGS=(
    build-essential pkg-config cmake curl gnupg
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly
    libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev libxcb-xfixes0-dev
    xserver-xorg-video-dummy
    libpulse-dev libopus-dev
    libpam0g-dev libclang-dev
    xfce4 xfce4-terminal xfce4-whiskermenu-plugin gnome-keyring
    unclutter-xfixes epiphany-browser
)
# Node.js: Ubuntu 24.04 ships Node 18, which Vite 8 / Rolldown won't run on
# (needs node:util.styleText, Node 20.12+). Use NodeSource Node 22 LTS so
# `npm run build` in web/ works regardless of the host's apt repo state.
# Note: only needed at install time for source installs; the .deb shipping
# from CI bundles a pre-built web/dist and doesn't need nodejs at runtime.

# Pick an audio runtime: 26.04 uses PipeWire's pulse shim; older releases
# stay on PulseAudio. Both expose a /tmp/.../native socket that beam-agent
# can capture from, so the rest of the agent code is unaffected.
if is_ubuntu_2604_plus; then
    COMMON_PKGS+=(pipewire pipewire-pulse wireplumber)
    log "Ubuntu 26.04+ detected: installing PipeWire (with pulse shim) instead of PulseAudio"
else
    COMMON_PKGS+=(pulseaudio)
fi

# Wayland backend runtime (headless wlroots compositor + portals) — only on
# 26.04+ where users will actually want to enable backend = "wayland".
# Currently advisory: the agent's wayland code path is not yet implemented,
# but installing the runtime now means switching to backend=wayland later
# is a config edit, not a fresh apt install.
if is_ubuntu_2604_plus; then
    COMMON_PKGS+=(cage xdg-desktop-portal-wlr)
fi

apt-get install -y -qq "${COMMON_PKGS[@]}"

# Install Node 22 from NodeSource (after the base apt run so curl is present).
# If a recent Node is already on the system, leave it alone.
NODE_MAJOR=0
if command -v node >/dev/null 2>&1; then
    NODE_MAJOR=$(node -p "process.versions.node.split('.')[0]" 2>/dev/null || echo "0")
fi
if [ "${NODE_MAJOR:-0}" -lt 20 ]; then
    log "Installing Node.js 22 from NodeSource (current: ${NODE_MAJOR:-none})..."
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null 2>&1 || true
    apt-get install -y -qq nodejs
fi

# GPU-specific packages
case "$GPU" in
    nvidia)
        log "Installing NVIDIA GStreamer plugins..."
        apt-get install -y -qq gstreamer1.0-plugins-bad || true
        ;;
    amd|intel)
        log "Installing VA-API GStreamer plugins..."
        apt-get install -y -qq gstreamer1.0-vaapi || true
        ;;
    *)
        log "No GPU detected, will use software encoding (x264)"
        ;;
esac

# Install Rust if not present
if ! command -v rustc &>/dev/null; then
    log "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

# Build web client
log "Building web client..."
cd web
npm install
npm run build
cd ..

# Build Rust binaries
log "Building server and agent..."
cargo build --release --workspace

# Install binaries
log "Installing binaries to $INSTALL_DIR..."
install -m 755 target/release/beam-server "$INSTALL_DIR/"
install -m 755 target/release/beam-agent "$INSTALL_DIR/"

# Install web client
log "Installing web client to $WEB_INSTALL_DIR..."
mkdir -p "$WEB_INSTALL_DIR"
cp -r web/dist/* "$WEB_INSTALL_DIR/"

# Create runtime directories
log "Creating runtime directories..."
mkdir -p /var/lib/beam/sessions
chmod 700 /var/lib/beam

# Install config (don't overwrite existing)
log "Installing configuration..."
mkdir -p "$CONFIG_DIR"
if [ ! -f "$CONFIG_DIR/beam.toml" ]; then
    cp config/beam.toml "$CONFIG_DIR/beam.toml"
    # Set production web_root (absolute path)
    sed -i '/^\[server\]/a web_root = "/usr/share/beam/web/dist"' "$CONFIG_DIR/beam.toml"
    chmod 644 "$CONFIG_DIR/beam.toml"
fi

# Install systemd service
log "Installing systemd service..."
install -m 644 systemd/beam.service /etc/systemd/system/
systemctl daemon-reload

# Set up uinput access
log "Configuring uinput access..."
cat > /etc/udev/rules.d/99-beam-uinput.rules << 'EOF'
KERNEL=="uinput", MODE="0660", GROUP="input"
EOF
udevadm control --reload-rules
udevadm trigger

# Install PAM config (required for systemd-logind session registration)
log "Installing PAM configuration..."
if [ ! -f /etc/pam.d/beam ]; then
    install -m 644 packaging/pam.d/beam /etc/pam.d/beam
fi

# Validate installation
log "Validating installation..."
if ! "$INSTALL_DIR/beam-server" --help >/dev/null 2>&1; then
    err "beam-server binary validation failed"
    exit 1
fi

PORT=$(grep -oP 'port\s*=\s*\K\d+' "$CONFIG_DIR/beam.toml" 2>/dev/null || echo "8444")

ok ""
ok "Beam installed successfully!"
ok ""
ok "  Start:   sudo systemctl enable --now beam"
ok "  Status:  sudo systemctl status beam"
ok "  Logs:    journalctl -u beam -f"
ok ""
ok "  Open https://$(hostname):$PORT in your browser"
ok "  Log in with any Linux user account on this machine"
ok ""
