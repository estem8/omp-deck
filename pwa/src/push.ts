// The gateway's declarative Web Push payload (`{web_push, notification: {...}}`), as the service
// worker shows it, and how pushes are told apart per desktop.
//
// A browser keeps one push subscription per service worker registration, bound to one VAPID key,
// and every desktop's gateway has its own key. So each desktop gets its own registration of the
// same `/sw.js` at the scope `/d/<id>/` (nothing is ever served there: it only holds the
// subscription), and the worker knows which desktop a push is for from `registration.scope`. The
// root registration stays the app-shell worker.

/** Cache Storage where the app keeps the VAPID key it subscribed with (the service worker's activate keeps it). */
export const PUSH_STATE_CACHE = 'push-state';
/** Version 1 kept one key for the one subscription of the root registration; the migration drops it. */
export const LEGACY_PUSH_KEY_URL = '/__push-key';
/** Posted by the service worker to open windows after it renewed the subscription. */
export const PUSH_CHANGED_MESSAGE = 'push_subscription_changed';

const DESKTOP_ID = /^[0-9a-f]{64}$/;

/** Whether `text` is a desktop id (the lowercase hex of a gateway's static key). */
export function isDesktopId(text: unknown): text is string {
  return typeof text === 'string' && DESKTOP_ID.test(text);
}

/** The service worker scope (a path) of desktop `id`'s push registration. */
export function desktopScope(id: string): string {
  return `/d/${id}/`;
}

/** The desktop a registration scope belongs to, or null for the root (app shell) registration or anything else. */
export function desktopIdFromScope(scope: string): string | null {
  let path: string;
  try {
    path = new URL(scope, 'https://app.invalid').pathname;
  } catch {
    return null;
  }
  const id = /^\/d\/([^/]+)\/$/.exec(path)?.[1];
  return isDesktopId(id) ? id : null;
}

/**
 * The key's entry in {@link PUSH_STATE_CACHE} for the registration at `scope` (an absolute scope URL,
 * as `registration.scope` reads): raw key bytes, one entry per registration.
 */
export function pushKeyUrl(scope: string): string {
  return `/__push-key?scope=${encodeURIComponent(scope)}`;
}

/**
 * The desktop's display label in {@link PUSH_STATE_CACHE} for the registration at `scope` (an absolute
 * scope URL): UTF-8 text, present only while two or more desktops are paired. The worker cannot read
 * the pairings (IndexedDB, and no names), so the app keeps this for it.
 */
export function pushLabelUrl(scope: string): string {
  return `/__push-label?scope=${encodeURIComponent(scope)}`;
}

/**
 * The labels the worker puts on each desktop's notifications: desktop id → the label the switcher
 * shows (`labels` are those display labels, collision suffix included). Only while 2+ desktops are
 * paired, so with one desktop the map is empty and its notifications stay as they are.
 */
export function pushLabels(desktops: readonly { id: string; gateway: string }[]): Map<string, string> {
  return desktops.length < 2 ? new Map() : new Map(desktops.map((desktop) => [desktop.id, desktop.gateway]));
}

/**
 * A notification title that names its desktop: `OmpDeck — <label>`; unchanged without a label. The
 * gateway's default label is "OmpDeck on <host>", so its "OmpDeck on " is dropped here: the title
 * already says OmpDeck.
 */
export function titleWithLabel(title: string, label: string | null): string {
  const name = label?.replace(/^OmpDeck on /, '') ?? '';
  return name.length === 0 ? title : `${title} — ${name}`;
}

/** The notification a push shows for desktop `desktop` (null: the root registration) and its label. */
export function notificationFor(shown: PushPayload, desktop: string | null, label: string | null): { title: string; options: NotificationOptions } {
  const options: NotificationOptions = {
    body: shown.body,
    icon: '/icon-192.png',
    // Android draws the status-bar badge from the alpha channel only: a white silhouette.
    badge: '/badge-96.png',
    // The tap switches the app to the desktop this registration belongs to.
    data: { navigate: navigateForDesktop(shown.navigate, desktop), desktop },
    silent: shown.silent,
  };
  if (shown.tag !== null) options.tag = desktopTag(desktop, shown.tag);
  return { title: titleWithLabel(shown.title, desktop === null ? null : label), options };
}

/** A notification tag made unique per desktop, so two desktops' tags never replace each other's notifications. */
export function desktopTag(desktop: string | null, tag: string): string {
  return desktop === null ? tag : `${desktop}:${tag}`;
}

/**
 * `navigate` (a path and hash) with the desktop named in the hash (`#d=<id>&…`), so the app opens
 * the notification on the desktop it came from. The hash's other parameters are kept.
 */
export function navigateForDesktop(navigate: string, desktop: string | null): string {
  if (desktop === null) return navigate;
  const url = new URL(navigate, 'https://app.invalid');
  const params = new URLSearchParams([['d', desktop], ...[...new URLSearchParams(url.hash.slice(1))].filter(([name]) => name !== 'd')]);
  return `${url.pathname}#${params.toString()}`;
}

export interface PushPayload {
  title: string;
  body: string;
  /** A same-origin path and hash (anything else becomes `/`). */
  navigate: string;
  tag: string | null;
  silent: boolean;
}

/** Shown when a push carries no readable payload: every push must show something. */
export const GENERIC_PUSH: PushPayload = {
  title: 'OmpDeck',
  body: 'Something on your desktop needs a look.',
  navigate: '/',
  tag: null,
  silent: false,
};

/**
 * The in-app destination of a notification's `navigate` (the gateway sends an absolute URL on this
 * origin): path and hash only, never another origin. Anything else is `/`.
 */
export function safeNavigate(navigate: unknown, origin: string): string {
  if (typeof navigate !== 'string') return '/';
  let url: URL;
  try {
    url = new URL(navigate, origin);
  } catch {
    return '/';
  }
  return url.origin === origin ? `${url.pathname}${url.hash}` : '/';
}

/** Reads a push payload; null when it is malformed. */
export function parsePush(text: string, origin: string): PushPayload | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof value !== 'object' || value === null || !('notification' in value)) return null;
  const notification: unknown = value.notification;
  if (typeof notification !== 'object' || notification === null) return null;
  const title = 'title' in notification ? notification.title : undefined;
  const body = 'body' in notification ? notification.body : undefined;
  const navigate = 'navigate' in notification ? notification.navigate : undefined;
  const tag = 'tag' in notification ? notification.tag : undefined;
  const silent = 'silent' in notification ? notification.silent : undefined;
  if (typeof title !== 'string' || title.length === 0) return null;
  return {
    title,
    body: typeof body === 'string' ? body : '',
    navigate: safeNavigate(navigate, origin),
    tag: typeof tag === 'string' ? tag : null,
    silent: silent === true,
  };
}
