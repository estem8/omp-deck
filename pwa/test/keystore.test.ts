import { describe, expect, test } from 'bun:test';
import { IDBFactory } from 'fake-indexeddb';
import { KeyStore, type StoredPairing, pairingId } from '../src/app/keystore.ts';

function pairing(patch: Partial<StoredPairing> = {}): StoredPairing {
  return {
    secret: new Uint8Array(32).fill(7),
    gatewayPublic: new Uint8Array(32).fill(9),
    url: 'wss://relay.example',
    room: new Uint8Array(16).fill(1),
    token: new Uint8Array(32).fill(2),
    device: 'd-0123456789abcdef',
    gateway: 'axel-desktop',
    name: 'Pixel',
    last: 1000,
    pairedUnixMs: 1000,
    ...patch,
  };
}

/** A second desktop: its own gateway key, room and device. */
function laptop(patch: Partial<StoredPairing> = {}): StoredPairing {
  return pairing({
    gatewayPublic: new Uint8Array(32).fill(11),
    room: new Uint8Array(16).fill(3),
    token: new Uint8Array(32).fill(4),
    secret: new Uint8Array(32).fill(8),
    device: 'd-fedcba9876543210',
    gateway: 'laptop',
    pairedUnixMs: 2000,
    ...patch,
  });
}

const DESKTOP = pairingId(pairing().gatewayPublic);
const LAPTOP = pairingId(laptop().gatewayPublic);

/** The database once an open request succeeded (raw API, below the KeyStore). */
function opened(request: IDBOpenDBRequest): Promise<IDBDatabase> {
  const { promise, resolve, reject } = Promise.withResolvers<IDBDatabase>();
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(request.error);
  return promise;
}

function committed(tx: IDBTransaction): Promise<void> {
  const { promise, resolve, reject } = Promise.withResolvers<void>();
  tx.oncomplete = () => resolve();
  tx.onerror = () => reject(tx.error);
  return promise;
}

/** The version 1 layout: database version 1, store `pairing`, one record at `current` (none when `record` is undefined). */
async function openLegacy(factory: IDBFactory, record?: unknown): Promise<void> {
  const request = factory.open('ompdeck', 1);
  request.onupgradeneeded = () => {
    const store = request.result.createObjectStore('pairing');
    if (record !== undefined) store.put(record, 'current');
  };
  (await opened(request)).close();
}

describe('pairingId', () => {
  test('is the lowercase hex of the gateway key', () => {
    expect(pairingId(new Uint8Array(32).fill(0xab))).toBe('ab'.repeat(32));
    expect(DESKTOP).toBe('09'.repeat(32));
  });
});

describe('KeyStore', () => {
  test('starts empty, then round-trips a pairing', async () => {
    const store = await KeyStore.open(new IDBFactory());
    expect(await store.list()).toEqual([]);
    expect(await store.load(DESKTOP)).toBeNull();
    expect(await store.active()).toBeNull();
    expect(await store.save(pairing())).toBe(DESKTOP);
    expect(await store.load(DESKTOP)).toEqual(pairing());
    store.close();
  });

  test('the stored pairing survives reopening the database', async () => {
    const factory = new IDBFactory();
    const first = await KeyStore.open(factory);
    await first.save(pairing({ name: 'Phone A' }));
    await first.setActive(DESKTOP);
    first.close();
    const second = await KeyStore.open(factory);
    expect((await second.load(DESKTOP))?.name).toBe('Phone A');
    expect(await second.active()).toBe(DESKTOP);
    second.close();
  });

  test('two pairings coexist, each under its own id', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(laptop());
    await store.save(pairing());
    // Oldest first.
    expect((await store.list()).map((p) => p.gateway)).toEqual(['axel-desktop', 'laptop']);
    expect((await store.load(LAPTOP))?.gateway).toBe('laptop');
    expect((await store.load(DESKTOP))?.gateway).toBe('axel-desktop');
    store.close();
  });

  test('pairing again with the same desktop replaces only that record', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(pairing());
    await store.save(laptop());
    await store.save(pairing({ secret: new Uint8Array(32).fill(99), name: 'Pixel 2', token: new Uint8Array(32).fill(55) }));
    expect(await store.list()).toHaveLength(2);
    expect((await store.load(DESKTOP))?.secret).toEqual(new Uint8Array(32).fill(99));
    expect((await store.load(DESKTOP))?.name).toBe('Pixel 2');
    expect(await store.load(LAPTOP)).toEqual(laptop());
    store.close();
  });

  test('removing one pairing leaves the other', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(pairing());
    await store.save(laptop());
    await store.remove(DESKTOP);
    expect(await store.load(DESKTOP)).toBeNull();
    expect((await store.list()).map((p) => pairingId(p.gatewayPublic))).toEqual([LAPTOP]);
    store.close();
  });

  test('reserveTimestamp is monotonic and persisted with the key', async () => {
    const factory = new IDBFactory();
    const store = await KeyStore.open(factory);
    await store.save(pairing({ last: 5000 }));
    // The clock went backwards: still strictly after the last one sent.
    expect(await store.reserveTimestamp(DESKTOP, 4000)).toBe(5001);
    expect(await store.reserveTimestamp(DESKTOP, 4000)).toBe(5002);
    expect(await store.reserveTimestamp(DESKTOP, 9000)).toBe(9000);
    store.close();
    const reopened = await KeyStore.open(factory);
    expect((await reopened.load(DESKTOP))?.last).toBe(9000);
    expect((await reopened.load(DESKTOP))?.secret).toEqual(new Uint8Array(32).fill(7));
    reopened.close();
  });

  test('`last` is kept per pairing: one desktop’s handshakes never move another’s', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(pairing({ last: 5000 }));
    await store.save(laptop({ last: 100 }));
    expect(await store.reserveTimestamp(DESKTOP, 1)).toBe(5001);
    expect(await store.reserveTimestamp(DESKTOP, 1)).toBe(5002);
    // The laptop's floor is its own: a clock below the desktop's but above its own is taken as is.
    expect(await store.reserveTimestamp(LAPTOP, 3000)).toBe(3000);
    expect((await store.load(DESKTOP))?.last).toBe(5002);
    expect((await store.load(LAPTOP))?.last).toBe(3000);
    store.close();
  });

  test('reserveTimestamp without that pairing fails (re-pair), even when another is stored', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await expect(store.reserveTimestamp(DESKTOP, 1)).rejects.toThrow();
    await store.save(laptop());
    await expect(store.reserveTimestamp(DESKTOP, 1)).rejects.toThrow();
    store.close();
  });

  test('loaded byte fields are copies', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(pairing());
    const first = await store.load(DESKTOP);
    first?.secret.fill(0);
    expect((await store.load(DESKTOP))?.secret).toEqual(new Uint8Array(32).fill(7));
    store.close();
  });

  test('the active desktop is persisted and clearable; the legacy-push marker starts unset', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.setActive(LAPTOP);
    expect(await store.active()).toBe(LAPTOP);
    await store.setActive(null);
    expect(await store.active()).toBeNull();
    expect(await store.legacyPush()).toBeNull();
    store.close();
  });

  test('a damaged record counts as not paired', async () => {
    const factory = new IDBFactory();
    const store = await KeyStore.open(factory);
    await store.save(pairing());
    store.close();
    // Garbage next to it, written through the raw API.
    const db = await opened(factory.open('ompdeck', 2));
    const tx = db.transaction('pairings', 'readwrite');
    tx.objectStore('pairings').put({ secret: 'nope' }, 'ff'.repeat(32));
    tx.objectStore('pairings').put({ ...pairing({ gatewayPublic: new Uint8Array(32).fill(12) }), secret: new Uint8Array(3) }, '0c'.repeat(32));
    await committed(tx);
    db.close();
    const reopened = await KeyStore.open(factory);
    expect((await reopened.list()).map((p) => pairingId(p.gatewayPublic))).toEqual([DESKTOP]);
    expect(await reopened.load('ff'.repeat(32))).toBeNull();
    await expect(reopened.reserveTimestamp('0c'.repeat(32), 1)).rejects.toThrow();
    reopened.close();
  });
});

describe('scoped store', () => {
  test('is bound to one pairing: load, save, reserveTimestamp and clear touch only that record', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(laptop());
    const scoped = store.scoped(DESKTOP);
    expect(await scoped.load()).toBeNull();
    await scoped.save(pairing({ last: 40 }));
    expect(await scoped.reserveTimestamp(10)).toBe(41);
    expect((await store.load(LAPTOP))?.last).toBe(1000);
    await scoped.clear();
    expect(await scoped.load()).toBeNull();
    expect(await store.load(LAPTOP)).toEqual(laptop());
    store.close();
  });

  test('refuses to save another desktop’s pairing', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await expect(store.scoped(DESKTOP).save(laptop())).rejects.toThrow('another desktop');
    expect(await store.list()).toEqual([]);
    store.close();
  });
});

describe('version 1 migration', () => {
  test('the single pairing becomes the active desktop and keeps its handshake floor', async () => {
    const factory = new IDBFactory();
    await openLegacy(factory, pairing({ last: 7777 }));
    const store = await KeyStore.open(factory);
    expect(await store.active()).toBe(DESKTOP);
    expect(await store.load(DESKTOP)).toEqual(pairing({ last: 7777 }));
    expect((await store.list()).map((p) => pairingId(p.gatewayPublic))).toEqual([DESKTOP]);
    // The floor keeps counting from where version 1 left it.
    expect(await store.reserveTimestamp(DESKTOP, 100)).toBe(7778);
    store.close();
  });

  test('marks the migrated desktop as the owner of the old root push subscription', async () => {
    const factory = new IDBFactory();
    await openLegacy(factory, pairing());
    const store = await KeyStore.open(factory);
    expect(await store.legacyPush()).toBe(DESKTOP);
    await store.clearLegacyPush();
    expect(await store.legacyPush()).toBeNull();
    store.close();
  });

  test('is done once: the old store is gone and reopening changes nothing', async () => {
    const factory = new IDBFactory();
    await openLegacy(factory, pairing());
    const first = await KeyStore.open(factory);
    await first.save(laptop());
    await first.setActive(LAPTOP);
    first.close();
    const second = await KeyStore.open(factory);
    expect(await second.active()).toBe(LAPTOP);
    expect((await second.list()).map((p) => pairingId(p.gatewayPublic))).toEqual([DESKTOP, LAPTOP]);
    second.close();
    const db = await opened(factory.open('ompdeck'));
    expect([...db.objectStoreNames].sort()).toEqual(['meta', 'pairings']);
    db.close();
  });

  test('a damaged or foreign version 1 record counts as not paired', async () => {
    for (const record of [{ foo: 'bar' }, 'nonsense', null, { ...pairing(), secret: new Uint8Array(5) }]) {
      const factory = new IDBFactory();
      await openLegacy(factory, record);
      const store = await KeyStore.open(factory);
      expect(await store.list()).toEqual([]);
      expect(await store.active()).toBeNull();
      expect(await store.legacyPush()).toBeNull();
      store.close();
    }
  });

  test('a version 1 database without any record migrates to an empty store', async () => {
    const factory = new IDBFactory();
    await openLegacy(factory);
    const store = await KeyStore.open(factory);
    expect(await store.list()).toEqual([]);
    store.close();
  });
});
