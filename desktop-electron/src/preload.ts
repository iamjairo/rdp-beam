/**
 * Electron preload script.
 *
 * Runs in an isolated context with access to Node and Electron internals,
 * and exposes a *narrow* surface to the renderer via `contextBridge`. The
 * shape it exposes must match the `BeamNative` interface declared in
 * `web/src/native-bridge.ts` — drift between the two is a TypeScript error
 * at web-build time because both shells use the same `BeamNative` type.
 *
 * Why every function here is wrapped: contextBridge requires plain values
 * (no class instances, no functions returning non-cloneable objects) so we
 * marshal everything through simple async wrappers.
 */

import { clipboard, contextBridge, ipcRenderer, nativeImage } from 'electron';
// `keytar` is a native module — only available in the main process tree.
// We call it directly here because the preload runs in the main process
// renderer-helper context, which has Node access.
import keytar from 'keytar';

const SERVICE = 'com.beam.desktop';

contextBridge.exposeInMainWorld('__BEAM_NATIVE__', {
  platform: 'electron' as const,

  clipboard: {
    async readText(): Promise<string> {
      return clipboard.readText();
    },
    async writeText(s: string): Promise<void> {
      clipboard.writeText(s);
    },
    async readImage(): Promise<Blob | null> {
      const img = clipboard.readImage();
      if (img.isEmpty()) return null;
      const buf = img.toPNG();
      return new Blob([buf], { type: 'image/png' });
    },
    async writeImage(b: Blob): Promise<void> {
      const arr = new Uint8Array(await b.arrayBuffer());
      const img = nativeImage.createFromBuffer(Buffer.from(arr));
      clipboard.writeImage(img);
    },
  },

  credentials: {
    async save(host: string, user: string, token: string): Promise<void> {
      await keytar.setPassword(SERVICE, `${host}|${user}`, token);
    },
    async load(host: string, user: string): Promise<string | null> {
      return keytar.getPassword(SERVICE, `${host}|${user}`);
    },
    async clear(host: string, user: string): Promise<void> {
      await keytar.deletePassword(SERVICE, `${host}|${user}`);
    },
  },

  deepLink: {
    onOpen(cb: (payload: { url: URL }) => void): () => void {
      const handler = (_e: unknown, raw: string) => {
        try {
          cb({ url: new URL(raw) });
        } catch (err) {
          console.error('invalid deep link url', raw, err);
        }
      };
      ipcRenderer.on('deep-link:open', handler);
      void ipcRenderer.invoke('deep-link:subscribe');
      return () => ipcRenderer.off('deep-link:open', handler);
    },
  },

  window: {
    async setFullscreen(value: boolean): Promise<void> {
      await ipcRenderer.invoke('window:setFullscreen', value);
    },
    async toggleAlwaysOnTop(): Promise<void> {
      await ipcRenderer.invoke('window:toggleAlwaysOnTop');
    },
  },
});
