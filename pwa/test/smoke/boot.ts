// Smoke-only boot script (served by server.ts as /__smoke/boot.js, never part of dist/): prepares
// the browser for one smoke route, then loads the real app bundle.
//
// - Seeds the pairing in IndexedDB (the harness's fixed device key), unless the hash asks for the
//   pairing screen (`#route=pair`, or a `#pair=` invite link): then it clears every pairing.
// - `desktops=2` seeds the second desktop's pairing too (the first stays active).
// - `legacy=1` seeds the first desktop the way version 1 stored it (database version 1, the
//   `pairing/current` record), so the app's own upgrade has to migrate it. Only on a fresh profile.
// - `theme=<id>` is stored as the remembered theme.
// - `gw=v1` selects the gateway that announces no `s4` (a cookie the WebSocket upgrade carries).
// The remaining hash (`route=…`, `s=…`, `d=…`, `pair=…`) is left for the app.

import { KeyStore, type StoredPairing, pairingId } from '../../src/app/keystore.ts';
import { isThemeId, saveTheme } from '../../src/app/theme.ts';
import { fromBase64Url } from '../../src/remote/index.ts';

interface SeedPairing {
  secret: string;
  gatewayPublic: string;
  url: string;
  room: string;
  token: string;
  device: string;
  gateway: string;
  name: string;
}

interface Seed {
  main: string;
  pairing: SeedPairing;
  second: SeedPairing;
}

function bytes(text: string): Uint8Array {
  const out = fromBase64Url(text);
  if (out === null) throw new Error('bad seed');
  return out;
}

function stored(p: SeedPairing, now: number): StoredPairing {
  return {
    secret: bytes(p.secret),
    gatewayPublic: bytes(p.gatewayPublic),
    url: p.url,
    room: bytes(p.room),
    token: bytes(p.token),
    device: p.device,
    gateway: p.gateway,
    name: p.name,
    last: now - 1,
    pairedUnixMs: now - 3 * 24 * 3_600_000,
  };
}

/** Writes the single pairing as version 1 did: database version 1, store `pairing`, key `current`. */
async function seedLegacy(record: StoredPairing): Promise<void> {
  if ((await indexedDB.databases()).some((db) => db.name === 'ompdeck')) return;
  const request = indexedDB.open('ompdeck', 1);
  request.onupgradeneeded = () => request.result.createObjectStore('pairing').put(record, 'current');
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  db.close();
}

async function boot(): Promise<void> {
  const params = new URLSearchParams(location.hash.slice(1));
  const seed = (await (await fetch('/__smoke/seed.json')).json()) as Seed;

  const theme = params.get('theme');
  if (isThemeId(theme)) saveTheme(localStorage, theme);
  document.cookie = `smoke_gw=${params.get('gw') === 'v1' ? 'v1' : 's4'}; path=/; SameSite=Strict`;

  const now = Date.now();
  if (params.get('legacy') === '1') {
    await seedLegacy(stored(seed.pairing, now));
  } else {
    const store = await KeyStore.open();
    if (params.get('route') === 'pair' || params.has('pair')) {
      for (const pairing of await store.list()) await store.remove(pairingId(pairing.gatewayPublic));
      await store.setActive(null);
      if (params.get('route') === 'pair') history.replaceState(null, '', location.pathname);
    } else {
      const first = await store.save(stored(seed.pairing, now));
      if (params.get('desktops') === '2') await store.save(stored(seed.second, now));
      await store.setActive(first);
    }
    store.close();
  }
  // The app bundle's content-hashed name is only known to the server at run time.
  await import(seed.main);
}

void boot();
