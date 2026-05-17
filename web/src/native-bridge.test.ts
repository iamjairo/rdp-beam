import { beforeEach, describe, expect, it } from 'vitest';

import { native } from './native-bridge';

// localStorage may not be configured in the default vitest env; install a
// simple in-memory shim if it's missing so the credential roundtrip test
// stays portable.
beforeEach(() => {
  if (typeof globalThis.localStorage === 'undefined') {
    const store = new Map<string, string>();
    Object.defineProperty(globalThis, 'localStorage', {
      configurable: true,
      value: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => store.set(k, v),
        removeItem: (k: string) => store.delete(k),
        clear: () => store.clear(),
      },
    });
  }
});

describe('native bridge fallback', () => {
  it('reports browser platform when no shell is present', () => {
    expect(native.platform).toBe('browser');
  });

  it('exposes a no-op clipboard.writeImage that resolves', async () => {
    await expect(native.clipboard.writeImage(new Blob())).resolves.toBeUndefined();
  });

  it('returns null from clipboard.readImage in the browser fallback', async () => {
    await expect(native.clipboard.readImage()).resolves.toBeNull();
  });

  it('credentials roundtrip via localStorage', async () => {
    await native.credentials.save('h', 'u', 'tok');
    await expect(native.credentials.load('h', 'u')).resolves.toBe('tok');
    await native.credentials.clear('h', 'u');
    await expect(native.credentials.load('h', 'u')).resolves.toBeNull();
  });

  it('deepLink.onOpen returns an unsubscribe function', () => {
    const off = native.deepLink.onOpen(() => {});
    expect(typeof off).toBe('function');
    off();
  });
});
