use serde::{Deserialize, Serialize};

/// Top-level configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeamConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub session: SessionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address
    #[serde(default = "default_bind")]
    pub bind: String,
    /// HTTPS port
    #[serde(default = "default_port")]
    pub port: u16,
    /// Public hostname (e.g., "host.example.com"). When set with a custom
    /// TLS cert, the agent connects via wss://hostname:port instead of
    /// wss://127.0.0.1:port, avoiding TLS hostname mismatch errors.
    pub hostname: Option<String>,
    /// Path to TLS certificate (auto-generated if absent)
    pub tls_cert: Option<String>,
    /// Path to TLS key (auto-generated if absent)
    pub tls_key: Option<String>,
    /// JWT secret (auto-generated if absent)
    pub jwt_secret: Option<String>,
    /// Path to web client static files
    #[serde(default = "default_web_root")]
    pub web_root: String,
    /// Require JWT auth for the /metrics endpoint (default: true)
    #[serde(default = "default_true")]
    pub metrics_require_auth: bool,
    /// Users allowed to access /api/admin/* endpoints (empty = admin panel disabled)
    #[serde(default)]
    pub admin_users: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Target bitrate in kbps
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,
    /// Minimum bitrate in kbps (for adaptive bitrate)
    #[serde(default = "default_min_bitrate")]
    pub min_bitrate: u32,
    /// Maximum bitrate in kbps (for adaptive bitrate)
    #[serde(default = "default_max_bitrate")]
    pub max_bitrate: u32,
    /// Target framerate
    #[serde(default = "default_framerate")]
    pub framerate: u32,
    /// Force a specific encoder: "nvh264enc", "vah264enc", "x264enc"
    pub encoder: Option<String>,
    /// Maximum width (0 = unlimited, default: 3840)
    #[serde(default = "default_max_width")]
    pub max_width: u32,
    /// Maximum height (0 = unlimited, default: 2160)
    #[serde(default = "default_max_height")]
    pub max_height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Enable audio streaming
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Opus bitrate in kbps
    #[serde(default = "default_audio_bitrate")]
    pub bitrate: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Default width
    #[serde(default = "default_width")]
    pub default_width: u32,
    /// Default height
    #[serde(default = "default_height")]
    pub default_height: u32,
    /// Starting X display number
    #[serde(default = "default_display_start")]
    pub display_start: u32,
    /// Maximum concurrent sessions
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    /// Idle timeout in seconds (0 = disabled)
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    /// GPU driver for virtual displays: "auto" (default), "nvidia", "dummy"
    #[serde(default = "default_gpu_driver")]
    pub gpu_driver: String,
    /// Display backend: "auto" (detect from /etc/os-release), "xorg", "wayland".
    /// `auto` picks `wayland` on Ubuntu 26.04+ (and other Wayland-default distros)
    /// and `xorg` everywhere else.
    #[serde(default = "default_backend")]
    pub backend: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            hostname: None,
            tls_cert: None,
            tls_key: None,
            jwt_secret: None,
            web_root: default_web_root(),
            metrics_require_auth: true,
            admin_users: Vec::new(),
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            bitrate: default_bitrate(),
            min_bitrate: default_min_bitrate(),
            max_bitrate: default_max_bitrate(),
            framerate: default_framerate(),
            encoder: None,
            max_width: default_max_width(),
            max_height: default_max_height(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bitrate: default_audio_bitrate(),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            default_width: default_width(),
            default_height: default_height(),
            display_start: default_display_start(),
            max_sessions: default_max_sessions(),
            idle_timeout: default_idle_timeout(),
            gpu_driver: default_gpu_driver(),
            backend: default_backend(),
        }
    }
}

impl BeamConfig {
    /// Validate the configuration, returning a list of issues found.
    ///
    /// Issues are prefixed with "ERROR:" (fatal, server should not start) or
    /// "WARNING:" (advisory, server can start but the config is likely wrong).
    ///
    /// Returns `Ok(())` if no issues, or `Err(issues)` with all found problems.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut issues = Vec::new();

        // --- TLS cert/key ---
        match (&self.server.tls_cert, &self.server.tls_key) {
            (Some(cert), Some(key)) => {
                if !std::path::Path::new(cert).exists() {
                    issues.push(format!(
                        "ERROR: tls_cert '{}' does not exist. \
                         Generate with: openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes",
                        cert
                    ));
                }
                if !std::path::Path::new(key).exists() {
                    issues.push(format!(
                        "ERROR: tls_key '{}' does not exist. \
                         Generate with: openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes",
                        key
                    ));
                }
            }
            (Some(_), None) => {
                issues.push(
                    "WARNING: tls_cert is set but tls_key is not. \
                     Both must be set for custom TLS, or omit both for auto-generated certificates."
                        .to_string(),
                );
            }
            (None, Some(_)) => {
                issues.push(
                    "WARNING: tls_key is set but tls_cert is not. \
                     Both must be set for custom TLS, or omit both for auto-generated certificates."
                        .to_string(),
                );
            }
            (None, None) => {} // Fine — auto-generated
        }

        // --- Port ---
        if self.server.port == 0 {
            issues.push("ERROR: server.port must be between 1 and 65535, got 0.".to_string());
        }

        // --- Video bitrate ---
        if self.video.bitrate > 200_000 {
            issues.push(format!(
                "WARNING: video.bitrate is {} kbps ({} Mbps) — this is unusually high \
                 and may indicate a misconfiguration. Typical values: 20000-100000 kbps.",
                self.video.bitrate,
                self.video.bitrate / 1000
            ));
        }

        // --- Framerate ---
        if self.video.framerate == 0 || self.video.framerate > 240 {
            issues.push(format!(
                "ERROR: video.framerate must be between 1 and 240, got {}.",
                self.video.framerate
            ));
        }

        // --- Max resolution ---
        if self.video.max_width != 0 && self.video.max_width < 320 {
            issues.push(format!(
                "ERROR: video.max_width must be 0 (unlimited) or at least 320, got {}.",
                self.video.max_width
            ));
        }
        if self.video.max_height != 0 && self.video.max_height < 240 {
            issues.push(format!(
                "ERROR: video.max_height must be 0 (unlimited) or at least 240, got {}.",
                self.video.max_height
            ));
        }

        // --- Display start ---
        if self.session.display_start == 0 {
            issues.push(
                "ERROR: session.display_start must be >= 1. \
                 Display :0 is typically the local console."
                    .to_string(),
            );
        }

        // --- GPU driver ---
        let valid_gpu_drivers = ["auto", "nvidia", "dummy"];
        if !valid_gpu_drivers.contains(&self.session.gpu_driver.as_str()) {
            issues.push(format!(
                "WARNING: session.gpu_driver '{}' is not recognized. \
                 Valid values: auto, nvidia, dummy. Defaulting to auto.",
                self.session.gpu_driver
            ));
        }

        // --- Backend ---
        let valid_backends = ["auto", "xorg", "wayland"];
        if !valid_backends.contains(&self.session.backend.as_str()) {
            issues.push(format!(
                "WARNING: session.backend '{}' is not recognized. \
                 Valid values: auto, xorg, wayland. Defaulting to auto.",
                self.session.backend
            ));
        }

        // --- Max sessions ---
        if self.session.max_sessions == 0 {
            issues.push("ERROR: session.max_sessions must be >= 1.".to_string());
        }

        // --- Idle timeout ---
        if self.session.idle_timeout > 0 && self.session.idle_timeout < 60 {
            issues.push(format!(
                "ERROR: session.idle_timeout must be 0 (disabled) or at least 60 seconds, \
                 got {}. Values under 60s will disconnect users too aggressively.",
                self.session.idle_timeout
            ));
        }

        // --- Admin users ---
        for user in &self.server.admin_users {
            let trimmed = user.trim();
            if trimmed != user {
                issues.push(format!(
                    "WARNING: admin_users entry '{}' has leading/trailing whitespace. \
                     This will never match a login username. Did you mean '{}'?",
                    user, trimmed
                ));
            }
            if user.is_empty() {
                issues.push(
                    "WARNING: admin_users contains an empty string. This entry will never match."
                        .to_string(),
                );
            } else if !user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
            {
                issues.push(format!(
                    "WARNING: admin_users entry '{}' contains characters not allowed in usernames \
                     (only a-z, 0-9, _, -, . are valid). This entry will never match.",
                    user
                ));
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }
}

fn default_web_root() -> String {
    "web/dist".to_string()
}
fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8444
}
fn default_bitrate() -> u32 {
    50000
}
fn default_min_bitrate() -> u32 {
    2000
}
fn default_max_bitrate() -> u32 {
    100000
}
fn default_framerate() -> u32 {
    120
}
fn default_max_width() -> u32 {
    3840 // 4K
}
fn default_max_height() -> u32 {
    2160 // 4K
}
fn default_true() -> bool {
    true
}
fn default_audio_bitrate() -> u32 {
    128
}
fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}
fn default_display_start() -> u32 {
    10
}
fn default_max_sessions() -> u32 {
    8
}
fn default_idle_timeout() -> u64 {
    3600 // 1 hour
}
fn default_gpu_driver() -> String {
    "auto".to_string()
}
fn default_backend() -> String {
    "auto".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_from_empty_string() {
        let config: BeamConfig =
            toml::from_str("").expect("empty string should deserialize to default config");

        // Server defaults
        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.server.port, 8444);
        assert!(config.server.hostname.is_none());
        assert!(config.server.tls_cert.is_none());
        assert!(config.server.tls_key.is_none());
        assert!(config.server.jwt_secret.is_none());
        assert_eq!(config.server.web_root, "web/dist");
        assert!(config.server.metrics_require_auth);

        // Video defaults
        assert_eq!(config.video.bitrate, 50000);
        assert_eq!(config.video.min_bitrate, 2000);
        assert_eq!(config.video.max_bitrate, 100000);
        assert_eq!(config.video.framerate, 120);
        assert!(config.video.encoder.is_none());
        assert_eq!(config.video.max_width, 3840);
        assert_eq!(config.video.max_height, 2160);

        // Audio defaults
        assert!(config.audio.enabled);
        assert_eq!(config.audio.bitrate, 128);

        // Session defaults
        assert_eq!(config.session.default_width, 1920);
        assert_eq!(config.session.default_height, 1080);
        assert_eq!(config.session.display_start, 10);
        assert_eq!(config.session.max_sessions, 8);
        assert_eq!(config.session.idle_timeout, 3600);
        assert_eq!(config.session.gpu_driver, "auto");
        assert_eq!(config.session.backend, "auto");
    }

    #[test]
    fn partial_config_only_video_section() {
        let toml_str = r#"
[video]
bitrate = 8000
framerate = 30
"#;
        let config: BeamConfig =
            toml::from_str(toml_str).expect("partial config should deserialize");

        // Video: overridden values
        assert_eq!(config.video.bitrate, 8000);
        assert_eq!(config.video.framerate, 30);
        // Video: remaining fields use defaults
        assert_eq!(config.video.min_bitrate, 2000);
        assert_eq!(config.video.max_bitrate, 100000);
        assert!(config.video.encoder.is_none());
        assert_eq!(config.video.max_width, 3840);
        assert_eq!(config.video.max_height, 2160);

        // Other sections use full defaults
        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.server.port, 8444);
        assert_eq!(config.server.web_root, "web/dist");
        assert!(config.audio.enabled);
        assert_eq!(config.audio.bitrate, 128);
        assert_eq!(config.session.default_width, 1920);
        assert_eq!(config.session.idle_timeout, 3600);
    }

    #[test]
    fn idle_timeout_zero_works() {
        let toml_str = r#"
[session]
idle_timeout = 0
"#;
        let config: BeamConfig =
            toml::from_str(toml_str).expect("idle_timeout=0 should deserialize");
        assert_eq!(config.session.idle_timeout, 0);
        // Other session fields retain defaults
        assert_eq!(config.session.default_width, 1920);
        assert_eq!(config.session.default_height, 1080);
        assert_eq!(config.session.display_start, 10);
        assert_eq!(config.session.max_sessions, 8);
    }

    #[test]
    fn max_width_and_max_height_zero_works() {
        let toml_str = r#"
[video]
max_width = 0
max_height = 0
"#;
        let config: BeamConfig =
            toml::from_str(toml_str).expect("max_width=0, max_height=0 should deserialize");
        assert_eq!(config.video.max_width, 0);
        assert_eq!(config.video.max_height, 0);
        // Other video fields retain defaults
        assert_eq!(config.video.bitrate, 50000);
        assert_eq!(config.video.framerate, 120);
    }

    #[test]
    fn custom_values_override_defaults() {
        let toml_str = r#"
[server]
bind = "127.0.0.1"
port = 9444
hostname = "beam.example.com"
tls_cert = "/etc/beam/cert.pem"
tls_key = "/etc/beam/key.pem"
jwt_secret = "supersecret"
web_root = "/usr/share/beam/web/dist"
metrics_require_auth = false

[video]
bitrate = 10000
min_bitrate = 1000
max_bitrate = 30000
framerate = 120
encoder = "nvh264enc"
max_width = 7680
max_height = 4320

[audio]
enabled = false
bitrate = 256

[session]
default_width = 2560
default_height = 1440
display_start = 20
max_sessions = 16
idle_timeout = 7200
"#;
        let config: BeamConfig =
            toml::from_str(toml_str).expect("full custom config should deserialize");

        // Server
        assert_eq!(config.server.bind, "127.0.0.1");
        assert_eq!(config.server.port, 9444);
        assert_eq!(config.server.hostname.as_deref(), Some("beam.example.com"));
        assert_eq!(
            config.server.tls_cert.as_deref(),
            Some("/etc/beam/cert.pem")
        );
        assert_eq!(config.server.tls_key.as_deref(), Some("/etc/beam/key.pem"));
        assert_eq!(config.server.jwt_secret.as_deref(), Some("supersecret"));
        assert_eq!(config.server.web_root, "/usr/share/beam/web/dist");
        assert!(!config.server.metrics_require_auth);

        // Video
        assert_eq!(config.video.bitrate, 10000);
        assert_eq!(config.video.min_bitrate, 1000);
        assert_eq!(config.video.max_bitrate, 30000);
        assert_eq!(config.video.framerate, 120);
        assert_eq!(config.video.encoder.as_deref(), Some("nvh264enc"));
        assert_eq!(config.video.max_width, 7680);
        assert_eq!(config.video.max_height, 4320);

        // Audio
        assert!(!config.audio.enabled);
        assert_eq!(config.audio.bitrate, 256);

        // Session
        assert_eq!(config.session.default_width, 2560);
        assert_eq!(config.session.default_height, 1440);
        assert_eq!(config.session.display_start, 20);
        assert_eq!(config.session.max_sessions, 16);
        assert_eq!(config.session.idle_timeout, 7200);
    }

    #[test]
    fn default_trait_produces_valid_configs() {
        let from_toml: BeamConfig =
            toml::from_str("").expect("empty string should deserialize to default config");

        let server = ServerConfig::default();
        assert_eq!(server.bind, from_toml.server.bind);
        assert_eq!(server.port, from_toml.server.port);
        assert_eq!(server.hostname, from_toml.server.hostname);
        assert_eq!(server.tls_cert, from_toml.server.tls_cert);
        assert_eq!(server.tls_key, from_toml.server.tls_key);
        assert_eq!(server.jwt_secret, from_toml.server.jwt_secret);
        assert_eq!(server.web_root, from_toml.server.web_root);
        assert_eq!(
            server.metrics_require_auth,
            from_toml.server.metrics_require_auth
        );

        let video = VideoConfig::default();
        assert_eq!(video.bitrate, from_toml.video.bitrate);
        assert_eq!(video.min_bitrate, from_toml.video.min_bitrate);
        assert_eq!(video.max_bitrate, from_toml.video.max_bitrate);
        assert_eq!(video.framerate, from_toml.video.framerate);
        assert_eq!(video.encoder, from_toml.video.encoder);
        assert_eq!(video.max_width, from_toml.video.max_width);
        assert_eq!(video.max_height, from_toml.video.max_height);

        let audio = AudioConfig::default();
        assert_eq!(audio.enabled, from_toml.audio.enabled);
        assert_eq!(audio.bitrate, from_toml.audio.bitrate);

        let session = SessionConfig::default();
        assert_eq!(session.default_width, from_toml.session.default_width);
        assert_eq!(session.default_height, from_toml.session.default_height);
        assert_eq!(session.display_start, from_toml.session.display_start);
        assert_eq!(session.max_sessions, from_toml.session.max_sessions);
        assert_eq!(session.idle_timeout, from_toml.session.idle_timeout);
        assert_eq!(session.gpu_driver, from_toml.session.gpu_driver);
        assert_eq!(session.backend, from_toml.session.backend);
    }

    // --- Validation tests ---

    /// Helper: create a default config that passes validation, then mutate it.
    fn valid_config() -> BeamConfig {
        toml::from_str("").expect("default config")
    }

    /// Helper: collect issues from validation.
    fn validate_issues(config: &BeamConfig) -> Vec<String> {
        match config.validate() {
            Ok(()) => vec![],
            Err(issues) => issues,
        }
    }

    /// Helper: check if issues contain an ERROR with the given substring.
    fn has_error(issues: &[String], substring: &str) -> bool {
        issues
            .iter()
            .any(|i| i.starts_with("ERROR:") && i.contains(substring))
    }

    /// Helper: check if issues contain a WARNING with the given substring.
    fn has_warning(issues: &[String], substring: &str) -> bool {
        issues
            .iter()
            .any(|i| i.starts_with("WARNING:") && i.contains(substring))
    }

    #[test]
    fn validate_default_config_passes() {
        let config = valid_config();
        assert!(config.validate().is_ok(), "default config should validate");
    }

    #[test]
    fn validate_port_zero_is_error() {
        let mut config = valid_config();
        config.server.port = 0;
        let issues = validate_issues(&config);
        assert!(has_error(&issues, "port"), "port=0 should produce error");
    }

    #[test]
    fn validate_port_one_is_ok() {
        let mut config = valid_config();
        config.server.port = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_tls_cert_missing_file_is_error() {
        let mut config = valid_config();
        config.server.tls_cert = Some("/nonexistent/cert.pem".to_string());
        config.server.tls_key = Some("/nonexistent/key.pem".to_string());
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "tls_cert"),
            "missing tls_cert file should produce error"
        );
        assert!(
            has_error(&issues, "tls_key"),
            "missing tls_key file should produce error"
        );
    }

    #[test]
    fn validate_tls_cert_without_key_is_warning() {
        let mut config = valid_config();
        config.server.tls_cert = Some("/some/cert.pem".to_string());
        config.server.tls_key = None;
        let issues = validate_issues(&config);
        assert!(
            has_warning(&issues, "tls_cert is set but tls_key is not"),
            "cert without key should warn"
        );
    }

    #[test]
    fn validate_tls_key_without_cert_is_warning() {
        let mut config = valid_config();
        config.server.tls_cert = None;
        config.server.tls_key = Some("/some/key.pem".to_string());
        let issues = validate_issues(&config);
        assert!(
            has_warning(&issues, "tls_key is set but tls_cert is not"),
            "key without cert should warn"
        );
    }

    #[test]
    fn validate_bitrate_over_200k_is_warning() {
        let mut config = valid_config();
        config.video.bitrate = 200_001;
        let issues = validate_issues(&config);
        assert!(
            has_warning(&issues, "bitrate"),
            "bitrate > 200000 should warn"
        );
        assert!(
            !has_error(&issues, "bitrate"),
            "bitrate warning should not be an error"
        );
    }

    #[test]
    fn validate_bitrate_200k_is_ok() {
        let mut config = valid_config();
        config.video.bitrate = 200_000;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_framerate_zero_is_error() {
        let mut config = valid_config();
        config.video.framerate = 0;
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "framerate"),
            "framerate=0 should produce error"
        );
    }

    #[test]
    fn validate_framerate_241_is_error() {
        let mut config = valid_config();
        config.video.framerate = 241;
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "framerate"),
            "framerate=241 should produce error"
        );
    }

    #[test]
    fn validate_framerate_240_is_ok() {
        let mut config = valid_config();
        config.video.framerate = 240;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_framerate_1_is_ok() {
        let mut config = valid_config();
        config.video.framerate = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_max_width_too_small_is_error() {
        let mut config = valid_config();
        config.video.max_width = 319;
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "max_width"),
            "max_width=319 should produce error"
        );
    }

    #[test]
    fn validate_max_width_zero_is_ok() {
        let mut config = valid_config();
        config.video.max_width = 0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_max_width_320_is_ok() {
        let mut config = valid_config();
        config.video.max_width = 320;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_max_height_too_small_is_error() {
        let mut config = valid_config();
        config.video.max_height = 239;
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "max_height"),
            "max_height=239 should produce error"
        );
    }

    #[test]
    fn validate_max_height_zero_is_ok() {
        let mut config = valid_config();
        config.video.max_height = 0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_max_height_240_is_ok() {
        let mut config = valid_config();
        config.video.max_height = 240;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_display_start_zero_is_error() {
        let mut config = valid_config();
        config.session.display_start = 0;
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "display_start"),
            "display_start=0 should produce error"
        );
    }

    #[test]
    fn validate_display_start_one_is_ok() {
        let mut config = valid_config();
        config.session.display_start = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_gpu_driver_invalid_warns() {
        let mut config = valid_config();
        config.session.gpu_driver = "invalid".to_string();
        let issues = validate_issues(&config);
        assert!(
            issues.iter().any(|i| i.contains("gpu_driver")),
            "invalid gpu_driver should produce warning"
        );
    }

    #[test]
    fn validate_gpu_driver_valid_values() {
        for driver in &["auto", "nvidia", "dummy"] {
            let mut config = valid_config();
            config.session.gpu_driver = driver.to_string();
            assert!(
                config.validate().is_ok(),
                "gpu_driver={driver} should be valid"
            );
        }
    }

    #[test]
    fn validate_backend_invalid_warns() {
        let mut config = valid_config();
        config.session.backend = "invalid".to_string();
        let issues = validate_issues(&config);
        assert!(
            issues.iter().any(|i| i.contains("backend")),
            "invalid backend should produce warning"
        );
    }

    #[test]
    fn validate_backend_valid_values() {
        for backend in &["auto", "xorg", "wayland"] {
            let mut config = valid_config();
            config.session.backend = backend.to_string();
            assert!(
                config.validate().is_ok(),
                "backend={backend} should be valid"
            );
        }
    }

    #[test]
    fn validate_max_sessions_zero_is_error() {
        let mut config = valid_config();
        config.session.max_sessions = 0;
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "max_sessions"),
            "max_sessions=0 should produce error"
        );
    }

    #[test]
    fn validate_max_sessions_one_is_ok() {
        let mut config = valid_config();
        config.session.max_sessions = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_idle_timeout_zero_is_ok() {
        let mut config = valid_config();
        config.session.idle_timeout = 0;
        assert!(config.validate().is_ok(), "idle_timeout=0 means disabled");
    }

    #[test]
    fn validate_idle_timeout_59_is_error() {
        let mut config = valid_config();
        config.session.idle_timeout = 59;
        let issues = validate_issues(&config);
        assert!(
            has_error(&issues, "idle_timeout"),
            "idle_timeout=59 should produce error"
        );
    }

    #[test]
    fn validate_idle_timeout_60_is_ok() {
        let mut config = valid_config();
        config.session.idle_timeout = 60;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_multiple_errors_collected() {
        let mut config = valid_config();
        config.server.port = 0;
        config.video.framerate = 0;
        config.session.max_sessions = 0;
        let issues = validate_issues(&config);
        assert!(
            issues.len() >= 3,
            "expected at least 3 errors, got {}: {:?}",
            issues.len(),
            issues
        );
    }

    #[test]
    fn validate_warnings_only_is_err_with_no_errors() {
        let mut config = valid_config();
        config.video.bitrate = 200_001; // warning only
        let issues = validate_issues(&config);
        assert!(!issues.is_empty(), "should have warning");
        assert!(
            !issues.iter().any(|i| i.starts_with("ERROR:")),
            "should only contain warnings, not errors"
        );
    }

    #[test]
    fn validate_admin_users_whitespace_warning() {
        let mut config = valid_config();
        config.server.admin_users = vec!["admin ".to_string()];
        let issues = validate_issues(&config);
        assert!(
            issues.iter().any(|i| i.contains("whitespace")),
            "trailing whitespace should produce warning"
        );
    }

    #[test]
    fn validate_admin_users_invalid_chars_warning() {
        let mut config = valid_config();
        config.server.admin_users = vec!["admin@host".to_string()];
        let issues = validate_issues(&config);
        assert!(
            issues.iter().any(|i| i.contains("not allowed")),
            "invalid chars should produce warning"
        );
    }

    #[test]
    fn validate_admin_users_valid_names_ok() {
        let mut config = valid_config();
        config.server.admin_users = vec![
            "alice".to_string(),
            "bob-1".to_string(),
            "sys_admin.2".to_string(),
        ];
        let issues = validate_issues(&config);
        assert!(
            !issues.iter().any(|i| i.contains("admin_users")),
            "valid admin usernames should not produce warnings"
        );
    }

    #[test]
    fn validate_admin_users_empty_string_warning() {
        let mut config = valid_config();
        config.server.admin_users = vec!["".to_string()];
        let issues = validate_issues(&config);
        assert!(
            issues.iter().any(|i| i.contains("empty string")),
            "empty admin username should produce warning"
        );
    }
}
