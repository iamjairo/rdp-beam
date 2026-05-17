/**
 * Electron main process for Beam desktop.
 *
 * Owns the BrowserWindow that hosts the existing `web/dist` bundle, plus
 * the native capabilities that browser tabs can't provide:
 *  - `beam://` URL handler (single-instance, route into the running window)
 *  - System tray (quick-connect, quit)
 *  - Auto-update via electron-updater (GitHub releases)
 *
 * Renderer-side capabilities (clipboard, credentials, deep-link callbacks)
 * are exposed via `preload.ts` using contextBridge — the renderer never has
 * direct access to Node APIs.
 */

import { join } from 'node:path';

import { app, BrowserWindow, ipcMain, shell } from 'electron';
import { autoUpdater } from 'electron-updater';

import { initTray } from './tray';

const PROTOCOL = 'beam';

// Single-instance lock: a second `beam://...` click should route to the
// existing window, not spawn a duplicate process. The protocol handler in
// macOS arrives as an `open-url` event; on Linux/Windows it lands in argv
// on the second instance.
const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
}

// Register `beam://` so the OS routes external clicks to us. Must happen
// before app.whenReady() so the registration is in place even if the user
// hasn't launched us yet.
if (process.defaultApp && process.argv.length >= 2) {
  app.setAsDefaultProtocolClient(PROTOCOL, process.execPath, [
    join(__dirname, '..'),
  ]);
} else {
  app.setAsDefaultProtocolClient(PROTOCOL);
}

let mainWindow: BrowserWindow | null = null;
const deepLinkSubscribers: Set<(url: string) => void> = new Set();

function createWindow(): BrowserWindow {
  const win = new BrowserWindow({
    width: 1600,
    height: 900,
    backgroundColor: '#0a0a0a',
    title: 'Beam',
    webPreferences: {
      preload: join(__dirname, 'preload.js'),
      nodeIntegration: false,
      contextIsolation: true,
      sandbox: true,
    },
  });

  // Production: load the bundled web client. The Electron build copies
  // `web/dist/` into the resources tree, so it's a file:// URL.
  win.loadFile(join(__dirname, '..', '..', 'web', 'dist', 'index.html'));

  // Open external links in the system browser, not inside our shell.
  win.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: 'deny' };
  });

  return win;
}

function dispatchDeepLink(url: string) {
  for (const cb of deepLinkSubscribers) {
    try {
      cb(url);
    } catch (e) {
      console.error('deep-link subscriber failed', e);
    }
  }
}

// Window controls exposed to the renderer via the BeamNative bridge.
// Handlers no-op cleanly when mainWindow hasn't been constructed yet.
ipcMain.handle('window:setFullscreen', (_e, value: boolean) => {
  mainWindow?.setFullScreen(value);
});
ipcMain.handle('window:toggleAlwaysOnTop', () => {
  if (mainWindow) {
    mainWindow.setAlwaysOnTop(!mainWindow.isAlwaysOnTop());
  }
});

// Renderer registers a deep-link callback via the preload bridge.
ipcMain.handle('deep-link:subscribe', (event) => {
  const send = (url: string) => event.sender.send('deep-link:open', url);
  deepLinkSubscribers.add(send);
  event.sender.once('destroyed', () => deepLinkSubscribers.delete(send));
});

// Second instance arriving — route the URL (if any) into our window.
app.on('second-instance', (_event, argv) => {
  if (mainWindow) {
    if (mainWindow.isMinimized()) mainWindow.restore();
    mainWindow.focus();
  }
  const url = argv.find((arg) => arg.startsWith(`${PROTOCOL}://`));
  if (url) dispatchDeepLink(url);
});

// macOS deep link arrives as an event, not in argv.
app.on('open-url', (event, url) => {
  event.preventDefault();
  if (mainWindow) {
    if (mainWindow.isMinimized()) mainWindow.restore();
    mainWindow.focus();
  }
  dispatchDeepLink(url);
});

void app.whenReady().then(() => {
  mainWindow = createWindow();
  initTray(mainWindow);

  // Auto-update from GitHub releases. Errors here should not block startup.
  autoUpdater.checkForUpdatesAndNotify().catch((e: unknown) => {
    console.warn('auto-update check failed', e);
  });

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) {
      mainWindow = createWindow();
    }
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});
