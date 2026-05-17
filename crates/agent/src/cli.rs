use anyhow::Context;
use uuid::Uuid;

pub(crate) const DEFAULT_BITRATE: u32 = 50_000; // 50 Mbps -- LAN default
pub(crate) const DEFAULT_FRAMERATE: u32 = 120; // 120fps

pub(crate) struct Args {
    pub display: String,
    pub server_url: String,
    pub session_id: Uuid,
    pub agent_token: Option<String>,
    pub tls_cert_path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate: u32,
    pub encoder: Option<String>,
    pub max_width: u32,
    pub max_height: u32,
    pub gpu_driver: String,
    pub display_start: u32,
    /// Display backend: "xorg" (X11 dummy/nvidia) or "wayland" (headless wlroots compositor).
    /// "auto" is resolved here by reading /etc/os-release; the server normally passes
    /// an already-resolved value, but the agent must also handle the unresolved case
    /// to stay testable in isolation.
    pub backend: String,
}

pub(crate) fn parse_args() -> anyhow::Result<Args> {
    let mut display = ":0".to_string();
    let mut server_url = String::new();
    let mut session_id = None;
    let mut agent_token = None;
    let mut tls_cert_path = None;
    let mut width: u32 = 1920;
    let mut height: u32 = 1080;
    let mut framerate: u32 = DEFAULT_FRAMERATE;
    let mut bitrate: u32 = DEFAULT_BITRATE;
    let mut encoder: Option<String> = None;
    let mut max_width: u32 = 3840;
    let mut max_height: u32 = 2160;
    let mut gpu_driver = "auto".to_string();
    let mut display_start: u32 = 10;
    let mut backend = "auto".to_string();

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-V" | "--version" => {
                println!("beam-agent {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("beam-agent - Beam Remote Desktop capture agent");
                println!();
                println!("USAGE:");
                println!("    beam-agent [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!("    --display <DISPLAY>          X11 display [default: :0]");
                println!("    --server-url <URL>           Signaling server WebSocket URL");
                println!("    --session-id <UUID>          Session identifier (required)");
                println!(
                    "    --agent-token <TOKEN>        Agent authentication token (prefer BEAM_AGENT_TOKEN env)"
                );
                println!(
                    "    --tls-cert <PATH>            TLS certificate to pin for server connection"
                );
                println!("    --width <PIXELS>             Initial display width [default: 1920]");
                println!("    --height <PIXELS>            Initial display height [default: 1080]");
                println!("    --framerate <FPS>            Target framerate [default: 120]");
                println!(
                    "    --bitrate <KBPS>             Initial video bitrate [default: 100000]"
                );
                println!(
                    "    --encoder <NAME>             Force encoder (nvh264enc, vah264enc, x264enc)"
                );
                println!("    --max-width <PIXELS>         Maximum resize width [default: 3840]");
                println!("    --max-height <PIXELS>        Maximum resize height [default: 2160]");
                println!(
                    "    --gpu-driver <MODE>          GPU driver: auto, nvidia, dummy [default: auto]"
                );
                println!(
                    "    --backend <MODE>             Display backend: auto, xorg, wayland [default: auto]"
                );
                println!("    --display-start <N>          Starting display number [default: 10]");
                println!("    -V, --version                Print version and exit");
                println!("    -h, --help                   Print this help and exit");
                std::process::exit(0);
            }
            "--display" => {
                i += 1;
                display = args.get(i).context("Missing --display value")?.clone();
            }
            "--server-url" => {
                i += 1;
                server_url = args.get(i).context("Missing --server-url value")?.clone();
            }
            "--session-id" => {
                i += 1;
                session_id = Some(
                    args.get(i)
                        .context("Missing --session-id value")?
                        .parse::<Uuid>()
                        .context("Invalid session-id UUID")?,
                );
            }
            "--agent-token" => {
                // Legacy CLI support (prefer BEAM_AGENT_TOKEN env var)
                i += 1;
                agent_token = Some(args.get(i).context("Missing --agent-token value")?.clone());
            }
            "--tls-cert" => {
                i += 1;
                tls_cert_path = Some(args.get(i).context("Missing --tls-cert value")?.clone());
            }
            "--width" => {
                i += 1;
                width = args
                    .get(i)
                    .context("Missing --width value")?
                    .parse()
                    .context("Invalid --width value")?;
            }
            "--height" => {
                i += 1;
                height = args
                    .get(i)
                    .context("Missing --height value")?
                    .parse()
                    .context("Invalid --height value")?;
            }
            "--framerate" => {
                i += 1;
                framerate = args
                    .get(i)
                    .context("Missing --framerate value")?
                    .parse()
                    .context("Invalid --framerate value")?;
            }
            "--bitrate" => {
                i += 1;
                bitrate = args
                    .get(i)
                    .context("Missing --bitrate value")?
                    .parse()
                    .context("Invalid --bitrate value")?;
            }
            "--encoder" => {
                i += 1;
                encoder = Some(args.get(i).context("Missing --encoder value")?.clone());
            }
            "--max-width" => {
                i += 1;
                max_width = args
                    .get(i)
                    .context("Missing --max-width value")?
                    .parse()
                    .context("Invalid --max-width value")?;
            }
            "--max-height" => {
                i += 1;
                max_height = args
                    .get(i)
                    .context("Missing --max-height value")?
                    .parse()
                    .context("Invalid --max-height value")?;
            }
            "--gpu-driver" => {
                i += 1;
                gpu_driver = args.get(i).context("Missing --gpu-driver value")?.clone();
            }
            "--backend" => {
                i += 1;
                backend = args.get(i).context("Missing --backend value")?.clone();
            }
            "--display-start" => {
                i += 1;
                display_start = args
                    .get(i)
                    .context("Missing --display-start value")?
                    .parse()
                    .context("Invalid --display-start value")?;
            }
            other => anyhow::bail!("Unknown argument: {other}"),
        }
        i += 1;
    }

    // Prefer env var for agent token (CLI args are visible in /proc)
    if agent_token.is_none() {
        agent_token = std::env::var("BEAM_AGENT_TOKEN").ok();
    }

    let resolved_backend = resolve_backend(&backend);

    Ok(Args {
        display,
        server_url,
        session_id: session_id.context("--session-id is required")?,
        agent_token,
        tls_cert_path,
        width,
        height,
        framerate,
        bitrate,
        encoder,
        max_width,
        max_height,
        gpu_driver,
        display_start,
        backend: resolved_backend,
    })
}

/// Resolve `backend = "auto"` to a concrete `"xorg"` or `"wayland"` based on
/// `/etc/os-release`. Mirrors the server-side logic in
/// `crates/server/src/session.rs::resolve_backend` so the agent stays
/// runnable standalone (manual `beam-agent` invocation, integration tests,
/// future single-binary deployments).
///
/// Non-`auto` inputs pass through unchanged. Anything other than
/// `auto`/`xorg`/`wayland` is left alone so the dispatcher in `main.rs`
/// can produce a clear error.
fn resolve_backend(value: &str) -> String {
    if value != "auto" {
        return value.to_string();
    }
    let osrel = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut id = String::new();
    let mut version_id = String::new();
    for line in osrel.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version_id = v.trim_matches('"').to_string();
        }
    }
    // Parse only the first two dot-separated components — handles both
    // `26.04` and `26.04.1` point releases without choking on the inner dot.
    let mut parts = version_id.split('.');
    let maj: Option<u32> = parts.next().and_then(|s| s.parse().ok());
    let min: Option<u32> = parts.next().and_then(|s| s.parse().ok());
    let is_ubuntu_2604_plus =
        id == "ubuntu" && matches!((maj, min), (Some(m), Some(n)) if (m, n) >= (26, 4));
    if is_ubuntu_2604_plus {
        "wayland".to_string()
    } else {
        "xorg".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_backend;

    #[test]
    fn explicit_passes_through() {
        assert_eq!(resolve_backend("xorg"), "xorg");
        assert_eq!(resolve_backend("wayland"), "wayland");
        assert_eq!(resolve_backend("garbage"), "garbage");
    }

    #[test]
    fn auto_resolves_to_known_value() {
        let r = resolve_backend("auto");
        assert!(r == "xorg" || r == "wayland");
    }
}
