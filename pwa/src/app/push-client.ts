// The Web Push client (mobile-pwa.md "Web Push"): subscribes this browser with the gateway's VAPID
// key (`Welcome.push_key`) and registers the subscription with the desktop (`push_subscribe`).
//
// - Every desktop has its own gateway key, and a browser holds one subscription per service worker
//   registration, so each desktop's subscription lives on its own registration of `/sw.js` at the
//   scope `/d/<id>/`. The controller works on the active desktop's registration only; the root
//   registration (the app shell) holds no subscription.
// - Version 1 subscribed on the root registration. The migrated desktop moves that subscription to
//   its own registration at its first sync (`legacy*` in `PushEnv`).
// - Permission is only requested from a tap (`enable`); iOS needs the app on the Home Screen first.
// - At every start (each Welcome) an existing subscription is re-sent, and replaced when the
//   gateway's key changed. That is also how a subscription the service worker renewed
//   (`pushsubscriptionchange`) reaches the desktop: the worker cannot speak Noise, so it tells
//   open windows (`push_subscription_changed`) and the app re-syncs; a closed app re-syncs on its
//   next start.

import { useEffect, useMemo, useSyncExternalStore } from 'react';
import { api, fromBase64Url, toBase64Url } from '../remote/index.ts';
import type { Bytes } from '../remote/bytes.ts';
import { LEGACY_PUSH_KEY_URL, PUSH_CHANGED_MESSAGE, PUSH_STATE_CACHE, desktopScope, pushKeyUrl, pushLabelUrl, pushLabels } from '../push.ts';
import { hasS4 } from './client.ts';
import { useClient, useClientState, useDesktops, useDesktopsState } from './ui/hooks.ts';

export type PushSupport =
  | 'supported'
  /** No service worker, Push API or Notification API here. */
  | 'unsupported'
  /** iOS Safari outside the Home Screen app: push needs "Add to Home Screen" first. */
  | 'ios_home_screen';

export type PushPermission = NotificationPermission | 'unsupported';

/** The browser facilities push needs (a seam for tests). */
export interface PushEnv {
  support: PushSupport;
  permission(): PushPermission;
  /** Must be called from a user gesture. */
  requestPermission(): Promise<NotificationPermission>;
  /** This desktop's service worker registration (scope `/d/<id>/`), or null when it has none yet. */
  registration(): Promise<ServiceWorkerRegistration | null>;
  /** This desktop's registration, registered and activated if need be (a worker must be active to subscribe). */
  ensureRegistration(): Promise<ServiceWorkerRegistration>;
  /** Keeps the VAPID key for this registration's `pushsubscriptionchange` resubscription. */
  rememberKey(key: Bytes): Promise<void>;
  /** Version 1's subscription on the root registration, when this desktop is the one that made it; else null. */
  legacySubscription(): Promise<PushSubscription | null>;
  /** The version 1 subscription is gone (moved or dropped): forgets the marker and its kept key. */
  legacyDone(): Promise<void>;
}

/** What push needs from the client. */
export interface PushClientLike {
  request(op: api.Op): Promise<api.Resp>;
  getState(): { welcome: api.Welcome | null };
}

export interface PushState {
  support: PushSupport;
  permission: PushPermission;
  /** This browser holds a push subscription (registered with the desktop by `enable`/sync). */
  subscribed: boolean;
  busy: boolean;
  error: string | null;
}

export interface PushControl extends PushState {
  /** From a tap: asks permission, subscribes, registers with the desktop. */
  enable(): Promise<void>;
  /** Unregisters at the desktop and drops this browser's subscription. */
  disable(): Promise<void>;
  setPrefs(prefs: api.PushPrefs): Promise<void>;
}

/** Whether push can work here, from the user agent, the APIs present and standalone mode. */
export function detectSupport(env: { userAgent: string; maxTouchPoints: number; standalone: boolean; hasServiceWorker: boolean; hasPushManager: boolean; hasNotification: boolean }): PushSupport {
  const ios = /iPhone|iPad|iPod/.test(env.userAgent) || (/Macintosh/.test(env.userAgent) && env.maxTouchPoints > 1);
  if (ios && !env.standalone) return 'ios_home_screen';
  return env.hasServiceWorker && env.hasPushManager && env.hasNotification ? 'supported' : 'unsupported';
}

/** The `push_subscribe` fields of a subscription (`p256dh`/`auth` base64url, unpadded). */
export function subscriptionOp(subscription: Pick<PushSubscription, 'endpoint' | 'getKey'>): Extract<api.Op, { op: 'push_subscribe' }> {
  const key = (name: PushEncryptionKeyName): string => {
    const buffer = subscription.getKey(name);
    if (buffer === null) throw new Error(`the push subscription has no ${name} key`);
    return toBase64Url(new Uint8Array(buffer));
  };
  return { op: 'push_subscribe', endpoint: subscription.endpoint, p256dh: key('p256dh'), auth: key('auth') };
}

/** Whether `subscription` was made with a different VAPID key than `pushKey`. */
export function keyChanged(subscription: Pick<PushSubscription, 'options'>, pushKey: string): boolean {
  const current = subscription.options.applicationServerKey;
  const wanted = fromBase64Url(pushKey);
  if (current === null || wanted === null) return true;
  const bytes = new Uint8Array(current);
  return bytes.length !== wanted.length || bytes.some((value, index) => value !== wanted[index]);
}

function pushKeyOf(welcome: api.Welcome | null): Bytes | null {
  if (!hasS4(welcome) || welcome?.push_key === undefined) return null;
  return fromBase64Url(welcome.push_key);
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Holds the push state for `usePush` and runs its flows. */
export class PushController {
  readonly #client: PushClientLike;
  readonly #env: PushEnv;
  readonly #listeners = new Set<() => void>();
  #state: PushState;

  constructor(client: PushClientLike, env: PushEnv) {
    this.#client = client;
    this.#env = env;
    this.#state = { support: env.support, permission: env.permission(), subscribed: false, busy: false, error: null };
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  readonly getState = (): PushState => this.#state;

  #set(patch: Partial<PushState>): void {
    this.#state = { ...this.#state, ...patch, permission: this.#env.permission() };
    for (const listener of this.#listeners) listener();
  }

  async #subscription(): Promise<PushSubscription | null> {
    const registration = await this.#env.registration();
    return registration === null ? null : registration.pushManager.getSubscription();
  }

  /**
   * At start (after each Welcome) and when the service worker renewed the subscription: re-sends
   * the existing subscription, resubscribing first when the gateway's key changed. Never prompts.
   */
  async sync(): Promise<void> {
    const key = pushKeyOf(this.#client.getState().welcome);
    if (this.#env.support !== 'supported' || key === null) return;
    try {
      let registration = await this.#env.registration();
      let subscription = (await registration?.pushManager.getSubscription()) ?? null;
      if (subscription === null && this.#env.permission() === 'granted' && (await this.#env.legacySubscription()) !== null) {
        // The migrated desktop: its version 1 subscription is on the root registration. Subscribe on
        // its own registration first; the root one is dropped once the desktop has the new endpoint.
        registration = await this.#env.ensureRegistration();
        subscription = await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      }
      this.#set({ subscribed: subscription !== null });
      if (subscription === null) {
        // Nothing of this desktop's is subscribed anywhere (and no version 1 subscription is left
        // to move): the migration marker has done its job.
        if ((await this.#env.legacySubscription()) === null) await this.#env.legacyDone();
        return;
      }
      if (registration === null || this.#env.permission() !== 'granted') return;
      const pushKey = this.#client.getState().welcome?.push_key ?? '';
      if (keyChanged(subscription, pushKey)) {
        await subscription.unsubscribe();
        subscription = await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      }
      await this.#env.rememberKey(key);
      await this.#client.request(subscriptionOp(subscription));
      await this.#retireLegacy();
      this.#set({ subscribed: true, error: null });
    } catch (error) {
      this.#set({ error: `Could not renew notifications: ${message(error)}` });
    }
  }

  /** Drops version 1's root-registration subscription once this desktop has its own (or turned push off). */
  async #retireLegacy(): Promise<void> {
    await (await this.#env.legacySubscription())?.unsubscribe();
    await this.#env.legacyDone();
  }

  async enable(): Promise<void> {
    if (this.#env.support === 'ios_home_screen') {
      this.#set({ error: 'On iPhone and iPad, add OmpDeck to the Home Screen (Share → Add to Home Screen) and open it from there to get notifications.' });
      return;
    }
    if (this.#env.support !== 'supported') {
      this.#set({ error: 'This browser cannot receive notifications.' });
      return;
    }
    // First, synchronously inside the tap: browsers only show the prompt for a user gesture.
    const asked = this.#env.requestPermission();
    this.#set({ busy: true, error: null });
    try {
      const permission = await asked;
      if (permission !== 'granted') {
        this.#set({ busy: false, error: permission === 'denied' ? 'Notifications are blocked for this site; allow them in the browser settings.' : 'Notifications were not allowed.' });
        return;
      }
      const welcome = this.#client.getState().welcome;
      const key = pushKeyOf(welcome);
      if (key === null) {
        this.#set({ busy: false, error: hasS4(welcome) ? 'The desktop has no push key yet.' : 'Notifications need a newer OmpDeck on the desktop.' });
        return;
      }
      const registration = await this.#env.ensureRegistration();
      let subscription = await registration.pushManager.getSubscription();
      if (subscription !== null && keyChanged(subscription, welcome?.push_key ?? '')) {
        await subscription.unsubscribe();
        subscription = null;
      }
      subscription ??= await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      await this.#env.rememberKey(key);
      await this.#client.request(subscriptionOp(subscription));
      await this.#retireLegacy();
      this.#set({ busy: false, subscribed: true });
    } catch (error) {
      this.#set({ busy: false, error: `Could not turn on notifications: ${message(error)}` });
    }
  }

  async disable(): Promise<void> {
    this.#set({ busy: true, error: null });
    try {
      await this.#client.request({ op: 'push_unsubscribe' });
      const subscription = await this.#subscription();
      await subscription?.unsubscribe();
      await this.#retireLegacy();
      this.#set({ busy: false, subscribed: false });
    } catch (error) {
      this.#set({ busy: false, error: `Could not turn off notifications: ${message(error)}` });
    }
  }

  async setPrefs(prefs: api.PushPrefs): Promise<void> {
    this.#set({ busy: true, error: null });
    try {
      await this.#client.request({ op: 'push_prefs', prefs });
      this.#set({ busy: false });
    } catch (error) {
      this.#set({ busy: false, error: `Could not save the notification settings: ${message(error)}` });
    }
  }
}

/** The active desktop, as push sees it. */
export interface PushDesktop {
  /** The desktop's id: names its service worker registration (`/d/<id>/`). */
  id: string;
  /** Whether this desktop is the one whose version 1 subscription may still be on the root registration. */
  isLegacy(): boolean;
  /** Records that the version 1 subscription is gone. */
  legacyDone(): Promise<void>;
}

/** The absolute scope URL of desktop `id`'s registration (what `registration.scope` reads). */
export function desktopScopeUrl(id: string, base: string = location.href): string {
  return new URL(desktopScope(id), base).href;
}

/**
 * The registration whose scope is exactly `scope`, or null. `getRegistration(url)` answers with the
 * registration *covering* the URL, which for a desktop without its own would be the root one.
 */
export async function registrationAt(container: Pick<ServiceWorkerContainer, 'getRegistration'>, scope: string): Promise<ServiceWorkerRegistration | null> {
  const found = await container.getRegistration(scope);
  return found !== undefined && found.scope === scope ? found : null;
}

/** Resolves once `registration` has an active worker (a fresh registration installs first). */
export async function activated(registration: ServiceWorkerRegistration): Promise<ServiceWorkerRegistration> {
  if (registration.active !== null) return registration;
  const worker = registration.installing ?? registration.waiting;
  if (worker === null) throw new Error('the service worker did not install');
  const { promise, resolve, reject } = Promise.withResolvers<ServiceWorkerRegistration>();
  const check = () => {
    if (worker.state === 'activated') resolve(registration);
    else if (worker.state === 'redundant') reject(new Error('the service worker failed to install'));
  };
  worker.addEventListener('statechange', check);
  check();
  return promise.finally(() => worker.removeEventListener('statechange', check));
}

/** The browser's push facilities for one desktop. */
export function browserPushEnv(standalone: boolean, desktop: PushDesktop): PushEnv {
  const hasNotification = typeof Notification !== 'undefined';
  const scope = desktopScopeUrl(desktop.id);
  const rootScope = new URL('/', location.href).href;
  return {
    support: detectSupport({
      userAgent: navigator.userAgent,
      maxTouchPoints: navigator.maxTouchPoints,
      standalone,
      hasServiceWorker: 'serviceWorker' in navigator,
      hasPushManager: typeof PushManager !== 'undefined',
      hasNotification,
    }),
    permission: () => (hasNotification ? Notification.permission : 'unsupported'),
    requestPermission: () => Notification.requestPermission(),
    registration: async () => ('serviceWorker' in navigator ? registrationAt(navigator.serviceWorker, scope) : null),
    ensureRegistration: async () => {
      if (!('serviceWorker' in navigator)) throw new Error('this browser has no service workers');
      return activated(await navigator.serviceWorker.register('/sw.js', { scope: desktopScope(desktop.id) }));
    },
    rememberKey: async (key) => {
      if (typeof caches === 'undefined') return;
      await (await caches.open(PUSH_STATE_CACHE)).put(pushKeyUrl(scope), new Response(key));
    },
    legacySubscription: async () => {
      if (!desktop.isLegacy() || !('serviceWorker' in navigator)) return null;
      return (await (await registrationAt(navigator.serviceWorker, rootScope))?.pushManager.getSubscription()) ?? null;
    },
    legacyDone: async () => {
      if (!desktop.isLegacy()) return;
      if (typeof caches !== 'undefined') await (await caches.open(PUSH_STATE_CACHE)).delete(LEGACY_PUSH_KEY_URL);
      await desktop.legacyDone();
    },
  };
}

/**
 * Drops this browser's push subscription for desktop `id` (forget/unpair): its subscription, its
 * registration, and its kept VAPID key, plus version 1's root-registration subscription when this
 * is the desktop that made it. Every step runs even when an earlier one fails (the kept key is
 * always deleted); the first failure is then rethrown for the caller to report. Other desktops'
 * registrations are never touched.
 */
export async function dropBrowserPush(id: string, legacy = false): Promise<void> {
  const scope = desktopScopeUrl(id);
  let failure: unknown = null;
  const attempt = async (step: () => Promise<unknown>): Promise<void> => {
    try {
      await step();
    } catch (error) {
      failure ??= error;
    }
  };
  if ('serviceWorker' in navigator) {
    const container = navigator.serviceWorker;
    const registration = await registrationAt(container, scope).catch((error: unknown) => {
      failure ??= error;
      return null;
    });
    if (registration !== null) {
      await attempt(async () => (await registration.pushManager.getSubscription())?.unsubscribe());
      await attempt(() => registration.unregister());
    }
    if (legacy) {
      await attempt(async () => (await (await registrationAt(container, new URL('/', location.href).href))?.pushManager.getSubscription())?.unsubscribe());
    }
  }
  if (typeof caches !== 'undefined') {
    await attempt(async () => {
      const cache = await caches.open(PUSH_STATE_CACHE);
      await cache.delete(pushKeyUrl(scope));
      await cache.delete(pushLabelUrl(scope));
      if (legacy) await cache.delete(LEGACY_PUSH_KEY_URL);
    });
  }
  if (failure !== null) throw failure;
}

/** The part of Cache Storage the label sync uses. */
export interface LabelCache {
  put(url: string, response: Response): Promise<void>;
  delete(url: string): Promise<boolean>;
}

/**
 * Keeps the worker's per-desktop label in step with the paired desktops: each one's display label
 * while two or more are paired, nothing (entries deleted) while one is. The worker reads it when a
 * push arrives; it cannot read the pairings itself.
 */
export async function syncPushLabels(cache: LabelCache, desktops: readonly { id: string; gateway: string }[], base: string = location.href): Promise<void> {
  const labels = pushLabels(desktops);
  for (const desktop of desktops) {
    const url = pushLabelUrl(desktopScopeUrl(desktop.id, base));
    const label = labels.get(desktop.id);
    if (label === undefined) await cache.delete(url);
    else await cache.put(url, new Response(label));
  }
}

/**
 * Runs {@link syncPushLabels} for the current desktops and again whenever their set or labels change
 * (pairing added, replaced or forgotten). Failures only mean notifications go unlabelled.
 */
export function watchPushLabels(desktops: { getState(): { desktops: readonly { id: string; gateway: string }[] }; subscribe(listener: () => void): () => void }): () => void {
  if (typeof caches === 'undefined') return () => undefined;
  let last = '';
  const sync = () => {
    const current = desktops.getState().desktops;
    const key = JSON.stringify(current.map((desktop) => [desktop.id, desktop.gateway]));
    if (key === last) return;
    last = key;
    void caches.open(PUSH_STATE_CACHE).then((cache) => syncPushLabels(cache, current)).catch(() => undefined);
  };
  sync();
  return desktops.subscribe(sync);
}

const controllers = new WeakMap<object, PushController>();

function isStandalone(): boolean {
  return window.matchMedia('(display-mode: standalone)').matches || (navigator as Navigator & { standalone?: boolean }).standalone === true;
}

/** The app's push controller for `client` (one per desktop: the client is bound to it). */
export function pushController(client: PushClientLike, desktop: PushDesktop): PushController {
  let controller = controllers.get(client);
  if (controller === undefined) {
    controller = new PushController(client, browserPushEnv(isStandalone(), desktop));
    controllers.set(client, controller);
  }
  return controller;
}

/** The active desktop for push, read live from the desktops (the legacy marker changes). */
function useActivePushDesktop(): PushDesktop {
  const desktops = useDesktops();
  const id = useDesktopsState((state) => state.activeId);
  return useMemo(() => {
    if (id === null) throw new Error('push without an active desktop');
    return {
      id,
      isLegacy: () => desktops.getState().legacyPush === id,
      legacyDone: () => desktops.clearLegacyPush(),
    };
  }, [desktops, id]);
}

/** Push state and actions for the notification settings. */
export function usePush(): PushControl {
  const controller = pushController(useClient(), useActivePushDesktop());
  const state = useSyncExternalStore(controller.subscribe, controller.getState);
  return useMemo(
    () => ({
      ...state,
      enable: () => controller.enable(),
      disable: () => controller.disable(),
      setPrefs: (prefs: api.PushPrefs) => controller.setPrefs(prefs),
    }),
    [state, controller],
  );
}

/**
 * Keeps the active desktop's copy of this browser's subscription current: re-sends it after each Welcome
 * (with a new key when `push_key` changed) and when the service worker renewed it. Mount once.
 */
export function usePushSync(): void {
  const client = useClient();
  const desktop = useActivePushDesktop();
  const welcome = useClientState((state) => state.welcome);
  const controller = pushController(client, desktop);
  useEffect(() => {
    if (welcome !== null) void controller.sync();
  }, [controller, welcome]);
  useEffect(() => {
    const container: ServiceWorkerContainer | undefined = navigator.serviceWorker;
    if (container === undefined) return;
    const onMessage = (event: MessageEvent<unknown>) => {
      const data = event.data;
      if (typeof data !== 'object' || data === null || !('type' in data) || data.type !== PUSH_CHANGED_MESSAGE) return;
      // Another desktop's renewed subscription reaches that desktop at its next start; the root
      // registration (version 1's subscription) has no desktop and is re-synced here.
      const owner = 'desktop' in data && typeof data.desktop === 'string' ? data.desktop : null;
      if (owner === null || owner === desktop.id) void controller.sync();
    };
    container.addEventListener('message', onMessage);
    return () => container.removeEventListener('message', onMessage);
  }, [controller, desktop.id]);
}
