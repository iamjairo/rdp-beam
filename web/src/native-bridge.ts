/**
 * Native shell bridge contract.
 *
 * Both the Tauri (`desktop-tauri/`) and Electron (`desktop-electron/`) shells
 * inject a `window.__BEAM_NATIVE__` object that implements this interface.
 * The web client checks `native?.platform` and uses native capabilities
 * (OS clipboard, OS keyring, deep links, multi-window) when available,
 * falling back to browser APIs otherwise.
 *
 * Keeping the contract in `web/` means a single TypeScript file defines what
 * both shells must implement. Drift between the two shells is a TypeScript
 * compile error, not a runtime surprise.
 *
 * Important: the interface is intentionally narrow. Anything that *can* be
 * done with browser APIs should be — the native shell is for things browsers
 * can't do at all (OS keyring, real protocol handlers, image-copying to
 * external apps, true keyboard passthrough). Resist the urge to add a
 * `runShellCommand` here.
 */

export type NativePlatform = 'browser' | 'tauri' | 'electron';

export interface DeepLinkPayload {
  /** The full URL the user clicked, e.g. `beam://host.example.com:8444/session/abc`. */
  readonly url: URL;
}

export interface BeamNative {
  /** Identifies which shell is in use. `browser` is reported by the no-op fallback. */
  readonly platform: NativePlatform;

  /**
   * Native clipboard. The browser fallback writes/reads text via
   * `navigator.clipboard` and silently no-ops on image methods.
   */
  readonly clipboard: {
    readText(): Promise<string>;
    writeText(s: string): Promise<void>;
    /** Returns null in the browser fallback or when the clipboard holds no image. */
    readImage(): Promise<Blob | null>;
    /** No-op in the browser fallback (browsers can't write images cross-origin). */
    writeImage(b: Blob): Promise<void>;
  };

  /**
   * OS keyring for "remember me". Browser fallback uses `localStorage` —
   * acceptable security-wise because the JWT is the only thing stored and it
   * already expires in 24h, but native is preferred when available.
   */
  readonly credentials: {
    save(host: string, user: string, token: string): Promise<void>;
    load(host: string, user: string): Promise<string | null>;
    clear(host: string, user: string): Promise<void>;
  };

  /**
   * `beam://` URL handler. Only fires in the native shells. Browser fallback
   * registers a no-op handler. Returns an unsubscribe function.
   */
  readonly deepLink: {
    onOpen(cb: (payload: DeepLinkPayload) => void): () => void;
  };

  /**
   * Window controls that require native APIs (fullscreen API is enough for
   * the browser, but always-on-top and screen pinning are not).
   */
  readonly window: {
    setFullscreen(value: boolean): Promise<void>;
    toggleAlwaysOnTop(): Promise<void>;
  };
}

/**
 * No-op browser fallback. Used when there is no native shell injecting
 * `__BEAM_NATIVE__`. Keeping a real implementation here (rather than `null`)
 * lets callers do `native.clipboard.readText()` without null-checking every
 * field.
 */
const browserFallback: BeamNative = {
  platform: 'browser',
  clipboard: {
    async readText() {
      return navigator.clipboard?.readText?.() ?? '';
    },
    async writeText(s) {
      await navigator.clipboard?.writeText?.(s);
    },
    async readImage() {
      return null;
    },
    async writeImage() {
      /* not supported in browser */
    },
  },
  credentials: {
    async save(host, user, token) {
      localStorage.setItem(`beam.cred.${host}.${user}`, token);
    },
    async load(host, user) {
      return localStorage.getItem(`beam.cred.${host}.${user}`);
    },
    async clear(host, user) {
      localStorage.removeItem(`beam.cred.${host}.${user}`);
    },
  },
  deepLink: {
    onOpen() {
      return () => {};
    },
  },
  window: {
    async setFullscreen(value) {
      if (value) {
        await document.documentElement.requestFullscreen?.();
      } else if (document.fullscreenElement) {
        await document.exitFullscreen?.();
      }
    },
    async toggleAlwaysOnTop() {
      /* not supported in browser */
    },
  },
};

/**
 * The active bridge. Picks the injected `__BEAM_NATIVE__` if a native shell
 * is hosting us, otherwise falls back to the browser implementation.
 *
 * Native shells must inject their object *before* the bundle's `main.ts`
 * runs — Tauri does this via a preload script, Electron via `preload.ts`
 * + `contextBridge.exposeInMainWorld('__BEAM_NATIVE__', ...)`.
 */
export const native: BeamNative =
  (globalThis as unknown as { __BEAM_NATIVE__?: BeamNative }).__BEAM_NATIVE__ ?? browserFallback;
