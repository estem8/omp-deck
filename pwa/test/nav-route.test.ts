import { describe, expect, test } from 'bun:test';
import {
  deepLinkEntries,
  desktopFromHash,
  historyEntry,
  isBaseEntry,
  linkFromHash,
  routeFromHash,
  routeFromHistory,
  startRoute,
  syncedEntry,
} from '../src/app/nav.ts';
import { navigateForDesktop } from '../src/push.ts';
import { ROUTES } from './smoke/routes.ts';

describe('routeFromHash', () => {
  test('the notification deep link opens the conversation', () => {
    expect(routeFromHash('#s=s-convo')).toEqual({
      tab: 'sessions',
      stack: [{ kind: 'conversation', session: 's-convo', liveKey: null, title: 'Session' }],
    });
  });

  test('tabs and settings sections', () => {
    expect(routeFromHash('#route=inbox')).toEqual({ tab: 'inbox', stack: [] });
    expect(routeFromHash('#route=settings&section=usage')).toEqual({ tab: 'settings', stack: [{ kind: 'settings', section: 'usage' }] });
    expect(routeFromHash('#route=settings&section=nope')).toBeNull();
  });

  test('screens with their parameters', () => {
    expect(routeFromHash('#route=tree&session=s-1')?.stack).toEqual([{ kind: 'tree', session: 's-1', title: 'Session' }]);
    expect(routeFromHash('#route=worktrees&project=p-1')?.stack).toEqual([{ kind: 'worktrees', project: 'p-1', display: '' }]);
    expect(routeFromHash('#route=terminal&live=tmux%3A8%3Aomp-deck%3Ax')?.stack).toEqual([{ kind: 'terminal', liveKey: 'tmux:8:omp-deck:x', title: 'Terminal' }]);
    expect(routeFromHash('#route=subagent&session=s&agent=A')).toEqual({ tab: 'agents', stack: [{ kind: 'subagent', session: 's', agent: 'A' }] });
    expect(routeFromHash('#route=running')?.stack).toEqual([{ kind: 'running' }]);
    expect(routeFromHash('#route=browse')?.stack).toEqual([{ kind: 'browse' }]);
  });

  test('a link never names a title or folder (they are looked up by id)', () => {
    expect(routeFromHash('#route=worktrees&project=p-1&display=~%2Fscratch')?.stack).toEqual([{ kind: 'worktrees', project: 'p-1', display: '' }]);
    expect(routeFromHash('#route=changes&project=p-1&display=~%2Fscratch')?.stack).toEqual([{ kind: 'changes', project: 'p-1', display: '' }]);
    expect(routeFromHash('#route=tree&session=s-1&title=Spoofed')?.stack).toEqual([{ kind: 'tree', session: 's-1', title: 'Session' }]);
    expect(routeFromHash('#route=conversation&session=s-1&title=Spoofed')?.stack).toEqual([
      { kind: 'conversation', session: 's-1', liveKey: null, title: 'Session' },
    ]);
    expect(routeFromHash('#route=terminal&live=k&title=Spoofed')?.stack).toEqual([{ kind: 'terminal', liveKey: 'k', title: 'Terminal' }]);
  });

  test("a notification for a session without an id yet opens its conversation by live key (the gateway's encoding)", () => {
    expect(routeFromHash('#route=conversation&live=tmux%3A24%3Aomp-deck%3Aomp-new-17%26x%23y')).toEqual({
      tab: 'sessions',
      stack: [{ kind: 'conversation', session: null, liveKey: 'tmux:24:omp-deck:omp-new-17&x#y', title: 'Session' }],
    });
  });

  test('missing parameters, unknown routes and no route are null', () => {
    expect(routeFromHash('#route=tree')).toBeNull();
    expect(routeFromHash('#route=changes')).toBeNull();
    expect(routeFromHash('#route=bogus')).toBeNull();
    expect(routeFromHash('#theme=aurora')).toBeNull();
    expect(routeFromHash('')).toBeNull();
  });

  test('every documented smoke route but pair parses', () => {
    for (const [name, hash] of Object.entries(ROUTES)) {
      if (name === 'pair') continue;
      expect([name, routeFromHash(`#${hash}`)]).not.toEqual([name, null]);
    }
  });
});

const DESK = 'ab'.repeat(32);
const OTHER = 'cd'.repeat(32);

describe('desktop in deep links', () => {
  test('desktopFromHash reads a desktop id and nothing else', () => {
    expect(desktopFromHash(`#d=${DESK}&s=s-1`)).toBe(DESK);
    expect(desktopFromHash(`d=${DESK}`)).toBe(DESK);
    expect(desktopFromHash('#s=s-1')).toBeNull();
    // Not a gateway key: too short, upper case, or not hex.
    expect(desktopFromHash('#d=abc')).toBeNull();
    expect(desktopFromHash(`#d=${DESK.toUpperCase()}`)).toBeNull();
    expect(desktopFromHash(`#d=${'zz'.repeat(32)}`)).toBeNull();
    expect(desktopFromHash(`#d=${DESK}${DESK}`)).toBeNull();
  });

  test('linkFromHash pairs the desktop with the route, either may be absent', () => {
    expect(linkFromHash(`#d=${DESK}&s=s-convo`)).toEqual({
      desktop: DESK,
      route: { tab: 'sessions', stack: [{ kind: 'conversation', session: 's-convo', liveKey: null, title: 'Session' }] },
    });
    expect(linkFromHash(`#d=${DESK}`)).toEqual({ desktop: DESK, route: null });
    expect(linkFromHash('#route=inbox')).toEqual({ desktop: null, route: { tab: 'inbox', stack: [] } });
    expect(linkFromHash('#theme=aurora')).toBeNull();
    expect(linkFromHash('')).toBeNull();
  });

  test('the desktop does not disturb the route parameters, and a route link names its desktop too', () => {
    const live = 'tmux:24:omp-deck:omp-new-17&x#y';
    const hash = navigateForDesktop(`/#route=conversation&live=${encodeURIComponent(live)}`, DESK).split('#')[1] ?? '';
    expect(linkFromHash(hash)).toEqual({
      desktop: DESK,
      route: { tab: 'sessions', stack: [{ kind: 'conversation', session: null, liveKey: live, title: 'Session' }] },
    });
  });

  test('what the service worker builds from the gateway’s link round-trips', () => {
    expect(linkFromHash(navigateForDesktop('/#s=sess-1', DESK).split('#')[1] ?? '')).toEqual({
      desktop: DESK,
      route: { tab: 'sessions', stack: [{ kind: 'conversation', session: 'sess-1', liveKey: null, title: 'Session' }] },
    });
    expect(linkFromHash(navigateForDesktop('/', DESK).split('#')[1] ?? '')).toEqual({ desktop: DESK, route: null });
  });
});

describe('history entries per desktop', () => {
  const a = { kind: 'conversation', session: 'a', liveKey: null, title: 'A' } as const;

  test('an entry belongs to the desktop that wrote it', () => {
    const entry = historyEntry('sessions', [a], false, DESK);
    expect(routeFromHistory(entry, DESK)).toEqual({ tab: 'sessions', stack: [a] });
    // Another desktop's route means nothing here: the entry reads as one without a route.
    expect(routeFromHistory(entry, OTHER)).toBeNull();
    expect(isBaseEntry(entry, OTHER)).toBe(true);
    expect(startRoute(null, entry, OTHER)).toEqual({ tab: 'sessions', stack: [] });
    expect(startRoute(null, entry, DESK)).toEqual({ tab: 'sessions', stack: [a] });
  });

  test('entries written before desktops existed stay valid for whichever desktop is active', () => {
    const old = historyEntry('inbox', []);
    expect(routeFromHistory(old, DESK)).toEqual({ tab: 'inbox', stack: [] });
  });

  test('deep link and synced entries carry the desktop; a foreign base entry is rewritten, not trusted', () => {
    expect(deepLinkEntries({ tab: 'sessions', stack: [a] }, DESK).map((entry) => entry.desktop)).toEqual([DESK]);
    const foreign = historyEntry('sessions', [a], true, OTHER);
    const synced = syncedEntry(foreign, 'settings', [], DESK);
    expect(synced).toMatchObject({ tab: 'settings', desktop: DESK });
    expect(isBaseEntry(foreign, DESK)).toBe(true);
  });
});
