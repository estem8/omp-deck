// Regression tests for the slice-4 review findings in the PWA.

import { afterEach, describe, expect, test } from 'bun:test';
import { ClientError } from '../src/app/client.ts';
import { deepLinkEntries, historyEntry, isBaseEntry, routeFromHistory, startRoute, syncedEntry } from '../src/app/nav.ts';
import { type Attachment, attachmentsReducer } from '../src/app/screens/conversation/compose.ts';
import { LoadQueue } from '../src/app/screens/conversation/images.ts';
import { UNOBSERVED_TEXT, describeResult, succeeded } from '../src/app/ui/format.ts';
import { applyUpdate, cancelUpdate, registerServiceWorker, updates } from '../src/app/update.ts';
import type { RequestView } from '../src/app/client.ts';

function attachment(id: number, upload: string | null): Attachment {
  return { id, name: `f${id}`, mime: 'text/plain', size: 10, status: upload === null ? 'uploading' : 'done', sent: 0, upload, error: null };
}

describe('#3 a send clears only what it submitted', () => {
  test('attachments added during the send survive', () => {
    let state: readonly Attachment[] = [attachment(1, 'up-1')];
    const submitted = ['up-1'];
    state = attachmentsReducer(state, { type: 'add', attachment: attachment(2, null) });
    state = attachmentsReducer(state, { type: 'done', id: 2, upload: 'up-2' });
    state = attachmentsReducer(state, { type: 'remove_sent', uploads: submitted });
    expect(state.map((entry) => entry.upload)).toEqual(['up-2']);
  });
});

describe('#12 blob load queue', () => {
  test('runs at most two at a time', async () => {
    const queue = new LoadQueue(2, async () => undefined);
    let active = 0;
    let peak = 0;
    const gates: Array<() => void> = [];
    const task = () =>
      new Promise<void>((done) => {
        active += 1;
        peak = Math.max(peak, active);
        gates.push(() => {
          active -= 1;
          done();
        });
      });
    const runs = [0, 1, 2, 3, 4].map(() => queue.run(task));
    await Promise.resolve();
    await Promise.resolve();
    expect(gates.length).toBe(2);
    while (gates.length > 0) {
      gates.shift()?.();
      await new Promise((resume) => setTimeout(resume, 0));
    }
    await Promise.all(runs);
    expect(peak).toBe(2);
  });

  test('too_many_requests is retried after a backoff, other errors are not', async () => {
    const waits: number[] = [];
    const queue = new LoadQueue(2, async (ms) => void waits.push(ms));
    let calls = 0;
    const result = await queue.run(async () => {
      calls += 1;
      if (calls < 3) throw new ClientError('api', 'busy', { code: 'too_many_requests' });
      return 'ok';
    });
    expect(result).toBe('ok');
    expect(calls).toBe(3);
    expect(waits.length).toBe(2);
    expect(waits[1] ?? 0).toBeGreaterThan(waits[0] ?? 0);
    let other = 0;
    await expect(
      queue.run(async () => {
        other += 1;
        throw new Error('broken');
      }),
    ).rejects.toThrow('broken');
    expect(other).toBe(1);
  });
});

describe('#13 Later stops the update timer', () => {
  const realNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  afterEach(() => {
    if (realNavigator !== undefined) Object.defineProperty(globalThis, 'navigator', realNavigator);
  });

  test('a request finishing after Later does not reload', async () => {
    const posted: unknown[] = [];
    const worker = { postMessage: (message: unknown) => void posted.push(message) };
    const container = {
      controller: {},
      addEventListener: () => undefined,
      register: async () => ({ waiting: worker, addEventListener: () => undefined, installing: null }),
    };
    Object.defineProperty(globalThis, 'navigator', { value: { serviceWorker: container }, configurable: true });
    registerServiceWorker({ navigate: () => undefined });
    await new Promise((resume) => setTimeout(resume, 0));
    expect(updates.get().available).toBe(true);

    let open = 1;
    applyUpdate(() => open, false);
    expect(updates.get().pending).toBe(true);
    cancelUpdate();
    open = 0;
    await new Promise((resume) => setTimeout(resume, 1200));
    expect(posted).toEqual([]);
    expect(updates.get().pending).toBe(false);
  });
});

describe('#14 history entries carry the route', () => {
  const a = { kind: 'conversation', session: 'a', liveKey: null, title: 'A' } as const;
  const b = { kind: 'conversation', session: 'b', liveKey: null, title: 'B' } as const;

  test('a deep link on top of a pushed screen leaves the previous route to Back', () => {
    // On conversation `a` at depth 1; a notification links to `b`.
    const before = historyEntry('sessions', [a]);
    const [link] = deepLinkEntries({ tab: 'sessions', stack: [b] });
    expect(routeFromHistory(before)).toEqual({ tab: 'sessions', stack: [a] });
    expect(routeFromHistory(link)).toEqual({ tab: 'sessions', stack: [b] });
    expect(link?.depth).toBe(before.depth);
  });

  test('every screen of a link gets its own prefix, a bare tab gets one entry', () => {
    expect(deepLinkEntries({ tab: 'sessions', stack: [a, b] }).map((entry) => entry.stack.length)).toEqual([1, 2]);
    expect(deepLinkEntries({ tab: 'inbox', stack: [] })).toEqual([{ depth: 0, tab: 'inbox', stack: [] }]);
    expect(routeFromHistory({ depth: 1 })).toBeNull();
    expect(routeFromHistory(null)).toBeNull();
  });
});

describe('#15 unobserved delivery', () => {
  const view = {
    status: 'done',
    label: 'Send',
    outcome: { state: 'done', result: { type: 'submitted', request: 1, label: 'Send', delivery: { state: 'unobserved' } } },
  } as unknown as RequestView;

  test('is shown as uncertain and is not a success', () => {
    expect(describeResult(view)).toEqual({ text: `Send: ${UNOBSERVED_TEXT}`, tone: 'warn' });
    expect(succeeded(view)).toBe(false);
    expect(UNOBSERVED_TEXT).toBe('Sent, but the desktop could not confirm omp received it — check the session before retrying.');
  });
});

describe('#14 history survives reloads and root changes', () => {
  const a = { kind: 'conversation', session: 'a', liveKey: null, title: 'A' } as const;
  const section = { kind: 'settings', section: 'usage' } as const;

  test('a reload resumes from the history entry, an incoming link still wins', () => {
    const state = historyEntry('sessions', [a]);
    expect(startRoute(null, state)).toEqual({ tab: 'sessions', stack: [a] });
    expect(startRoute({ tab: 'inbox', stack: [] }, state)).toEqual({ tab: 'inbox', stack: [] });
    expect(startRoute(null, null)).toEqual({ tab: 'sessions', stack: [] });
  });

  test('cold-open A, Back, Settings, a section, browser Back lands on Settings, not A', () => {
    // Boot on A: the base entry records it.
    let base: unknown = syncedEntry(null, 'sessions', [a]);
    expect(isBaseEntry(base)).toBe(true);
    // In-app Back pops locally at the base entry and keeps it current; so does switching tab.
    base = syncedEntry(base, 'sessions', []);
    base = syncedEntry(base, 'settings', []);
    // Pushing the section writes a separate, non-base entry; the base is untouched.
    const pushed = historyEntry('settings', [section]);
    expect(isBaseEntry(pushed)).toBe(false);
    // Browser Back returns to the base entry: Settings root.
    expect(routeFromHistory(base)).toEqual({ tab: 'settings', stack: [] });
    // Reloading on the base entry resumes there too, and replace keeps the flag.
    expect(startRoute(null, base)).toEqual({ tab: 'settings', stack: [] });
    expect(isBaseEntry(syncedEntry(base, 'settings', [section]))).toBe(true);
    expect(isBaseEntry(syncedEntry(pushed, 'settings', []))).toBe(false);
  });
});
