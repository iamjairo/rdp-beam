/**
 * System tray for the Electron shell.
 *
 * Provides the bare minimum a long-running remote-desktop client needs:
 * show/hide the window and quit. We intentionally don't add a "Connect to
 * last host" item here — that lives in the web client's session manager
 * (multi-host bookmarks, JWT freshness, etc.) and belongs in the renderer.
 */

import { join } from 'node:path';

import { app, BrowserWindow, Menu, nativeImage, Tray } from 'electron';

let tray: Tray | null = null;

export function initTray(window: BrowserWindow): Tray {
  // Use a small placeholder until a real icon ships under build/.
  const iconPath = join(__dirname, '..', 'build', 'tray.png');
  let icon = nativeImage.createFromPath(iconPath);
  if (icon.isEmpty()) {
    // 16x16 fully-transparent fallback so packaging never breaks because
    // the icon file is missing during early development.
    icon = nativeImage.createEmpty();
  }

  tray = new Tray(icon);
  tray.setToolTip('Beam Remote Desktop');

  const menu = Menu.buildFromTemplate([
    {
      label: 'Show Beam',
      click: () => {
        if (window.isMinimized()) window.restore();
        window.show();
        window.focus();
      },
    },
    { type: 'separator' },
    { label: 'Quit', click: () => app.quit() },
  ]);
  tray.setContextMenu(menu);

  tray.on('click', () => {
    if (window.isVisible()) {
      window.hide();
    } else {
      window.show();
      window.focus();
    }
  });

  return tray;
}
