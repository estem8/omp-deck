// React bindings for the client store (`useSyncExternalStore`) and the feed subscriptions.

import { createContext, useContext, useEffect, useState, useSyncExternalStore } from 'react';
import type { api } from '../../remote/index.ts';
import { type Client, type ClientState, type TranscriptState, feedKey } from '../client.ts';
import type { Desktops, DesktopsState } from '../desktops.ts';

export const ClientContext = createContext<Client | null>(null);

export function useClient(): Client {
  const client = useContext(ClientContext);
  if (client === null) throw new Error('useClient outside ClientContext');
  return client;
}

/** A slice of the client state; `select` must return a stored reference or a primitive. */
export function useClientState<T>(select: (state: ClientState) => T): T {
  const client = useClient();
  return useSyncExternalStore(client.subscribeState, () => select(client.getState()));
}

export const DesktopsContext = createContext<Desktops | null>(null);

export function useDesktops(): Desktops {
  const desktops = useContext(DesktopsContext);
  if (desktops === null) throw new Error('useDesktops outside DesktopsContext');
  return desktops;
}

/** A slice of the desktops state; `select` must return a stored reference or a primitive. */
export function useDesktopsState<T>(select: (state: DesktopsState) => T): T {
  const desktops = useDesktops();
  return useSyncExternalStore(desktops.subscribe, () => select(desktops.getState()));
}

/** Whether mutations can be sent now (online with a Welcome). */
export function useOnline(): boolean {
  return useClientState((state) => state.conn.phase === 'online' && state.welcome !== null);
}

/** Whether `op` may be sent now: online and permitted by this device's permissions. */
export function useCan(): (op: api.Op) => boolean {
  const client = useClient();
  // Re-render when the connection or the permissions change.
  useClientState((state) => state.welcome);
  useClientState((state) => state.conn.phase);
  return (op) => client.canSend(op);
}

/**
 * Follows `feed` while mounted (null = none) and returns its latest value. The subscription
 * survives reconnects; offline, the last value stays (stale).
 */
export function useFeed<V extends api.FeedValue['feed']>(
  feed: api.Feed | null,
  kind: V,
): Extract<api.FeedValue, { feed: V }> | undefined {
  const client = useClient();
  const key = feed === null ? null : feedKey(feed);
  useEffect(() => {
    if (feed === null) return;
    const release = client.follow(feed);
    if (release === null) {
      client.notify('Too many open views on this phone; close one and try again.', 'warn');
      return;
    }
    return release;
    // `key` identifies the feed; the object itself is rebuilt on every render.
  }, [client, key]);
  const value = useClientState((state) => (key === null ? undefined : state.feeds.get(key)));
  return value !== undefined && value.feed === kind ? (value as Extract<api.FeedValue, { feed: V }>) : undefined;
}

/** Follows a transcript feed while mounted and returns its accumulated items. */
export function useTranscript(feed: Extract<api.Feed, { feed: 'transcript' }> | null): TranscriptState | undefined {
  const client = useClient();
  const key = feed === null ? null : feedKey(feed);
  useEffect(() => {
    if (feed === null) return;
    const release = client.follow(feed);
    if (release === null) {
      client.notify('Too many open views on this phone; close one and try again.', 'warn');
      return;
    }
    return release;
  }, [client, key]);
  return useClientState((state) => (key === null ? undefined : state.transcripts.get(key)));
}

/** The current time, refreshed every `everyMs` (relative ages, countdowns). */
export function useNow(everyMs = 30_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), everyMs);
    return () => clearInterval(timer);
  }, [everyMs]);
  return now;
}

/** The live session running `session` (an omp session id), from the `live` feed. */
export function useLiveFor(session: string | null): api.LiveSession | null {
  const live = useFeed({ feed: 'live' }, 'live');
  if (session === null || live === undefined) return null;
  return live.sessions.find((entry) => entry.session === session) ?? null;
}

/**
 * A project's folder as the desktop reports it (the `projects` feed, by id). The route's own
 * `display` can come from a link, so it only stands in while the feed has not delivered the project
 * (`resolved` false) and must never reach a confirmation.
 */
export function useProjectDisplay(project: string, placeholder: string): { text: string; resolved: boolean } {
  const feed = useFeed({ feed: 'projects' }, 'projects');
  const found = feed?.projects.find((entry) => entry.id === project);
  if (found !== undefined) return { text: found.display, resolved: true };
  return { text: placeholder.length > 0 ? placeholder : 'Project', resolved: false };
}
