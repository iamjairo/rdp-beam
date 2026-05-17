// Hide the Windows console window on release builds. Doesn't affect Linux/macOS.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Tauri main process for Beam.
//!
//! This shell is intentionally thin: it loads the existing `web/dist`
//! bundle as its frontend and provides the OS-level capabilities that
//! browsers can't (deep links, OS keyring, system tray, native clipboard).
//!
//! The renderer-visible API matches the `BeamNative` interface in
//! `web/src/native-bridge.ts`. Each capability is implemented as a Tauri
//! command and re-exposed under `window.__BEAM_NATIVE__` via a small
//! initialization script.
//!
//! Some pieces are deliberately delegated to first-party Tauri plugins
//! rather than reimplemented here:
//!   - clipboard text reads/writes → `tauri-plugin-clipboard-manager`
//!   - deep links → `tauri-plugin-deep-link`
//!   - single-instance enforcement → `tauri-plugin-single-instance`
//!   - auto-update → `tauri-plugin-updater`
//!
//! Anything not yet covered by a plugin (image clipboard, keyring) lives
//! in this crate as a `#[tauri::command]`.

mod credentials;

use tauri::Manager;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // Second instance arrived — bring the existing window forward.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
                let _ = window.show();
            }
            // The deep-link plugin handles routing `beam://...` from argv
            // into the renderer; nothing extra needed here unless a
            // future feature wants to consume other CLI args.
            let _ = argv;
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            credentials::credentials_save,
            credentials::credentials_load,
            credentials::credentials_clear,
        ])
        .setup(|app| {
            // Build the JS shim that adapts the Tauri command surface to
            // the `BeamNative` shape expected by `web/src/native-bridge.ts`.
            // Injected via the standard Tauri init-script mechanism so it
            // runs before any of the app's own JS.
            let window = app
                .get_webview_window("main")
                .expect("main window not found");
            window.eval(BEAM_NATIVE_SHIM)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// JS shim that exposes `__BEAM_NATIVE__` matching the `BeamNative`
/// contract. Kept as a single string literal so changes to the contract
/// (in `web/src/native-bridge.ts`) translate directly into edits here
/// without touching build configuration.
///
/// The renderer never sees `invoke` — only the bridge shape.
const BEAM_NATIVE_SHIM: &str = r#"
(() => {
  const { invoke } = window.__TAURI__.core;
  const cb = window.__TAURI__.clipboardManager;
  const dl = window.__TAURI__.deepLink;

  window.__BEAM_NATIVE__ = {
    platform: 'tauri',
    clipboard: {
      readText: () => cb.readText(),
      writeText: (s) => cb.writeText(s),
      readImage: async () => {
        // tauri-plugin-clipboard-manager v2 doesn't expose image read in
        // every backend yet; gracefully degrade to null on platforms
        // where it isn't available. The browser fallback returns null
        // here too, so callers already handle the null case.
        if (cb.readImage) {
          try { const img = await cb.readImage(); return img?.toBlob?.() ?? null; }
          catch { return null; }
        }
        return null;
      },
      writeImage: async (blob) => {
        if (cb.writeImage) {
          const buf = new Uint8Array(await blob.arrayBuffer());
          await cb.writeImage(buf);
        }
      },
    },
    credentials: {
      save:  (host, user, token) => invoke('credentials_save',  { host, user, token }),
      load:  (host, user)        => invoke('credentials_load',  { host, user }),
      clear: (host, user)        => invoke('credentials_clear', { host, user }),
    },
    deepLink: {
      onOpen: (cb) => {
        const unlistenPromise = dl.onOpenUrl(({ urls }) => {
          for (const raw of urls) {
            try { cb({ url: new URL(raw) }); } catch (e) { console.error(e); }
          }
        });
        return () => { unlistenPromise.then(fn => fn?.()); };
      },
    },
    window: {
      setFullscreen: (v) => window.__TAURI__.window.getCurrentWindow().setFullscreen(v),
      toggleAlwaysOnTop: async () => {
        const w = window.__TAURI__.window.getCurrentWindow();
        const cur = await w.isAlwaysOnTop();
        await w.setAlwaysOnTop(!cur);
      },
    },
  };
})();
"#;
