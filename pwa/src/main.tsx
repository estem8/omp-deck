// Boot: theme, deep links (`#pair=`, `#s=`, `#d=`), the key store and the desktops (one `Client`, for
// the active desktop), the service worker, and wake-ups on network/visibility changes.

import { StrictMode, useCallback, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './app/App.tsx';
import { Client, browserClock } from './app/client.ts';
import { Desktops } from './app/desktops.ts';
import { KeyStore } from './app/keystore.ts';
import { dropBrowserPush, watchPushLabels } from './app/push-client.ts';
import { dialRelay, pairWithInvite } from './app/link.ts';
import { type Link, inviteFromHash, linkFromHash } from './app/nav.ts';
import { type ThemeId, applyTheme, loadTheme, saveTheme } from './app/theme.ts';
import { DesktopsContext } from './app/ui/hooks.ts';
import { askOthersToReload, registerServiceWorker } from './app/update.ts';
import './styles/app.css';
import './styles/themes.css';

function storage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function platform(): string {
  const ua = navigator.userAgent;
  if (/iPhone|iPad|iPod/.test(ua)) return 'iOS';
  if (/Android/.test(ua)) return 'Android';
  if (/Mac OS X/.test(ua)) return 'macOS';
  if (/Windows/.test(ua)) return 'Windows';
  if (/Linux/.test(ua)) return 'Linux';
  return 'Web';
}

function isStandalone(): boolean {
  return (
    window.matchMedia('(display-mode: standalone)').matches ||
    (navigator as Navigator & { standalone?: boolean }).standalone === true
  );
}

// The invite and deep link are read once and removed from the URL at once (the invite is a
// secret; it must not stay in history or be bookmarked).
const hash = location.hash;
const initialInvite = inviteFromHash(hash);
const initialLink = linkFromHash(hash);
if (hash.length > 0) history.replaceState(null, '', `${location.pathname}${location.search}`);

const theme = loadTheme(storage());
applyTheme(document, theme);

const rootElement = document.getElementById('root');
if (rootElement === null) throw new Error('index.html lacks #root');
const root = createRoot(rootElement);

let deepLink: ((link: Link) => void) | null = null;
const openDeepLink = (url: string) => {
  const link = linkFromHash(new URL(url, location.href).hash);
  if (link !== null) deepLink?.(link);
};
// A route link opened while the app runs (e.g. pasted, or a notification on platforms that
// navigate the existing window) opens its screen too, on the desktop it names (`#d=`).
let inviteLink: ((invite: string) => void) | null = null;
window.addEventListener('hashchange', () => {
  const invite = inviteFromHash(location.hash);
  if (invite !== null) {
    // The invite is a secret: off the URL at once, then on to the pairing flow (the paired desktops stay paired).
    history.replaceState(null, '', `${location.pathname}${location.search}`);
    inviteLink?.(invite);
    return;
  }
  const link = linkFromHash(location.hash);
  if (link === null) return;
  history.replaceState(null, '', `${location.pathname}${location.search}`);
  deepLink?.(link);
});

function Root({ desktops }: { desktops: Desktops }) {
  const [current, setCurrent] = useState<ThemeId>(theme);
  const onTheme = useCallback((next: ThemeId) => {
    setCurrent(next);
    applyTheme(document, next);
    saveTheme(storage(), next);
  }, []);
  const onDeepLink = useCallback((open: (link: Link) => void) => {
    deepLink = open;
  }, []);
  const onInviteLink = useCallback((open: (invite: string) => void) => {
    inviteLink = open;
  }, []);
  return (
    <DesktopsContext.Provider value={desktops}>
      <App
        initialInvite={initialInvite}
        initialLink={initialLink}
        theme={current}
        onTheme={onTheme}
        standalone={isStandalone()}
        onDeepLink={onDeepLink}
        onInviteLink={onInviteLink}
      />
    </DesktopsContext.Provider>
  );
}

function Fatal({ message }: { message: string }) {
  return (
    <div className="screen">
      <div className="body">
        <div className="pair-hero">
          <div className="mark">!</div>
          <h2>OmpDeck cannot start here</h2>
          <p className="muted">{message}</p>
        </div>
      </div>
    </div>
  );
}

/** Shown while another window still on the old version holds the key store: the upgrade waits for it. */
function Updating() {
  return (
    <div className="screen">
      <div className="body">
        <div className="pair-hero">
          <div className="mark">↻</div>
          <h2>Finishing the update…</h2>
          <p className="muted">
            Another OmpDeck window still runs the old version; it is being reloaded. If this stays, close the other OmpDeck tabs and windows.
          </p>
        </div>
      </div>
    </div>
  );
}

async function boot(): Promise<void> {
  // First: the worker reloads the windows that block the key store's upgrade (below).
  registerServiceWorker({ navigate: openDeepLink });
  let store: KeyStore;
  try {
    store = await KeyStore.open(indexedDB, {
      onBlocked: () => {
        root.render(<Updating />);
        void askOthersToReload().catch(() => undefined);
      },
      // A later upgrade by another window: let it through, and come back as the new version.
      onVersionChange: () => location.reload(),
    });
  } catch (error) {
    root.render(
      <Fatal message={`This browser cannot keep the pairing key (IndexedDB: ${error instanceof Error ? error.message : String(error)}). Leave private browsing or use another browser.`} />,
    );
    return;
  }
  const desktops: Desktops = new Desktops({
    store,
    createClient: (id, scoped) => {
      const client = new Client({
        store: scoped,
        dial: (target, events) => dialRelay(target, events),
        pair: (invite, keypair, hello, onStage) => pairWithInvite(invite, keypair, hello, onStage),
        clock: browserClock,
        random: Math.random,
        platform: platform(),
        persistStorage: () => navigator.storage?.persist?.() ?? Promise.resolve(false),
        // This desktop's subscription and registration only; the migrated desktop also takes the version 1 root one.
        dropPush: () => dropBrowserPush(id, desktops.getState().legacyPush === id),
      });
      // The desktop skips push notifications while this phone is visible and connected.
      client.setVisible(document.visibilityState === 'visible');
      return client;
    },
  });
  root.render(
    <StrictMode>
      <Root desktops={desktops} />
    </StrictMode>,
  );
  // The worker names the desktop on each notification while several are paired.
  watchPushLabels(desktops);
  window.addEventListener('online', () => desktops.getState().client?.wake());
  document.addEventListener('visibilitychange', () => {
    const visible = document.visibilityState === 'visible';
    const client = desktops.getState().client;
    client?.setVisible(visible);
    if (visible) client?.wake();
  });
  try {
    await desktops.load(initialLink?.desktop ?? null);
  } catch (error) {
    root.render(<Fatal message={`Could not read this phone’s pairings: ${error instanceof Error ? error.message : String(error)}`} />);
  }
}

void boot();
