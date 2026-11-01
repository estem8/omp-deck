// Shared by the smoke page's boot (boot.ts) and the old-app stand-in (holder.ts): the seed the server
// hands out and the pairing as version 1 stored it.

import type { StoredPairing } from '../../src/app/keystore.ts';
import { fromBase64Url } from '../../src/remote/index.ts';

export interface SeedPairing {
  secret: string;
  gatewayPublic: string;
  url: string;
  room: string;
  token: string;
  device: string;
  gateway: string;
  name: string;
}

export interface Seed {
  main: string;
  pairing: SeedPairing;
  second: SeedPairing;
}

function bytes(text: string): Uint8Array {
  const out = fromBase64Url(text);
  if (out === null) throw new Error('bad seed');
  return out;
}

export function stored(p: SeedPairing, now: number): StoredPairing {
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

/**
 * Writes the single pairing as version 1 did: database version 1, store `pairing`, key `current`.
 * Returns the open connection: version 1 kept it for the page's whole life (`seedLegacy` closes it).
 */
export async function openLegacy(record: StoredPairing): Promise<IDBDatabase> {
  const request = indexedDB.open('ompdeck', 1);
  request.onupgradeneeded = () => request.result.createObjectStore('pairing').put(record, 'current');
  return new Promise<IDBDatabase>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

/** Seeds a version 1 database unless one exists (a fresh profile only). */
export async function seedLegacy(record: StoredPairing): Promise<void> {
  if ((await indexedDB.databases()).some((db) => db.name === 'ompdeck')) return;
  (await openLegacy(record)).close();
}

