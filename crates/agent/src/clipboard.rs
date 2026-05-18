use anyhow::Context;
use std::process::Command;
use tracing::debug;

pub struct ClipboardBridge {
    x_display: String,
}

impl ClipboardBridge {
    /// Construct a bridge that silently no-ops all operations. Used when
    /// xclip is unavailable (e.g. on a headless Wayland session without
    /// Xwayland). `get_text` returns `Ok(None)` and `set_text` returns `Ok(())`.
    #[cfg(feature = "wayland")]
    pub fn noop() -> Self {
        Self {
            x_display: String::new(),
        }
    }

    pub fn new(x_display: &str) -> anyhow::Result<Self> {
        // Verify xclip is available
        Command::new("which")
            .arg("xclip")
            .output()
            .context("Failed to check for xclip")?
            .status
            .success()
            .then_some(())
            .context("xclip not found; install it for clipboard support")?;

        debug!(x_display, "Clipboard bridge initialized");
        Ok(Self {
            x_display: x_display.to_string(),
        })
    }

    /// Strip terminal control characters that could execute commands
    /// when pasted into a terminal emulator. Keep \t (0x09), \n (0x0A), \r (0x0D).
    fn sanitize(text: &str) -> String {
        text.chars()
            .filter(|&c| c == '\t' || c == '\n' || c == '\r' || (c >= ' ' && c != '\x7f'))
            .collect()
    }

    /// Write text to an X11 selection via xclip.
    fn set_selection(&self, selection: &str, text: &str) -> anyhow::Result<()> {
        use std::io::Write;

        let sanitized = Self::sanitize(text);

        let mut child = Command::new("xclip")
            .args(["-selection", selection])
            .env("DISPLAY", &self.x_display)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn xclip")?;

        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(sanitized.as_bytes())
                .context("Failed to write to xclip stdin")?;
        }

        let status = child.wait().context("Failed to wait for xclip")?;
        if !status.success() {
            anyhow::bail!("xclip exited with status {status}");
        }

        debug!(len = text.len(), selection, "Clipboard text set");
        Ok(())
    }

    pub fn set_text(&self, text: &str) -> anyhow::Result<()> {
        self.set_selection("clipboard", text)
    }

    /// Write text to the X11 PRIMARY selection (used by middle-click paste).
    pub fn set_primary_text(&self, text: &str) -> anyhow::Result<()> {
        self.set_selection("primary", text)
    }

    pub fn get_text(&self) -> anyhow::Result<Option<String>> {
        let output = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .env("DISPLAY", &self.x_display)
            .output()
            .context("Failed to run xclip -o")?;

        if !output.status.success() {
            // No clipboard content or xclip error - not fatal
            return Ok(None);
        }

        let text =
            String::from_utf8(output.stdout).context("Clipboard content is not valid UTF-8")?;
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize: control character stripping ---

    #[test]
    fn sanitize_strips_escape() {
        assert_eq!(ClipboardBridge::sanitize("hello\x1bworld"), "helloworld");
    }

    #[test]
    fn sanitize_strips_bell() {
        assert_eq!(ClipboardBridge::sanitize("ding\x07dong"), "dingdong");
    }

    #[test]
    fn sanitize_strips_null() {
        assert_eq!(ClipboardBridge::sanitize("a\x00b"), "ab");
    }

    #[test]
    fn sanitize_strips_delete() {
        assert_eq!(ClipboardBridge::sanitize("rm\x7fme"), "rmme");
    }

    #[test]
    fn sanitize_strips_mixed_control_chars() {
        // ESC[31m is a typical ANSI color sequence
        let input = "\x1b[31mred text\x1b[0m";
        assert_eq!(ClipboardBridge::sanitize(input), "[31mred text[0m");
    }

    #[test]
    fn sanitize_strips_all_c0_controls_except_whitespace() {
        // C0 control range is 0x00..=0x1F. Tab (0x09), LF (0x0A), CR (0x0D) are kept.
        let mut input = String::new();
        for c in 0x00u8..=0x1F {
            input.push(c as char);
        }
        let result = ClipboardBridge::sanitize(&input);
        assert_eq!(result, "\t\n\r");
    }

    // --- sanitize: preserves normal text ---

    #[test]
    fn sanitize_preserves_ascii_text() {
        let text = "Hello, World! 123 @#$%^&*()";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_unicode() {
        let text = "Hei verden! \u{1F600} \u{00E6}\u{00F8}\u{00E5}";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_empty_string() {
        assert_eq!(ClipboardBridge::sanitize(""), "");
    }

    #[test]
    fn sanitize_preserves_spaces_and_punctuation() {
        let text = "line one.  line two!  (parens) [brackets] {braces}";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    // --- sanitize: preserves whitespace ---

    #[test]
    fn sanitize_preserves_newlines() {
        let text = "line one\nline two\nline three";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_carriage_returns() {
        let text = "line one\r\nline two\r\n";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_tabs() {
        let text = "col1\tcol2\tcol3";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_mixed_whitespace() {
        let text = "header\n\tcol1\tcol2\r\ndata\n";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    // --- sanitize: realistic clipboard payloads ---

    #[test]
    fn sanitize_handles_terminal_escape_injection() {
        // Simulates a malicious clipboard payload that could run commands
        // if pasted into a terminal: ESC ]0; sets title, BEL terminates
        let malicious = "\x1b]0;pwned\x07\necho hacked";
        assert_eq!(
            ClipboardBridge::sanitize(malicious),
            "]0;pwned\necho hacked"
        );
    }

    #[test]
    fn sanitize_handles_multiline_code_snippet() {
        let code = "fn main() {\n    println!(\"hello\");\n}\n";
        assert_eq!(ClipboardBridge::sanitize(code), code);
    }
}
