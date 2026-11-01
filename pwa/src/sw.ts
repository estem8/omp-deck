/// <reference lib="webworker" />
// The service worker: precaches the hashed app shell (cache-first, navigations fall back to the
// cached index.html), never caches `/v1/*`, shows push notifications, and routes notification
// taps. Updates are user-confirmed: the app posts SKIP_WAITING.
//
// The same script runs in several registrations: the root one (scope `/`) is the app shell; each
// paired desktop has one at `/d/<id>/` that only holds that desktop's push subscription (one
// subscription and one VAPID key per registration). Only the root worker touches the shell caches:
// a desktop's worker updating first must not delete the cache the root worker still serves from.

import {
  GENERIC_PUSH,
  LEGACY_PUSH_KEY_URL,
  PUSH_CHANGED_MESSAGE,
  PUSH_STATE_CACHE,
  desktopIdFromScope,
  notificationFor,
  parsePush,
  pushKeyUrl,
  pushLabelUrl,
  safeNavigate,
} from './push.ts';

declare const self: ServiceWorkerGlobalScope;

/** Injected by build.ts: the app shell's URLs, and a version that changes with them. */
declare const __PRECACHE__: string[];
declare const __VERSION__: string;

const CACHE = `ompdeck-${__VERSION__}`;
const SHELL = '/index.html';
/** The desktop this registration serves pushes for; null for the root (app shell) registration. */
const DESKTOP = desktopIdFromScope(self.registration.scope);
const IS_SHELL = DESKTOP === null;

self.addEventListener('install', (event) => {
  if (!IS_SHELL) return;
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      await cache.addAll(__PRECACHE__);
    })(),
  );
});

self.addEventListener('activate', (event) => {
  if (!IS_SHELL) return;
  event.waitUntil(
    (async () => {
      for (const name of await caches.keys()) {
        if (name.startsWith('ompdeck-') && name !== CACHE) await caches.delete(name);
      }
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('message', (event) => {
  const data: unknown = event.data;
  if (typeof data !== 'object' || data === null || !('type' in data)) return;
  if (data.type === 'SKIP_WAITING') void self.skipWaiting();
  // A window of the new version is waiting for the key store upgrade, which another window still
  // on the old version blocks (it keeps its database connection for its whole life and cannot be
  // told to let go). Reloading those windows runs the new code there, which releases the connection.
  if (data.type === 'RELOAD_OTHERS' && IS_SHELL) {
    const sender = event.source;
    const senderId = sender !== null && 'id' in sender ? sender.id : null;
    event.waitUntil(
      (async () => {
        for (const client of await self.clients.matchAll({ type: 'window' })) {
          if (client.id === senderId) continue;
          // A window that cannot be navigated is left to the user (the sender says so on screen).
          await client.navigate(client.url).catch(() => null);
        }
      })(),
    );
  }
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (!IS_SHELL || request.method !== 'GET') return;
  const url = new URL(request.url);
  // Only this origin's app files: never the relay endpoints or anything cross-origin.
  if (url.origin !== self.location.origin || url.pathname.startsWith('/v1/') || url.pathname === '/healthz') return;
  if (request.mode === 'navigate') {
    event.respondWith(
      (async () => {
        try {
          return await fetch(request);
        } catch {
          const cached = await caches.match(SHELL, { cacheName: CACHE });
          return cached ?? Response.error();
        }
      })(),
    );
    return;
  }
  event.respondWith(
    (async () => {
      const cached = await caches.match(request, { cacheName: CACHE });
      return cached ?? fetch(request);
    })(),
  );
});

/** The label the app keeps for this registration's desktop (only while 2+ desktops are paired); null when unset or unreadable. */
async function desktopLabel(): Promise<string | null> {
  if (DESKTOP === null) return null;
  try {
    const stored = await (await caches.open(PUSH_STATE_CACHE)).match(pushLabelUrl(self.registration.scope));
    return stored === undefined ? null : await stored.text();
  } catch {
    return null;
  }
}

self.addEventListener('push', (event) => {
  // Every push shows a notification (browsers penalise silent pushes); bad JSON gets a generic one.
  const payload = event.data === null ? null : parsePush(event.data.text(), self.location.origin);
  const shown = payload ?? GENERIC_PUSH;
  event.waitUntil(
    (async () => {
      const { title, options } = notificationFor(shown, DESKTOP, await desktopLabel());
      await self.registration.showNotification(title, options);
    })(),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const data: unknown = event.notification.data;
  const navigate = typeof data === 'object' && data !== null && 'navigate' in data ? data.navigate : '/';
  const target = new URL(safeNavigate(navigate, self.location.origin), self.location.origin).href;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
      const open = windows.find((client) => new URL(client.url).origin === self.location.origin);
      if (open !== undefined) {
        await open.focus();
        open.postMessage({ type: 'navigate', url: target });
        return;
      }
      await self.clients.openWindow(target);
    })(),
  );
});

self.addEventListener('pushsubscriptionchange', (event) => {
  // Keep receiving: subscribe again with the VAPID key the app stored when it subscribed (the
  // old subscription's own key when the browser still has it). The worker cannot speak Noise, so
  // the desktop learns the new endpoint from the app: open windows are told at once, and a closed
  // app re-sends its subscription (`push_subscribe`) at its next start.
  const change = event as Event & { oldSubscription?: PushSubscription | null; waitUntil(promise: Promise<unknown>): void };
  change.waitUntil(
    (async () => {
      const cache = await caches.open(PUSH_STATE_CACHE);
      // The root registration's entry is version 1's single key until the app migrates it.
      const stored = (await cache.match(pushKeyUrl(self.registration.scope))) ?? (IS_SHELL ? await cache.match(LEGACY_PUSH_KEY_URL) : undefined);
      const key = change.oldSubscription?.options.applicationServerKey ?? (stored === undefined ? null : await stored.arrayBuffer());
      if (key === null) return;
      await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      for (const client of await self.clients.matchAll({ type: 'window', includeUncontrolled: true })) {
        client.postMessage({ type: PUSH_CHANGED_MESSAGE, desktop: DESKTOP });
      }
    })(),
  );
});
