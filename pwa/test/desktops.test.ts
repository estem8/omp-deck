import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { IDBFactory } from 'fake-indexeddb';
import { setImmediate } from 'node:timers/promises';
import { Client, type PairingStore } from '../src/app/client.ts';
import { Desktops, inspectInvite, nextActive, resolveActive, summaries } from '../src/app/desktops.ts';
import { KeyStore, type StoredPairing, pairingId } from '../src/app/keystore.ts';
import type { Dialer, DialTarget, PairingResult } from '../src/app/link.ts';
import { pushLabels } from '../src/push.ts';
import { CLOSE, encodePairingInvite } from '../src/remote/index.ts';

// ---- fixtures -----------------------------------------------------------------------------------

function pairing(patch: Partial<StoredPairing> = {}): StoredPairing {
  return {
    secret: new Uint8Array(32).fill(7),
    gatewayPublic: new Uint8Array(32).fill(9),
    url: 'wss://relay.example',
    room: new Uint8Array(16).fill(1),
    token: new Uint8Array(32).fill(2),
    device: 'd-0123456789abcdef',
    gateway: 'workstation',
    name: 'Pixel',
    last: 1000,
    pairedUnixMs: 1000,
    ...patch,
  };
}

function laptop(patch: Partial<StoredPairing> = {}): StoredPairing {
  return pairing({
    gatewayPublic: new Uint8Array(32).fill(11),
    room: new Uint8Array(16).fill(3),
    token: new Uint8Array(32).fill(4),
    device: 'd-fedcba9876543210',
    gateway: 'laptop',
    pairedUnixMs: 2000,
    ...patch,
  });
}

const WORKSTATION = pairingId(pairing().gatewayPublic);
const LAPTOP = pairingId(laptop().gatewayPublic);

/** An invite the fake relay would hand out for `of`'s desktop. */
function inviteFor(of: StoredPairing): string {
  return encodePairingInvite({
    url: of.url,
    room: of.room as Uint8Array<ArrayBuffer>,
    token: new Uint8Array(32).fill(5),
    gatewayPublic: of.gatewayPublic as Uint8Array<ArrayBuffer>,
    psk: new Uint8Array(32).fill(6),
  });
}

interface Dial {
  target: DialTarget;
  closedWith: number[];
}

let dials: Dial[];
let factory: IDBFactory;
let keys: KeyStore;
let desktops: Desktops;
let created: { id: string; client: Client }[];
let pairs: { gateway: string; device: string }[];

const dial: Dialer = (target) => {
  const entry: Dial = { target, closedWith: [] };
  dials.push(entry);
  return { send: () => undefined, close: (code) => entry.closedWith.push(code) };
};

/** Lets the store's IndexedDB work and the client's promise chains run (event-loop turns, no wall-clock wait). */
async function settle(): Promise<void> {
  for (let i = 0; i < 20; i++) await setImmediate();
}

function build(id: string, store: PairingStore): Client {
  const client = new Client({
    store,
    dial,
    // The fake desktop approves at once, under the label queued in `pairs`.
    pair: async (): Promise<PairingResult> => {
      const next = pairs.shift() ?? { gateway: 'new desktop', device: 'd-aaaaaaaaaaaaaaaa' };
      return { token: new Uint8Array(32).fill(9), device: next.device, gateway: next.gateway };
    },
    clock: { now: () => 5_000_000, setTimer: () => 1, clearTimer: () => undefined },
    random: () => 0.5,
    platform: 'Test',
    persistStorage: async () => true,
  });
  created.push({ id, client });
  return client;
}

beforeEach(async () => {
  dials = [];
  created = [];
  pairs = [];
  factory = new IDBFactory();
  keys = await KeyStore.open(factory);
  desktops = new Desktops({ store: keys, createClient: build });
});

// A client started by a test may still be dialling when it ends: let it finish before the next test resets `dials`.
afterEach(settle);

// ---- pure helpers -------------------------------------------------------------------------------

describe('resolveActive and nextActive', () => {
  test('the stored choice wins while it is paired, else the first, else none', () => {
    expect(resolveActive(['a', 'b'], 'b')).toBe('b');
    expect(resolveActive(['a', 'b'], 'gone')).toBe('a');
    expect(resolveActive(['a', 'b'], null)).toBe('a');
    expect(resolveActive([], 'a')).toBeNull();
  });

  test('forgetting one makes the next in list order active, wrapping, or none', () => {
    expect(nextActive(['a', 'b', 'c'], 'a')).toBe('b');
    expect(nextActive(['a', 'b', 'c'], 'b')).toBe('c');
    expect(nextActive(['a', 'b', 'c'], 'c')).toBe('a');
    expect(nextActive(['a'], 'a')).toBeNull();
  });
});

describe('summaries', () => {
  test('desktops with one label stay distinguishable by their key', () => {
    const [a, b] = summaries([pairing({ gateway: 'OmpDeck on host' }), laptop({ gateway: 'OmpDeck on host' })]);
    expect(a?.gateway).toMatch(/^OmpDeck on host · [0-9a-f]{4}$/);
    expect(b?.gateway).toMatch(/^OmpDeck on host · [0-9a-f]{4}$/);
    expect(a?.gateway).not.toBe(b?.gateway);
  });

  test('distinct labels are shown as they are', () => {
    expect(summaries([pairing(), laptop()]).map((d) => d.gateway)).toEqual(['workstation', 'laptop']);
  });
});

describe('notification labels', () => {
  test('one desktop gets none: its notifications stay as they are', () => {
    expect(pushLabels(summaries([pairing()])).size).toBe(0);
    expect(pushLabels(summaries([])).size).toBe(0);
  });

  test('several desktops each get the label the switcher shows', () => {
    const labels = pushLabels(summaries([pairing(), laptop()]));
    expect(labels.get(WORKSTATION)).toBe('workstation');
    expect(labels.get(LAPTOP)).toBe('laptop');
  });

  test('colliding gateway labels carry the key suffix, and drop it when no longer colliding', () => {
    const same = pushLabels(summaries([pairing({ gateway: 'OmpDeck on host' }), laptop({ gateway: 'OmpDeck on host' })]));
    expect(same.get(WORKSTATION)).toMatch(/^OmpDeck on host · [0-9a-f]{4}$/);
    expect(same.get(LAPTOP)).toMatch(/^OmpDeck on host · [0-9a-f]{4}$/);
    expect(same.get(WORKSTATION)).not.toBe(same.get(LAPTOP));
    // The same desktop after the other was forgotten: one desktop, so no label at all.
    expect(pushLabels(summaries([laptop({ gateway: 'OmpDeck on host' })])).size).toBe(0);
  });
});

describe('inspectInvite', () => {
  test('names the desktop by its gateway key', () => {
    const info = inspectInvite(inviteFor(laptop()));
    expect(info).toMatchObject({ ok: true, id: LAPTOP, relay: 'wss://relay.example' });
  });

  test('says why an invite is not valid', () => {
    const info = inspectInvite('v1.nonsense');
    expect(info.ok).toBe(false);
  });
});

// ---- the manager --------------------------------------------------------------------------------

describe('Desktops', () => {
  test('loading with nothing paired shows no client', async () => {
    await desktops.load(null);
    expect(desktops.getState()).toMatchObject({ loaded: true, desktops: [], activeId: null, client: null });
    expect(created).toHaveLength(0);
  });

  test('starts the stored active desktop with a client bound to its own pairing', async () => {
    await keys.save(pairing());
    await keys.save(laptop());
    await keys.setActive(LAPTOP);
    await desktops.load(null);
    await settle();
    const state = desktops.getState();
    expect(state.activeId).toBe(LAPTOP);
    expect(state.desktops.map((d) => d.gateway)).toEqual(['workstation', 'laptop']);
    // One live connection: the active desktop's room.
    expect(dials).toHaveLength(1);
    expect(dials[0]?.target.room).toEqual(new Uint8Array(16).fill(3));
  });

  test('a notification link’s desktop wins over the stored one, and is remembered', async () => {
    await keys.save(pairing());
    await keys.save(laptop());
    await keys.setActive(WORKSTATION);
    await desktops.load(LAPTOP);
    expect(desktops.getState().activeId).toBe(LAPTOP);
    expect(await keys.active()).toBe(LAPTOP);
  });

  test('a stored choice whose pairing is gone falls back to the first desktop', async () => {
    await keys.save(laptop());
    await keys.setActive(WORKSTATION);
    await desktops.load(null);
    expect(desktops.getState().activeId).toBe(LAPTOP);
    expect(await keys.active()).toBe(LAPTOP);
  });

  test('an unknown desktop in a link is ignored', async () => {
    await keys.save(pairing());
    await desktops.load('ee'.repeat(32));
    expect(desktops.getState().activeId).toBe(WORKSTATION);
  });

  test('migrates a version 1 pairing and starts it without re-pairing', async () => {
    const legacy = new IDBFactory();
    const request = legacy.open('ompdeck', 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore('pairing').put(pairing({ last: 4242 }), 'current');
    };
    const { promise, resolve } = Promise.withResolvers<void>();
    request.onsuccess = () => {
      request.result.close();
      resolve();
    };
    await promise;
    const migrated = await KeyStore.open(legacy);
    const manager = new Desktops({ store: migrated, createClient: build });
    await manager.load(null);
    await settle();
    expect(manager.getState()).toMatchObject({ activeId: WORKSTATION, legacyPush: WORKSTATION });
    expect(dials).toHaveLength(1);
    // The handshake floor carried over: the first timestamp is after `last`, not a fresh start.
    expect((await migrated.load(WORKSTATION))?.last).toBeGreaterThanOrEqual(4243);
  });

  test('switching stops the current client (its link closes cleanly) and starts the chosen one', async () => {
    await keys.save(pairing());
    await keys.save(laptop());
    await desktops.load(null);
    await settle();
    const before = desktops.getState().client;
    expect(dials).toHaveLength(1);
    await desktops.switchTo(LAPTOP);
    await settle();
    expect(desktops.getState().activeId).toBe(LAPTOP);
    expect(desktops.getState().client).not.toBe(before);
    expect(dials[0]?.closedWith).toEqual([CLOSE.NORMAL]);
    expect(dials).toHaveLength(2);
    expect(dials[1]?.target.room).toEqual(new Uint8Array(16).fill(3));
    expect(await keys.active()).toBe(LAPTOP);
    // The old client never dials again, even on a wake-up.
    before?.wake();
    before?.reconnectNow();
    await settle();
    expect(dials).toHaveLength(2);
  });

  test('switching to the active desktop or to an unknown one changes nothing', async () => {
    await keys.save(pairing());
    await desktops.load(null);
    await settle();
    const client = desktops.getState().client;
    await desktops.switchTo(WORKSTATION);
    await desktops.switchTo(LAPTOP);
    expect(desktops.getState().client).toBe(client);
    expect(desktops.getState().activeId).toBe(WORKSTATION);
    expect(dials).toHaveLength(1);
  });

  test('forgetting the active desktop leaves the other and makes it active', async () => {
    await keys.save(pairing());
    await keys.save(laptop());
    await desktops.load(null);
    await settle();
    expect(desktops.getState().activeId).toBe(WORKSTATION);
    await desktops.getState().client?.forget();
    await settle();
    const state = desktops.getState();
    expect(state.desktops.map((d) => d.id)).toEqual([LAPTOP]);
    expect(state.activeId).toBe(LAPTOP);
    expect(await keys.active()).toBe(LAPTOP);
    expect(await keys.load(WORKSTATION)).toBeNull();
    // The laptop's client is the one dialling now (its connect reserved a timestamp).
    expect(await keys.load(LAPTOP)).toEqual(laptop({ last: 5_000_000 }));
    expect(dials.at(-1)?.target.room).toEqual(new Uint8Array(16).fill(3));
  });

  test('forgetting the last desktop leaves none: the pairing screen shows', async () => {
    await keys.save(pairing());
    await desktops.load(null);
    await settle();
    await desktops.getState().client?.forget();
    await settle();
    expect(desktops.getState()).toMatchObject({ desktops: [], activeId: null, client: null });
    expect(await keys.active()).toBeNull();
  });

  test('a pairing whose record vanished (evicted) hands over to the next desktop', async () => {
    await keys.save(pairing());
    await keys.save(laptop());
    await desktops.load(null);
    await keys.remove(WORKSTATION);
    // The next connect cannot reserve its timestamp: the key is gone.
    desktops.getState().client?.reconnectNow();
    await settle();
    expect(desktops.getState().activeId).toBe(LAPTOP);
  });

  test('pairing a new desktop adds it next to the others and makes it active', async () => {
    await keys.save(pairing());
    await desktops.load(null);
    await settle();
    const old = desktops.getState().client;
    desktops.beginAdd();
    pairs.push({ gateway: 'laptop', device: 'd-fedcba9876543210' });
    expect(await desktops.pair(inviteFor(laptop()), 'Pixel 9')).toBe(true);
    await settle();
    const state = desktops.getState();
    expect(state.desktops.map((d) => d.gateway)).toEqual(['workstation', 'laptop']);
    expect(state.activeId).toBe(LAPTOP);
    expect(state).toMatchObject({ attempt: null, adding: false });
    expect(state.client).not.toBe(old);
    expect(await keys.active()).toBe(LAPTOP);
    // The workstation is untouched (only its handshake floor moved, when it connected) and its link is closed: one live connection.
    expect(await keys.load(WORKSTATION)).toEqual(pairing({ last: 5_000_000 }));
    expect(dials[0]?.closedWith).toEqual([CLOSE.NORMAL]);
    expect(dials.at(-1)?.target.room).toEqual(new Uint8Array(16).fill(3));
  });

  test('pairing again with a paired desktop replaces only that record', async () => {
    await keys.save(pairing());
    await keys.save(laptop());
    await desktops.load(null);
    await settle();
    pairs.push({ gateway: 'workstation (new)', device: 'd-0000000000000001' });
    expect(await desktops.pair(inviteFor(pairing()), 'Pixel 10')).toBe(true);
    await settle();
    const all = await keys.list();
    expect(all).toHaveLength(2);
    const replaced = await keys.load(WORKSTATION);
    expect(replaced?.name).toBe('Pixel 10');
    expect(replaced?.gateway).toBe('workstation (new)');
    expect(replaced?.secret).not.toEqual(new Uint8Array(32).fill(7));
    expect(await keys.load(LAPTOP)).toEqual(laptop());
    expect(desktops.getState().activeId).toBe(WORKSTATION);
    expect(desktops.getState().desktops.map((d) => d.gateway)).toEqual(['laptop', 'workstation (new)']);
  });

  test('a failed pairing keeps the desktops and the active client; Back returns to the form', async () => {
    await keys.save(pairing());
    await desktops.load(null);
    await settle();
    const active = desktops.getState().client;
    const failing = new Desktops({
      store: keys,
      createClient: (id, store) => {
        const client = new Client({
          store,
          dial,
          pair: () => Promise.reject(new Error('the desktop said no')),
          clock: { now: () => 1, setTimer: () => 1, clearTimer: () => undefined },
          random: () => 0.5,
          platform: 'Test',
          persistStorage: async () => true,
        });
        created.push({ id, client });
        return client;
      },
    });
    await failing.load(null);
    await settle();
    expect(await failing.pair(inviteFor(laptop()), 'Pixel')).toBe(false);
    const attempt = failing.getState().attempt;
    expect(attempt?.client.getState().pairingFlow).toMatchObject({ stage: 'failed', error: 'the desktop said no' });
    expect(failing.getState().activeId).toBe(WORKSTATION);
    expect(await keys.load(LAPTOP)).toBeNull();
    failing.dismissPairing();
    expect(failing.getState().attempt).toBeNull();
    expect(desktops.getState().client).toBe(active);
  });

  test('an invalid invite starts nothing', async () => {
    await keys.save(pairing());
    await desktops.load(null);
    expect(await desktops.pair('v1.garbage', 'Pixel')).toBe(false);
    expect(desktops.getState().attempt).toBeNull();
  });

  test('the legacy-push marker is cleared on request and when its desktop is forgotten', async () => {
    const legacy = new IDBFactory();
    const request = legacy.open('ompdeck', 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore('pairing').put(pairing(), 'current');
    };
    const { promise, resolve } = Promise.withResolvers<void>();
    request.onsuccess = () => {
      request.result.close();
      resolve();
    };
    await promise;
    const migrated = await KeyStore.open(legacy);
    await migrated.save(laptop());
    const manager = new Desktops({ store: migrated, createClient: build });
    await manager.load(null);
    await settle();
    expect(manager.getState().legacyPush).toBe(WORKSTATION);
    await manager.getState().client?.forget();
    await settle();
    expect(manager.getState().legacyPush).toBeNull();
    expect(await migrated.legacyPush()).toBeNull();

    await migrated.save(pairing());
    const again = new Desktops({ store: migrated, createClient: build });
    await again.load(null);
    await again.clearLegacyPush();
    expect(again.getState().legacyPush).toBeNull();
  });
});
