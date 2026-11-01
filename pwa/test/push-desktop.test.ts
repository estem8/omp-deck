// Web Push per desktop: which registration (scope) belongs to which desktop, the kept VAPID key per
// registration, notification tags and links that name their desktop.

import { describe, expect, test } from 'bun:test';
import { desktopScopeUrl } from '../src/app/push-client.ts';
import {
  LEGACY_PUSH_KEY_URL,
  desktopIdFromScope,
  desktopScope,
  desktopTag,
  isDesktopId,
  GENERIC_PUSH,
  navigateForDesktop,
  notificationFor,
  pushKeyUrl,
  pushLabelUrl,
  pushLabels,
  safeNavigate,
  titleWithLabel,
} from '../src/push.ts';

const A = 'ab'.repeat(32);
const B = 'cd'.repeat(32);
const ORIGIN = 'https://relay.example';

describe('registration scope', () => {
  test('a desktop’s scope is /d/<id>/ and reads back to the id', () => {
    expect(desktopScope(A)).toBe(`/d/${A}/`);
    expect(desktopIdFromScope(`${ORIGIN}${desktopScope(A)}`)).toBe(A);
    expect(desktopIdFromScope(desktopScope(B))).toBe(B);
  });

  test('the root registration, other paths and malformed ids belong to no desktop', () => {
    expect(desktopIdFromScope(`${ORIGIN}/`)).toBeNull();
    expect(desktopIdFromScope(`${ORIGIN}/d/`)).toBeNull();
    expect(desktopIdFromScope(`${ORIGIN}/d/abc/`)).toBeNull();
    expect(desktopIdFromScope(`${ORIGIN}/d/${A.toUpperCase()}/`)).toBeNull();
    expect(desktopIdFromScope(`${ORIGIN}/d/${A}/more/`)).toBeNull();
    expect(desktopIdFromScope(`${ORIGIN}/x/d/${A}/`)).toBeNull();
    expect(desktopIdFromScope('')).toBeNull();
  });

  test('the app’s absolute scope is exactly what registration.scope reads', () => {
    expect(desktopScopeUrl(A, `${ORIGIN}/some/page#x`)).toBe(`${ORIGIN}/d/${A}/`);
    expect(desktopIdFromScope(desktopScopeUrl(A, `${ORIGIN}/`))).toBe(A);
  });

  test('desktop ids are 64 lowercase hex characters', () => {
    expect(isDesktopId(A)).toBe(true);
    expect(isDesktopId(A.slice(2))).toBe(false);
    expect(isDesktopId(undefined)).toBe(false);
    expect(isDesktopId(42)).toBe(false);
  });
});

describe('the kept VAPID key', () => {
  test('is stored per registration scope, so two desktops’ keys never overwrite each other', () => {
    const a = pushKeyUrl(`${ORIGIN}${desktopScope(A)}`);
    const b = pushKeyUrl(`${ORIGIN}${desktopScope(B)}`);
    expect(a).not.toBe(b);
    expect(a).not.toBe(pushKeyUrl(`${ORIGIN}/`));
    // The app and the worker compute the same entry for the same scope.
    expect(a).toBe(pushKeyUrl(desktopScopeUrl(A, `${ORIGIN}/`)));
    // None of them is version 1's single entry.
    expect([a, b, pushKeyUrl(`${ORIGIN}/`)]).not.toContain(LEGACY_PUSH_KEY_URL);
  });

  test('the scope survives the cache key round trip (it is a query value)', () => {
    const url = new URL(pushKeyUrl(`${ORIGIN}${desktopScope(A)}`), ORIGIN);
    expect(url.pathname).toBe('/__push-key');
    expect(url.searchParams.get('scope')).toBe(`${ORIGIN}/d/${A}/`);
  });
});

describe('notification tags and links', () => {
  test('tags are namespaced per desktop: the same gateway tag on two desktops never collides', () => {
    expect(desktopTag(A, 'sess-1')).not.toBe(desktopTag(B, 'sess-1'));
    expect(desktopTag(A, 'sess-1')).toBe(desktopTag(A, 'sess-1'));
    expect(desktopTag(A, 'sess-1')).toContain('sess-1');
    expect(desktopTag(null, 'sess-1')).toBe('sess-1');
  });

  test('the link names the desktop in the hash and keeps the rest', () => {
    expect(navigateForDesktop('/#s=sess-1', A)).toBe(`/#d=${A}&s=sess-1`);
    expect(navigateForDesktop('/', A)).toBe(`/#d=${A}`);
    expect(navigateForDesktop('/#route=inbox', A)).toBe(`/#d=${A}&route=inbox`);
    expect(navigateForDesktop('/#s=sess-1', null)).toBe('/#s=sess-1');
  });

  test('a desktop already in the link is replaced, never duplicated', () => {
    expect(navigateForDesktop(`/#d=${B}&s=x`, A)).toBe(`/#d=${A}&s=x`);
  });

  test('stays a same-origin path the worker’s guard accepts', () => {
    const navigate = navigateForDesktop('/#route=conversation&live=tmux%3A8%3Aomp-deck%3Ax', A);
    expect(safeNavigate(navigate, ORIGIN)).toBe(navigate);
    expect(navigate).toContain('live=tmux%3A8%3Aomp-deck%3Ax');
  });
});

describe('desktop labels on notifications', () => {
  const payload = { title: 'OmpDeck', body: 'A session finished', navigate: '/#s=sess-1', tag: 'sess-1', silent: false };

  test('the label sits in its own entry per registration scope, apart from the key', () => {
    const a = pushLabelUrl(`${ORIGIN}${desktopScope(A)}`);
    expect(a).not.toBe(pushLabelUrl(`${ORIGIN}${desktopScope(B)}`));
    expect(a).not.toBe(pushKeyUrl(`${ORIGIN}${desktopScope(A)}`));
    expect(new URL(a, ORIGIN).searchParams.get('scope')).toBe(`${ORIGIN}${desktopScope(A)}`);
  });

  test('one desktop gets no labels; several get their own', () => {
    expect(pushLabels([{ id: A, gateway: 'OmpDeck on host' }]).size).toBe(0);
    expect([...pushLabels([{ id: A, gateway: 'one' }, { id: B, gateway: 'two' }])]).toEqual([[A, 'one'], [B, 'two']]);
  });

  test('the title names the desktop; without a label it is unchanged', () => {
    expect(titleWithLabel('OmpDeck', 'Laptop')).toBe('OmpDeck — Laptop');
    expect(titleWithLabel('OmpDeck', null)).toBe('OmpDeck');
    expect(titleWithLabel('OmpDeck', '')).toBe('OmpDeck');
    // The gateway's default label: the title does not say OmpDeck twice.
    expect(titleWithLabel('OmpDeck', 'OmpDeck on axeleratorPC · 763f')).toBe('OmpDeck — axeleratorPC · 763f');
  });

  test('a labelled desktop’s notification: titled with the label, tag and link namespaced to it', () => {
    const { title, options } = notificationFor(payload, A, 'Laptop');
    expect(title).toBe('OmpDeck — Laptop');
    expect(options.body).toBe('A session finished');
    expect(options.tag).toBe(`${A}:sess-1`);
    expect(options.data).toEqual({ navigate: `/#d=${A}&s=sess-1`, desktop: A });
    expect(options.silent).toBe(false);
  });

  test('without a label (one desktop) the notification is exactly the payload’s, still bound to its desktop', () => {
    const { title, options } = notificationFor(payload, A, null);
    expect(title).toBe('OmpDeck');
    expect(options.tag).toBe(`${A}:sess-1`);
  });

  test('the root registration never carries a label, whatever is passed', () => {
    const { title, options } = notificationFor(payload, null, 'Laptop');
    expect(title).toBe('OmpDeck');
    expect(options.tag).toBe('sess-1');
    expect(options.data).toEqual({ navigate: '/#s=sess-1', desktop: null });
  });

  test('a payload-less push is the generic one, labelled the same way', () => {
    expect(notificationFor(GENERIC_PUSH, B, 'Laptop').title).toBe('OmpDeck — Laptop');
    expect(notificationFor(GENERIC_PUSH, B, 'Laptop').options.tag).toBeUndefined();
  });
});
