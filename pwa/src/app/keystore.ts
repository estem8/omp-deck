// The device's pairings, kept in IndexedDB (mobile-pwa.md "Keys"), one per desktop: the static
// secret, the gateway key, the relay URL, the room and admission token, and the `last` handshake
// timestamp. Each pairing is one record keyed by its id (the hex of the gateway's static key), so
// `last` is always persisted atomically with the key it belongs to, and it is persisted before the
// handshake that uses it is sent (a lost `last` would lock the device out until its clock passed
// the gateway's floor; mobile-relay.md R5). A small meta store holds which desktop is active.
//
// Version 1 kept the single pairing at `pairing/current`; opening migrates it (see `migrate`).

import { toHex } from '../remote/bytes.ts';
import { nextTimestamp } from '../remote/index.ts';
import type { PairingStore } from './client.ts';

const DB_NAME = 'ompdeck';
const DB_VERSION = 2;
const PAIRINGS = 'pairings';
const META = 'meta';
/** The version 1 store and key; dropped by the migration. */
const LEGACY_STORE = 'pairing';
const LEGACY_KEY = 'current';
/** Meta keys. */
const ACTIVE = 'active';
const LEGACY_PUSH = 'legacy-push';

/** One paired desktop. Byte fields are copies owned by the caller. */
export interface StoredPairing {
  /** The device's X25519 static secret (fresh for every pairing). */
  secret: Uint8Array;
  /** The gateway's static public key, from the invite. */
  gatewayPublic: Uint8Array;
  /** The canonical relay base URL (`wss://host[:port]`). */
  url: string;
  room: Uint8Array;
  /** This device's own relay admission token (from the pairing Welcome). */
  token: Uint8Array;
  /** The registry id the gateway gave this device (`d-…`). */
  device: string;
  /** The gateway's label (its desktop's name). */
  gateway: string;
  /** The name this device announced when it paired. */
  name: string;
  /** The last handshake timestamp sent with this key (unix ms). */
  last: number;
  pairedUnixMs: number;
}

export class KeyStoreError extends Error {
  override name = 'KeyStoreError';
}

/** A pairing's id: the lowercase hex of the gateway's static key (one pairing per desktop). */
export function pairingId(gatewayPublic: Uint8Array): string {
  return toHex(gatewayPublic);
}

function promised<T>(request: IDBRequest<T>): Promise<T> {
  const { promise, resolve, reject } = Promise.withResolvers<T>();
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(new KeyStoreError(request.error?.message ?? 'IndexedDB request failed'));
  return promise;
}

function finished(tx: IDBTransaction): Promise<void> {
  const { promise, resolve, reject } = Promise.withResolvers<void>();
  tx.oncomplete = () => resolve();
  tx.onabort = () => reject(new KeyStoreError(tx.error?.message ?? 'IndexedDB transaction aborted'));
  tx.onerror = () => reject(new KeyStoreError(tx.error?.message ?? 'IndexedDB transaction failed'));
  return promise;
}

function bytes(value: unknown): Uint8Array | null {
  return value instanceof Uint8Array ? Uint8Array.from(value) : null;
}

/** Reads a stored record defensively: a damaged or foreign record counts as "not paired". */
function decode(value: unknown): StoredPairing | null {
  if (typeof value !== 'object' || value === null) return null;
  const record = value as { [key: string]: unknown };
  const secret = bytes(record['secret']);
  const gatewayPublic = bytes(record['gatewayPublic']);
  const room = bytes(record['room']);
  const token = bytes(record['token']);
  const { url, device, gateway, name, last, pairedUnixMs } = record;
  if (
    secret?.length !== 32 ||
    gatewayPublic?.length !== 32 ||
    room?.length !== 16 ||
    token?.length !== 32 ||
    typeof url !== 'string' ||
    typeof device !== 'string' ||
    typeof gateway !== 'string' ||
    typeof name !== 'string' ||
    typeof last !== 'number' ||
    typeof pairedUnixMs !== 'number'
  ) {
    return null;
  }
  return { secret, gatewayPublic, url, room, token, device, gateway, name, last, pairedUnixMs };
}

function encode(pairing: StoredPairing): StoredPairing {
  return {
    ...pairing,
    secret: Uint8Array.from(pairing.secret),
    gatewayPublic: Uint8Array.from(pairing.gatewayPublic),
    room: Uint8Array.from(pairing.room),
    token: Uint8Array.from(pairing.token),
  };
}

/**
 * Moves the version 1 pairing into the version 2 layout inside the upgrade transaction: it becomes
 * the active desktop (no re-pairing after the upgrade), and is remembered as the owner of the
 * browser's old root-scope push subscription (`legacyPush`). A damaged or foreign version 1 record
 * counts as "not paired", as it always did. The old store is deleted either way.
 */
function migrate(db: IDBDatabase, tx: IDBTransaction): void {
  const read = tx.objectStore(LEGACY_STORE).get(LEGACY_KEY);
  read.onsuccess = () => {
    const record = decode(read.result);
    if (record !== null) {
      const id = pairingId(record.gatewayPublic);
      tx.objectStore(PAIRINGS).put(encode(record), id);
      tx.objectStore(META).put(id, ACTIVE);
      tx.objectStore(META).put(id, LEGACY_PUSH);
    }
    db.deleteObjectStore(LEGACY_STORE);
  };
}

export class KeyStore {
  readonly #db: IDBDatabase;

  private constructor(db: IDBDatabase) {
    this.#db = db;
  }

  static async open(factory: IDBFactory = indexedDB): Promise<KeyStore> {
    const request = factory.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = (event) => {
      const db = request.result;
      const tx = request.transaction;
      if (!db.objectStoreNames.contains(PAIRINGS)) db.createObjectStore(PAIRINGS);
      if (!db.objectStoreNames.contains(META)) db.createObjectStore(META);
      if (tx !== null && event.oldVersion >= 1 && db.objectStoreNames.contains(LEGACY_STORE)) migrate(db, tx);
    };
    return new KeyStore(await promised(request));
  }

  /** Every readable pairing, oldest first (a damaged record is skipped: not paired). */
  async list(): Promise<StoredPairing[]> {
    const tx = this.#db.transaction(PAIRINGS, 'readonly');
    const values = await promised(tx.objectStore(PAIRINGS).getAll());
    const found: StoredPairing[] = [];
    for (const value of values) {
      const pairing = decode(value);
      if (pairing !== null) found.push(pairing);
    }
    return found.sort((a, b) => a.pairedUnixMs - b.pairedUnixMs || (pairingId(a.gatewayPublic) < pairingId(b.gatewayPublic) ? -1 : 1));
  }

  /** The pairing `id`, or null (never paired, forgotten, or evicted: re-pair). */
  async load(id: string): Promise<StoredPairing | null> {
    const tx = this.#db.transaction(PAIRINGS, 'readonly');
    return decode(await promised(tx.objectStore(PAIRINGS).get(id)));
  }

  /**
   * Stores a pairing under its own id: pairing again with the same desktop replaces that record
   * (a new pairing always brings a fresh key); another desktop adds one. Returns the id.
   */
  async save(pairing: StoredPairing): Promise<string> {
    const id = pairingId(pairing.gatewayPublic);
    const tx = this.#db.transaction(PAIRINGS, 'readwrite');
    tx.objectStore(PAIRINGS).put(encode(pairing), id);
    await finished(tx);
    return id;
  }

  /**
   * Reserves the next handshake timestamp for pairing `id`'s key: max(now, last + 1), written in the
   * same transaction that read `last`. Resolves only after it is durable, so it is safe to send.
   */
  async reserveTimestamp(id: string, nowMs: number): Promise<number> {
    const tx = this.#db.transaction(PAIRINGS, 'readwrite');
    const store = tx.objectStore(PAIRINGS);
    let ts: number | null = null;
    const read = store.get(id);
    read.onsuccess = () => {
      const current = decode(read.result);
      if (current === null) {
        tx.abort();
        return;
      }
      ts = nextTimestamp(current.last, nowMs);
      store.put(encode({ ...current, last: ts }), id);
    };
    try {
      await finished(tx);
    } catch (error) {
      if (ts === null) throw new KeyStoreError('no pairing is stored');
      throw error;
    }
    if (ts === null) throw new KeyStoreError('no pairing is stored');
    return ts;
  }

  /** Forgets one desktop's pairing on this device (user action only; never because of a close code). */
  async remove(id: string): Promise<void> {
    const tx = this.#db.transaction(PAIRINGS, 'readwrite');
    tx.objectStore(PAIRINGS).delete(id);
    await finished(tx);
  }

  /** The active desktop's id, or null (none chosen yet). It may name a pairing that is gone: see `resolveActive`. */
  async active(): Promise<string | null> {
    return this.#meta(ACTIVE);
  }

  async setActive(id: string | null): Promise<void> {
    await this.#setMeta(ACTIVE, id);
  }

  /**
   * The desktop whose version 1 push subscription may still sit on the app's root service worker
   * registration (set by the migration; cleared once it moved or was dropped).
   */
  async legacyPush(): Promise<string | null> {
    return this.#meta(LEGACY_PUSH);
  }

  async clearLegacyPush(): Promise<void> {
    await this.#setMeta(LEGACY_PUSH, null);
  }

  /** A `PairingStore` bound to pairing `id`: what a `Client` for that desktop talks to. */
  scoped(id: string): PairingStore {
    return {
      load: () => this.load(id),
      save: async (pairing) => {
        if (pairingId(pairing.gatewayPublic) !== id) throw new KeyStoreError('the pairing belongs to another desktop');
        await this.save(pairing);
      },
      reserveTimestamp: (nowMs) => this.reserveTimestamp(id, nowMs),
      clear: () => this.remove(id),
    };
  }

  async #meta(key: string): Promise<string | null> {
    const tx = this.#db.transaction(META, 'readonly');
    const value: unknown = await promised(tx.objectStore(META).get(key));
    return typeof value === 'string' ? value : null;
  }

  async #setMeta(key: string, value: string | null): Promise<void> {
    const tx = this.#db.transaction(META, 'readwrite');
    if (value === null) tx.objectStore(META).delete(key);
    else tx.objectStore(META).put(value, key);
    await finished(tx);
  }

  close(): void {
    this.#db.close();
  }
}
