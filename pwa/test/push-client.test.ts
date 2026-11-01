import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { activated, desktopScopeUrl, detectSupport, dropBrowserPush, keyChanged, PushController, type PushEnv, registrationAt, subscriptionOp, syncPushLabels } from '../src/app/push-client.ts';
import { LEGACY_PUSH_KEY_URL, pushKeyUrl, pushLabelUrl } from '../src/push.ts';
import { type api, fromBase64Url, toBase64Url } from '../src/remote/index.ts';

const KEY_A = toBase64Url(Uint8Array.from({ length: 65 }, (_, i) => (i === 0 ? 4 : i)));
const KEY_B = toBase64Url(Uint8Array.from({ length: 65 }, (_, i) => (i === 0 ? 4 : 200 - i)));

function welcome(patch: Partial<api.Welcome> = {}): api.Welcome {
  return {
    protocol: 1,
    gateway_version: '0.10.0',
    epoch: '00112233445566778899aabbccddeeff',
    device: 'd-0123456789abcdef',
    name: 'Pixel',
    permissions: { control: true, manage: true },
    next_seq: 1,
    limits: { max_message: 1 << 20, window_bytes: 1 << 20, window_messages: 32, max_inflight: 32, max_views: 4, max_feeds: 16 },
    features: ['s4'],
    push_key: KEY_A,
    ...patch,
  };
}

class FakeSubscription {
  unsubscribed = false;
  constructor(
    readonly endpoint: string,
    readonly key: string,
  ) {}
  get options(): PushSubscriptionOptions {
    return { userVisibleOnly: true, applicationServerKey: (fromBase64Url(this.key) as Uint8Array<ArrayBuffer>).buffer };
  }
  getKey(name: PushEncryptionKeyName): ArrayBuffer {
    return Uint8Array.from(name === 'p256dh' ? [1, 2, 3] : [9, 8]).buffer;
  }
  async unsubscribe(): Promise<boolean> {
    this.unsubscribed = true;
    return true;
  }
}

class FakeBrowser {
  permission: NotificationPermission = 'default';
  answer: NotificationPermission = 'granted';
  current: FakeSubscription | null = null;
  subscribed: string[] = [];
  remembered: string[] = [];
  asks = 0;
  /** Version 1's subscription on the root registration, when this desktop owns one. */
  legacy: FakeSubscription | null = null;
  legacyDones = 0;
  ensured = 0;
  /** Whether the desktop's own registration exists yet (a tap or the migration creates it). */
  registered = true;
  readonly #registration = {
    pushManager: {
      getSubscription: async () => this.current,
      subscribe: async (options: PushSubscriptionOptionsInit) => {
        const key = toBase64Url(new Uint8Array(options.applicationServerKey as Uint8Array<ArrayBuffer>));
        this.subscribed.push(key);
        this.current = new FakeSubscription(`https://push.example/${this.subscribed.length}`, key);
        return this.current;
      },
    },
  } as unknown as ServiceWorkerRegistration;
  readonly env: PushEnv = {
    support: 'supported',
    permission: () => this.permission,
    requestPermission: async () => {
      this.asks++;
      this.permission = this.answer;
      return this.answer;
    },
    registration: async () => (this.registered ? this.#registration : null),
    ensureRegistration: async () => {
      this.ensured++;
      this.registered = true;
      return this.#registration;
    },
    rememberKey: async (key) => {
      this.remembered.push(toBase64Url(key));
    },
    legacySubscription: async () => this.legacy as unknown as PushSubscription | null,
    legacyDone: async () => {
      this.legacyDones++;
    },
  };
}

class FakeClient {
  readonly sent: api.Op[] = [];
  constructor(public welcome: api.Welcome | null) {}
  getState = () => ({ welcome: this.welcome });
  request = async (op: api.Op): Promise<api.Resp> => {
    this.sent.push(op);
    return { type: 'unit' };
  };
}

describe('support', () => {
  const base = { userAgent: 'Mozilla/5.0 (Linux; Android 15)', maxTouchPoints: 5, standalone: false, hasServiceWorker: true, hasPushManager: true, hasNotification: true };
  test('Android and desktop browsers with the APIs are supported', () => {
    expect(detectSupport(base)).toBe('supported');
  });
  test('iOS outside the Home Screen app needs the Home Screen hint', () => {
    expect(detectSupport({ ...base, userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_4 like Mac OS X)', hasPushManager: false })).toBe('ios_home_screen');
    expect(detectSupport({ ...base, userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', maxTouchPoints: 5 })).toBe('ios_home_screen');
    expect(detectSupport({ ...base, userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_4 like Mac OS X)', standalone: true })).toBe('supported');
  });
  test('missing APIs are unsupported', () => {
    expect(detectSupport({ ...base, hasPushManager: false })).toBe('unsupported');
  });
});

describe('subscription helpers', () => {
  test('push_subscribe carries the endpoint and base64url keys', () => {
    expect(subscriptionOp(new FakeSubscription('https://push.example/x', KEY_A))).toEqual({ op: 'push_subscribe', endpoint: 'https://push.example/x', p256dh: 'AQID', auth: 'CQg' });
  });
  test('keyChanged compares the subscription key with push_key', () => {
    const subscription = new FakeSubscription('e', KEY_A);
    expect(keyChanged(subscription, KEY_A)).toBe(false);
    expect(keyChanged(subscription, KEY_B)).toBe(true);
  });
});

describe('PushController', () => {
  test('enable asks permission, subscribes with push_key, registers it and remembers the key', async () => {
    const browser = new FakeBrowser();
    const client = new FakeClient(welcome());
    const push = new PushController(client, browser.env);
    await push.enable();
    expect(browser.asks).toBe(1);
    expect(browser.subscribed).toEqual([KEY_A]);
    expect(browser.remembered).toEqual([KEY_A]);
    expect(client.sent).toEqual([{ op: 'push_subscribe', endpoint: 'https://push.example/1', p256dh: 'AQID', auth: 'CQg' }]);
    expect(push.getState()).toMatchObject({ subscribed: true, busy: false, error: null, permission: 'granted' });
  });

  test('a denied permission explains and sends nothing', async () => {
    const browser = new FakeBrowser();
    browser.answer = 'denied';
    const client = new FakeClient(welcome());
    const push = new PushController(client, browser.env);
    await push.enable();
    expect(client.sent).toEqual([]);
    expect(push.getState().error).toContain('blocked');
  });

  test('an older desktop or iOS outside the Home Screen never prompts', async () => {
    const browser = new FakeBrowser();
    const old = new PushController(new FakeClient(welcome({ features: [] })), browser.env);
    await old.enable();
    expect(old.getState().error).toContain('newer OmpDeck');
    const ios = new PushController(new FakeClient(welcome()), { ...browser.env, support: 'ios_home_screen' });
    await ios.enable();
    expect(ios.getState().error).toContain('Home Screen');
    expect(browser.asks).toBe(1);
  });

  test('sync at start re-sends an existing subscription without prompting', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    browser.current = new FakeSubscription('https://push.example/old', KEY_A);
    const client = new FakeClient(welcome());
    await new PushController(client, browser.env).sync();
    expect(browser.asks).toBe(0);
    expect(browser.subscribed).toEqual([]);
    expect(client.sent).toEqual([{ op: 'push_subscribe', endpoint: 'https://push.example/old', p256dh: 'AQID', auth: 'CQg' }]);
  });

  test('sync resubscribes when push_key changed', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    const old = new FakeSubscription('https://push.example/old', KEY_A);
    browser.current = old;
    const client = new FakeClient(welcome({ push_key: KEY_B }));
    await new PushController(client, browser.env).sync();
    expect(old.unsubscribed).toBe(true);
    expect(browser.subscribed).toEqual([KEY_B]);
    expect(client.sent).toEqual([{ op: 'push_subscribe', endpoint: 'https://push.example/1', p256dh: 'AQID', auth: 'CQg' }]);
  });

  test('sync does nothing without a subscription, a push key or s4', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    const client = new FakeClient(welcome());
    await new PushController(client, browser.env).sync();
    browser.current = new FakeSubscription('e', KEY_A);
    await new PushController(new FakeClient(welcome({ features: [] })), browser.env).sync();
    const noKey = welcome();
    delete noKey.push_key;
    await new PushController(new FakeClient(noKey), browser.env).sync();
    expect(client.sent).toEqual([]);
  });

  test('disable unregisters at the desktop and drops the browser subscription; prefs are sent', async () => {
    const browser = new FakeBrowser();
    const subscription = new FakeSubscription('e', KEY_A);
    browser.current = subscription;
    const client = new FakeClient(welcome());
    const push = new PushController(client, browser.env);
    await push.disable();
    const prefs = { turn_finished: true, attention: false, ended: true, show_titles: false };
    await push.setPrefs(prefs);
    expect(client.sent).toEqual([{ op: 'push_unsubscribe' }, { op: 'push_prefs', prefs }]);
    expect(subscription.unsubscribed).toBe(true);
    expect(push.getState().subscribed).toBe(false);
  });

  test('enable registers the desktop’s own service worker first (a tap may precede any registration)', async () => {
    const browser = new FakeBrowser();
    browser.registered = false;
    await new PushController(new FakeClient(welcome()), browser.env).enable();
    expect(browser.ensured).toBe(1);
    expect(browser.subscribed).toEqual([KEY_A]);
  });

  test('sync never creates a registration: a desktop that never enabled push stays unregistered', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    browser.registered = false;
    const client = new FakeClient(welcome());
    await new PushController(client, browser.env).sync();
    expect(browser.ensured).toBe(0);
    expect(client.sent).toEqual([]);
  });

  test('sync moves a version 1 root subscription to the desktop’s own registration', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    browser.registered = false;
    const old = new FakeSubscription('https://push.example/root', KEY_A);
    browser.legacy = old;
    const client = new FakeClient(welcome());
    await new PushController(client, browser.env).sync();
    // Subscribed on its own registration with this desktop's key, and the desktop told the new endpoint.
    expect(browser.ensured).toBe(1);
    expect(browser.subscribed).toEqual([KEY_A]);
    expect(browser.remembered).toEqual([KEY_A]);
    expect(client.sent).toEqual([{ op: 'push_subscribe', endpoint: 'https://push.example/1', p256dh: 'AQID', auth: 'CQg' }]);
    // The root one is dropped, and the migration marker cleared.
    expect(old.unsubscribed).toBe(true);
    expect(browser.legacyDones).toBeGreaterThan(0);
  });

  test('a failed subscribe on the new registration leaves the root subscription in place', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    const old = new FakeSubscription('https://push.example/root', KEY_A);
    browser.legacy = old;
    browser.env.ensureRegistration = async () => {
      throw new Error('no service worker');
    };
    browser.registered = false;
    const push = new PushController(new FakeClient(welcome()), browser.env);
    await push.sync();
    expect(old.unsubscribed).toBe(false);
    expect(browser.legacyDones).toBe(0);
    expect(push.getState().error).toContain('no service worker');
  });

  test('without permission the root subscription is left alone and the marker stays', async () => {
    const browser = new FakeBrowser();
    browser.registered = false;
    const old = new FakeSubscription('e', KEY_A);
    browser.legacy = old;
    await new PushController(new FakeClient(welcome()), browser.env).sync();
    expect(old.unsubscribed).toBe(false);
    expect(browser.legacyDones).toBe(0);
    expect(browser.ensured).toBe(0);
  });

  test('with nothing subscribed anywhere the migration marker is cleared', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    await new PushController(new FakeClient(welcome()), browser.env).sync();
    expect(browser.legacyDones).toBe(1);
  });

  test('turning push on or off also retires version 1’s root subscription', async () => {
    const browser = new FakeBrowser();
    const old = new FakeSubscription('e', KEY_B);
    browser.legacy = old;
    const push = new PushController(new FakeClient(welcome()), browser.env);
    await push.enable();
    expect(old.unsubscribed).toBe(true);
    expect(browser.legacyDones).toBe(1);

    const other = new FakeBrowser();
    const stale = new FakeSubscription('e', KEY_B);
    other.legacy = stale;
    await new PushController(new FakeClient(welcome()), other.env).disable();
    expect(stale.unsubscribed).toBe(true);
  });
});

describe('dropBrowserPush', () => {
  const ID = 'ab'.repeat(32);
  const saved = {
    sw: Object.getOwnPropertyDescriptor(navigator, 'serviceWorker'),
    caches: Object.getOwnPropertyDescriptor(globalThis, 'caches'),
    location: Object.getOwnPropertyDescriptor(globalThis, 'location'),
  };
  // The page's address is what scopes resolve against; a test runtime has none.
  beforeEach(() => {
    Object.defineProperty(globalThis, 'location', { configurable: true, value: { href: 'https://relay.example/settings' } });
  });
  afterEach(() => {
    if (saved.sw) Object.defineProperty(navigator, 'serviceWorker', saved.sw);
    else Reflect.deleteProperty(navigator, 'serviceWorker');
    if (saved.caches) Object.defineProperty(globalThis, 'caches', saved.caches);
    else Reflect.deleteProperty(globalThis, 'caches');
    if (saved.location) Object.defineProperty(globalThis, 'location', saved.location);
    else Reflect.deleteProperty(globalThis, 'location');
  });

  interface Installed {
    deleted: string[];
    unsubscribed: string[];
    unregistered: string[];
  }

  /** A browser with a registration at each scope in `scopes` (each with a subscription), keyed by exact scope. */
  function install(scopes: string[], unsubscribe: (scope: string) => Promise<boolean> = async () => true): Installed {
    const seen: Installed = { deleted: [], unsubscribed: [], unregistered: [] };
    const registrations = new Map(
      scopes.map((scope) => [
        scope,
        {
          scope,
          pushManager: {
            getSubscription: async () => ({
              unsubscribe: async () => {
                seen.unsubscribed.push(scope);
                return unsubscribe(scope);
              },
            }),
          },
          unregister: async () => {
            seen.unregistered.push(scope);
            return true;
          },
        },
      ]),
    );
    Object.defineProperty(navigator, 'serviceWorker', {
      configurable: true,
      // Like the browser: the registration *covering* the URL, so a missing /d/<id>/ answers with the root one.
      value: {
        getRegistration: async (url: string) => registrations.get(url) ?? [...registrations.values()].find((r) => url.startsWith(r.scope) && r.scope === new URL('/', url).href),
      },
    });
    Object.defineProperty(globalThis, 'caches', {
      configurable: true,
      value: { open: async (name: string) => ({ delete: async (url: string) => seen.deleted.push(`${name}:${url}`) > 0 }) },
    });
    return seen;
  }

  const rootScope = () => new URL('/', location.href).href;

  test('unsubscribes and unregisters the desktop’s own registration and deletes its kept key', async () => {
    const seen = install([desktopScopeUrl(ID), rootScope()]);
    await dropBrowserPush(ID);
    expect(seen.unsubscribed).toEqual([desktopScopeUrl(ID)]);
    expect(seen.unregistered).toEqual([desktopScopeUrl(ID)]);
    expect(seen.deleted).toHaveLength(2);
    expect(seen.deleted[0]).toContain(pushKeyUrl(desktopScopeUrl(ID)));
    expect(seen.deleted[1]).toContain(pushLabelUrl(desktopScopeUrl(ID)));
  });

  test('another desktop’s registration and the root one are left alone', async () => {
    const other = desktopScopeUrl('cd'.repeat(32));
    const seen = install([desktopScopeUrl(ID), other, rootScope()]);
    await dropBrowserPush(ID);
    expect(seen.unsubscribed).not.toContain(other);
    expect(seen.unregistered).not.toContain(other);
    expect(seen.unsubscribed).not.toContain(rootScope());
    expect(seen.unregistered).not.toContain(rootScope());
  });

  test('a desktop that never registered touches nothing but its (absent) key entry; the root registration is not mistaken for it', async () => {
    const seen = install([rootScope()]);
    await dropBrowserPush(ID);
    expect(seen.unsubscribed).toEqual([]);
    expect(seen.unregistered).toEqual([]);
  });

  test('the migrated desktop also drops the root subscription and version 1’s key entry', async () => {
    const seen = install([desktopScopeUrl(ID), rootScope()]);
    await dropBrowserPush(ID, true);
    expect(seen.unsubscribed.sort()).toEqual([desktopScopeUrl(ID), rootScope()].sort());
    expect(seen.unregistered).toEqual([desktopScopeUrl(ID)]);
    expect(seen.deleted.join(' ')).toContain(LEGACY_PUSH_KEY_URL);
  });

  test('deletes the kept key and unregisters even when unsubscribe throws, and reports the failure', async () => {
    const seen = install([desktopScopeUrl(ID)], async () => {
      throw new Error('push service unreachable');
    });
    await expect(dropBrowserPush(ID)).rejects.toThrow('push service unreachable');
    expect(seen.unregistered).toEqual([desktopScopeUrl(ID)]);
    expect(seen.deleted).toHaveLength(2);
  });
});

describe('syncPushLabels', () => {
  const A = 'ab'.repeat(32);
  const B = 'cd'.repeat(32);
  const BASE = 'https://relay.example/settings';

  /** A cache that records what it holds, by entry URL → label. */
  function fakeCache() {
    const held = new Map<string, string>();
    return {
      held,
      cache: {
        put: async (url: string, response: Response) => {
          held.set(url, await response.text());
        },
        delete: async (url: string) => held.delete(url),
      },
    };
  }

  test('several desktops: each registration gets its own label', async () => {
    const { held, cache } = fakeCache();
    await syncPushLabels(cache, [{ id: A, gateway: 'Workstation' }, { id: B, gateway: 'Laptop' }], BASE);
    expect(held.get(pushLabelUrl(desktopScopeUrl(A, BASE)))).toBe('Workstation');
    expect(held.get(pushLabelUrl(desktopScopeUrl(B, BASE)))).toBe('Laptop');
  });

  test('back to one desktop: its label is cleared', async () => {
    const { held, cache } = fakeCache();
    await syncPushLabels(cache, [{ id: A, gateway: 'Workstation' }, { id: B, gateway: 'Laptop' }], BASE);
    await syncPushLabels(cache, [{ id: B, gateway: 'Laptop' }], BASE);
    expect(held.has(pushLabelUrl(desktopScopeUrl(B, BASE)))).toBe(false);
  });

  test('a label that changes (collision suffix appears) replaces the old one', async () => {
    const { held, cache } = fakeCache();
    await syncPushLabels(cache, [{ id: A, gateway: 'host' }, { id: B, gateway: 'other' }], BASE);
    await syncPushLabels(cache, [{ id: A, gateway: 'host · 1a2b' }, { id: B, gateway: 'host · 3c4d' }], BASE);
    expect(held.get(pushLabelUrl(desktopScopeUrl(A, BASE)))).toBe('host · 1a2b');
    expect(held.get(pushLabelUrl(desktopScopeUrl(B, BASE)))).toBe('host · 3c4d');
  });

  test('one desktop alone never gets a label', async () => {
    const { held, cache } = fakeCache();
    await syncPushLabels(cache, [{ id: A, gateway: 'Workstation' }], BASE);
    expect(held.size).toBe(0);
  });
});

describe('registrations', () => {
  const root = { scope: 'https://relay.example/' } as ServiceWorkerRegistration;
  const own = { scope: 'https://relay.example/d/x/' } as ServiceWorkerRegistration;

  test('registrationAt matches the exact scope, not the registration that merely covers it', async () => {
    const container = { getRegistration: async (url: string) => (url === own.scope ? own : root) };
    expect(await registrationAt(container, own.scope)).toBe(own);
    expect(await registrationAt(container, 'https://relay.example/d/y/')).toBeNull();
    expect(await registrationAt({ getRegistration: async () => undefined }, own.scope)).toBeNull();
  });

  test('activated returns an active registration at once and otherwise waits for its worker', async () => {
    const active = { active: {} } as ServiceWorkerRegistration;
    expect(await activated(active)).toBe(active);

    const listeners: (() => void)[] = [];
    const worker = {
      state: 'installing',
      addEventListener: (_: string, fn: () => void) => listeners.push(fn),
      removeEventListener: () => undefined,
    };
    const fresh = { active: null, installing: worker, waiting: null } as unknown as ServiceWorkerRegistration;
    const done = activated(fresh);
    worker.state = 'activated';
    for (const fn of listeners) fn();
    expect(await done).toBe(fresh);
  });

  test('activated fails when the worker becomes redundant or never installs', async () => {
    const worker = { state: 'redundant', addEventListener: () => undefined, removeEventListener: () => undefined };
    await expect(activated({ active: null, installing: worker, waiting: null } as unknown as ServiceWorkerRegistration)).rejects.toThrow('failed to install');
    await expect(activated({ active: null, installing: null, waiting: null } as unknown as ServiceWorkerRegistration)).rejects.toThrow('did not install');
  });
});
