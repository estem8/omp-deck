// Service worker registration and user-confirmed updates: a new version waits until the user taps
// "Reload", and the reload waits for open requests (their outcomes would otherwise be looked up
// only after the next start).

type Listener = () => void;

export interface UpdateState {
  /** A new version is installed and waiting. */
  available: boolean;
  /** The user asked to reload; waiting for open requests to finish. */
  pending: boolean;
}

let state: UpdateState = { available: false, pending: false };
let waiting: ServiceWorker | null = null;
let reloading = false;
const listeners = new Set<Listener>();

function set(patch: Partial<UpdateState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

export const updates = {
  subscribe(listener: Listener): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  get(): UpdateState {
    return state;
  },
};

export interface WorkerMessages {
  /** A notification was tapped while the app was open: show `url`'s screen. */
  navigate(url: string): void;
}

/**
 * Registers `/sw.js` (when supported) and tracks waiting updates. Without service workers (an
 * insecure origin, a private window, a blocking browser policy) the app simply runs online-only.
 */
export function registerServiceWorker(messages: WorkerMessages): void {
  // Absent outside secure contexts even where the API exists.
  const container: ServiceWorkerContainer | undefined = navigator.serviceWorker;
  if (container === undefined) return;
  container.addEventListener('message', (event: MessageEvent<unknown>) => {
    const data = event.data;
    if (typeof data !== 'object' || data === null || !('type' in data)) return;
    if (data.type === 'navigate' && 'url' in data && typeof data.url === 'string') messages.navigate(data.url);
  });
  container.addEventListener('controllerchange', () => {
    if (!reloading) return;
    location.reload();
  });
  // Some environments (blocked workers) fulfil `register` without a registration.
  const track = (registration: ServiceWorkerRegistration | undefined) => {
    if (registration === undefined) return;
    const offer = (worker: ServiceWorker | null) => {
      // Only an update needs confirmation: the first install takes over silently.
      if (worker === null || container.controller === null) return;
      waiting = worker;
      set({ available: true });
    };
    offer(registration.waiting);
    registration.addEventListener('updatefound', () => {
      const installing = registration.installing;
      installing?.addEventListener('statechange', () => {
        if (installing.state === 'installed') offer(installing);
      });
    });
  };
  let registering: Promise<ServiceWorkerRegistration | undefined>;
  try {
    registering = container.register('/sw.js', { scope: '/' });
  } catch {
    return;
  }
  registering.then(track, () => {
    // No service worker (e.g. a private window): the app still works online.
  });
}

/**
 * Asks the service worker to reload this app's other windows. A window of an older version keeps
 * its key store connection open and blocks this version's upgrade of it; reloading it runs the new
 * code there, which lets go. Best effort: without a worker nothing happens, and the caller's screen
 * tells the user to close the other windows.
 */
export async function askOthersToReload(): Promise<void> {
  const container: ServiceWorkerContainer | undefined = navigator.serviceWorker;
  if (container === undefined) return;
  // The controller is absent right after a forced reload or the first install: the registration's active worker stands in.
  const worker = container.controller ?? (await container.ready).active;
  worker?.postMessage({ type: 'RELOAD_OTHERS' });
}

/**
 * Activates the waiting version once `openRequests()` is zero (or at once with `force`); the
 * `controllerchange` that follows reloads the page.
 */
export function applyUpdate(openRequests: () => number, force: boolean): void {
  const worker = waiting;
  if (worker === null) return;
  stopWatching();
  if (!force && openRequests() > 0) {
    set({ pending: true });
    watcher = setInterval(() => {
      // Later (or anything else) withdrew the request: stop for good, whatever the open count is.
      if (!state.pending) {
        stopWatching();
        return;
      }
      if (openRequests() === 0) {
        stopWatching();
        applyUpdate(openRequests, true);
      }
    }, 500);
    return;
  }
  reloading = true;
  set({ pending: true });
  worker.postMessage({ type: 'SKIP_WAITING' });
}

let watcher: ReturnType<typeof setInterval> | null = null;

function stopWatching(): void {
  if (watcher === null) return;
  clearInterval(watcher);
  watcher = null;
}

/** Withdraws a pending reload (the user changed their mind while requests were open). */
export function cancelUpdate(): void {
  stopWatching();
  set({ pending: false });
}
