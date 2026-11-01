// The app shell: pairing gate, bottom-nav tabs, pushed screens (with browser-back support), the
// always-on `live` and `inbox` feeds, and toasts. One desktop is live at a time: everything below
// the desktop's `ClientContext` is keyed by its id, so switching desktops remounts it with fresh
// navigation, feeds and inbox state.

import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  type Link,
  type Nav,
  NavContext,
  type Route,
  type Screen,
  type Tab,
  deepLinkEntries,
  historyEntry,
  isBaseEntry,
  routeFromHistory,
  startRoute,
  syncedEntry,
} from './nav.ts';
import { AgentsScreen } from './screens/Agents.tsx';
import { ChangesScreen } from './screens/Changes.tsx';
import { ConversationScreen } from './screens/Conversation.tsx';
import { InboxScreen } from './screens/Inbox.tsx';
import { ManagerScreen } from './screens/Manager.tsx';
import { PairingScreen } from './screens/Pairing.tsx';
import { SessionsScreen } from './screens/Sessions.tsx';
import { SettingsScreen } from './screens/Settings.tsx';
import { SubagentScreen } from './screens/Subagent.tsx';
import { TerminalScreen } from './screens/Terminal.tsx';
import { BrowseScreen } from './screens/fleet/FolderBrowser.tsx';
import { RunningScreen } from './screens/fleet/Running.tsx';
import { TreeScreen } from './screens/fleet/Tree.tsx';
import { WorktreesScreen } from './screens/fleet/Worktrees.tsx';
import { SettingsSectionScreen } from './screens/settings/index.tsx';
import type { ThemeId } from './theme.ts';
import { ClientContext, useClientState, useDesktops, useDesktopsState, useFeed } from './ui/hooks.ts';
import { BottomNav, DesktopSheet, SwitcherContext, Toasts } from './ui/kit.tsx';
import { usePushSync } from './push-client.ts';

export interface AppProps {
  /** The pairing invite the page was opened with (already removed from the URL). */
  initialInvite: string | null;
  /** Where to open first (a `#route=` link, or a notification's `#d=…&s=…` link naming a desktop too). */
  initialLink: Link | null;
  theme: ThemeId;
  onTheme: (theme: ThemeId) => void;
  standalone: boolean;
  /** Registers the handler for deep links arriving while the app runs (notification taps, hash changes). */
  onDeepLink: (open: (link: Link) => void) => void;
  /** Registers the handler for a `#pair=` link opened while the app runs (the hash is already stripped). */
  onInviteLink: (open: (invite: string) => void) => void;
}

function Splash(): ReactNode {
  return (
    <div className="screen">
      <div className="pair-hero" style={{ marginTop: '30dvh' }}>
        <div className="mark">◆</div>
        <span className="busy" />
      </div>
    </div>
  );
}

export function App(props: AppProps): ReactNode {
  const desktops = useDesktops();
  const state = useDesktopsState((current) => current);
  const [invite, setInvite] = useState(props.initialInvite);
  // A link waiting for its desktop's shell to mount (cold start, or a tap for another desktop).
  const [pending, setPending] = useState<Link | null>(props.initialLink);
  // The mounted shell's own handler for links to a screen of the desktop it shows.
  const shellLink = useRef<((route: Route) => void) | null>(null);
  const onShellLink = useCallback((open: ((route: Route) => void) | null) => {
    shellLink.current = open;
  }, []);
  const onStarted = useCallback(() => setPending(null), []);
  const { onInviteLink, onDeepLink } = props;
  useEffect(() => {
    onInviteLink(setInvite);
  }, [onInviteLink]);

  useEffect(() => {
    onDeepLink((link) => {
      const now = desktops.getState();
      // A link for a desktop that is no longer paired opens nothing: its session ids mean nothing here.
      if (link.desktop !== null && !now.desktops.some((desktop) => desktop.id === link.desktop)) return;
      if (link.desktop !== null && link.desktop !== now.activeId) {
        setInvite(null);
        setPending(link);
        void desktops.switchTo(link.desktop);
        return;
      }
      if (link.route === null) return;
      if (shellLink.current !== null) shellLink.current(link.route);
      else setPending(link);
    });
  }, [onDeepLink, desktops]);

  if (!state.loaded) return <Splash />;
  const pairing = invite !== null || state.attempt !== null || state.adding || state.desktops.length === 0;
  // Pairing runs on its own client (toasts and the progress view read it); otherwise the active one.
  const client = state.attempt?.client ?? state.client;
  if (pairing) {
    const screen = <PairingScreen invite={invite} onInvite={setInvite} standalone={props.standalone} />;
    return client === null ? (
      screen
    ) : (
      <ClientContext.Provider value={client}>
        {screen}
        <Toasts />
      </ClientContext.Provider>
    );
  }
  if (state.client === null || state.activeId === null) return <Splash />;
  const route = pending !== null && (pending.desktop === null || pending.desktop === state.activeId) ? pending.route : null;
  return (
    <ClientContext.Provider value={state.client} key={state.activeId}>
      <DesktopHost {...props} initialRoute={route} onShellLink={onShellLink} onStarted={onStarted} />
    </ClientContext.Provider>
  );
}

/** Waits for the desktop's client to read its pairing, then shows the shell. */
function DesktopHost(props: ShellProps): ReactNode {
  const phase = useClientState((state) => state.conn.phase);
  const pairing = useClientState((state) => state.pairing);
  if (phase === 'loading' || phase === 'unpaired' || pairing === null) {
    return (
      <>
        <Splash />
        <Toasts />
      </>
    );
  }
  return <Shell {...props} />;
}

type ShellProps = Pick<AppProps, 'theme' | 'onTheme' | 'standalone'> & {
  initialRoute: Route | null;
  onShellLink: (open: ((route: Route) => void) | null) => void;
  onStarted: () => void;
};

function Shell({ initialRoute, theme, onTheme, standalone, onShellLink, onStarted }: ShellProps): ReactNode {
  // A link wins; a reload (hash already stripped) resumes from the route the history entry holds.
  const desktop = useDesktopsState((state) => state.activeId);
  const [start] = useState(() => startRoute(initialRoute, history.state, desktop));
  const [tab, setTabState] = useState<Tab>(start.tab);
  const [stack, setStack] = useState<readonly Screen[]>(() => start.stack);
  const [switcher, setSwitcher] = useState(false);
  const openSwitcher = useCallback(() => setSwitcher(true), []);
  // Always followed: running indicators, pending confirmations, and the inbox badge.
  useFeed({ feed: 'live' }, 'live');
  usePushSync();
  const inbox = useFeed({ feed: 'inbox' }, 'inbox');

  // Each pushed screen adds a history entry holding the whole route; the browser back button restores it.
  // The starting entry is kept in step with local navigation (`syncEntry`), so it never goes stale.
  const syncEntry = useCallback((at: Tab, next: readonly Screen[]) => {
    history.replaceState(syncedEntry(history.state, at, next, desktop), '');
  }, [desktop]);
  useEffect(() => {
    syncEntry(start.tab, start.stack);
  }, [syncEntry, start]);
  useEffect(() => {
    const onPop = (event: PopStateEvent) => {
      const route = routeFromHistory(event.state, desktop);
      if (route === null) {
        setStack([]);
        return;
      }
      setTabState(route.tab);
      setStack(route.stack);
    };
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, [desktop]);
  const depth = useRef(stack.length);
  depth.current = stack.length;
  const current = useRef({ tab, stack });
  current.current = { tab, stack };

  const push = useCallback((screen: Screen) => {
    const next = [...current.current.stack, screen];
    history.pushState(historyEntry(current.current.tab, next, false, desktop), '');
    setStack(next);
  }, [desktop]);
  const replace = useCallback(
    (screen: Screen) => {
      const { tab: at, stack: from } = current.current;
      const next = from.length === 0 ? [screen] : [...from.slice(0, -1), screen];
      syncEntry(at, next);
      setStack(next);
    },
    [syncEntry],
  );
  const pop = useCallback(() => {
    if (depth.current === 0) return;
    if (!isBaseEntry(history.state, desktop)) {
      history.back();
      return;
    }
    // The starting entry has nothing behind it in the app: pop locally and keep the entry current.
    const next = current.current.stack.slice(0, -1);
    syncEntry(current.current.tab, next);
    setStack(next);
  }, [syncEntry]);
  const setTab = useCallback(
    (next: Tab) => {
      syncEntry(next, []);
      setTabState(next);
      setStack([]);
    },
    [syncEntry],
  );
  const nav = useMemo<Nav>(() => ({ tab, stack, setTab, push, replace, pop }), [tab, stack, setTab, push, replace, pop]);

  useEffect(() => {
    onShellLink((route) => {
      setTabState(route.tab);
      // One history entry per screen, each holding its route, so the back button restores
      // whatever was showing before the link.
      for (const entry of deepLinkEntries(route, desktop)) history.pushState(entry, '');
      setStack(route.stack);
    });
    return () => onShellLink(null);
  }, [onShellLink, desktop]);
  // The link that brought this shell up is consumed: a later remount must not replay it.
  useEffect(() => onStarted(), [onStarted]);

  const top = stack.at(-1);
  return (
    <NavContext.Provider value={nav}>
      <SwitcherContext.Provider value={openSwitcher}>
        {top === undefined ? (
          <>
            {tab === 'sessions' && <SessionsScreen standalone={standalone} />}
            {tab === 'agents' && <AgentsScreen />}
            {tab === 'inbox' && <InboxScreen />}
            {tab === 'settings' && <SettingsScreen theme={theme} onTheme={onTheme} standalone={standalone} />}
            <BottomNav tab={tab} badge={inbox?.inbox.badge ?? null} onSelect={setTab} />
          </>
        ) : (
          <PushedScreen key={`${stack.length}:${JSON.stringify(top)}`} screen={top} />
        )}
        {switcher && <DesktopSheet onClose={() => setSwitcher(false)} />}
        <Toasts />
      </SwitcherContext.Provider>
    </NavContext.Provider>
  );
}

function PushedScreen({ screen }: { screen: Screen }): ReactNode {
  switch (screen.kind) {
    case 'conversation':
      return <ConversationScreen session={screen.session} liveKey={screen.liveKey} title={screen.title} />;
    case 'subagent':
      return <SubagentScreen session={screen.session} agent={screen.agent} />;
    case 'terminal':
      return <TerminalScreen liveKey={screen.liveKey} title={screen.title} />;
    case 'changes':
      return <ChangesScreen project={screen.project} display={screen.display} />;
    case 'manager':
      return <ManagerScreen />;
    case 'tree':
      return <TreeScreen session={screen.session} title={screen.title} />;
    case 'browse':
      return <BrowseScreen />;
    case 'worktrees':
      return <WorktreesScreen project={screen.project} display={screen.display} />;
    case 'running':
      return <RunningScreen />;
    case 'settings':
      return <SettingsSectionScreen section={screen.section} />;
  }
}
