// The phone's desktops: one pairing per desktop, one live connection (the active desktop's).
//
// `Desktops` owns the key store's list, which desktop is active, and the one `Client` talking to it.
// Switching stops that client (its link closes cleanly) and starts a `Client` for the chosen pairing,
// so every screen, feed and navigation state of the old desktop is gone with it (the app remounts by
// desktop id). Inactive desktops hold no connection; they reach the phone through Web Push only.
//
// A `Client` is bound to one pairing through a scoped `PairingStore`, so the Noise, relay and
// request logic knows nothing about other desktops. Pairing runs on its own client, scoped to the
// invite's desktop; the active client keeps running meanwhile and is replaced only when the new
// pairing is stored.

import { fingerprint, parsePairingInvite } from '../remote/index.ts';
import type { Client, PairingStore } from './client.ts';
import { type StoredPairing, pairingId } from './keystore.ts';

/** What the switcher and the settings list show of one paired desktop (no key material). */
export interface DesktopSummary {
  id: string;
  /** The gateway's label: the desktop's name. */
  gateway: string;
  /** The name this phone announced to that desktop. */
  name: string;
  /** The registry id the desktop gave this phone. */
  device: string;
  relay: string;
  /** Short fingerprint of the desktop's key, as the pairing dialog shows it. */
  fingerprint: string;
  pairedUnixMs: number;
}

export function summarize(pairing: StoredPairing): DesktopSummary {
  return {
    id: pairingId(pairing.gatewayPublic),
    gateway: pairing.gateway,
    name: pairing.name,
    device: pairing.device,
    relay: pairing.url,
    fingerprint: fingerprint(pairing.gatewayPublic),
    pairedUnixMs: pairing.pairedUnixMs,
  };
}

/**
 * Every desktop's summary. Gateway labels are `OmpDeck on <host>`, so two desktops with one host name
 * (two WSL instances, two machines called `localhost`) would be indistinguishable in the switcher:
 * those get their key's fingerprint head appended (the same short id the desktop's pairing dialog shows).
 */
export function summaries(pairings: readonly StoredPairing[]): DesktopSummary[] {
  const all = pairings.map(summarize);
  const count = new Map<string, number>();
  for (const entry of all) count.set(entry.gateway, (count.get(entry.gateway) ?? 0) + 1);
  return all.map((entry) => ((count.get(entry.gateway) ?? 0) > 1 ? { ...entry, gateway: `${entry.gateway} · ${entry.fingerprint.slice(0, 4)}` } : entry));
}

/** The desktop to start with: `wanted` when it is still paired, else the first one, else none. */
export function resolveActive(ids: readonly string[], wanted: string | null): string | null {
  return wanted !== null && ids.includes(wanted) ? wanted : (ids[0] ?? null);
}

/** The desktop that becomes active when `removed` is forgotten: the next in list order (wrapping), or none. */
export function nextActive(ids: readonly string[], removed: string): string | null {
  const at = ids.indexOf(removed);
  const rest = ids.filter((id) => id !== removed);
  return rest[Math.max(at, 0)] ?? rest[0] ?? null;
}

export type InviteInfo = { ok: true; id: string; relay: string; gatewayFingerprint: string } | { ok: false; error: string };

/** What a pairing invite names: the desktop's id (so the screen can tell a new desktop from a known one). */
export function inspectInvite(text: string): InviteInfo {
  try {
    const invite = parsePairingInvite(text);
    return { ok: true, id: pairingId(invite.gatewayPublic), relay: invite.url, gatewayFingerprint: fingerprint(invite.gatewayPublic) };
  } catch (error) {
    return { ok: false, error: error instanceof Error ? error.message : String(error) };
  }
}

/** The store as `Desktops` uses it (`KeyStore` implements it). */
export interface DesktopStore {
  list(): Promise<StoredPairing[]>;
  active(): Promise<string | null>;
  setActive(id: string | null): Promise<void>;
  legacyPush(): Promise<string | null>;
  clearLegacyPush(): Promise<void>;
  scoped(id: string): PairingStore;
}

export interface DesktopsDeps {
  store: DesktopStore;
  /** Builds the client of one pairing (not started). */
  createClient(id: string, store: PairingStore): Client;
}

export interface DesktopsState {
  /** The key store has been read. */
  loaded: boolean;
  desktops: readonly DesktopSummary[];
  activeId: string | null;
  /** The active desktop's client (null: none paired). */
  client: Client | null;
  /** A pairing in flight (or failed, waiting for Back / Try again), on its own client. */
  attempt: { id: string; client: Client; invite: string } | null;
  /** The pairing screen was opened with "Pair another desktop" (no invite yet). */
  adding: boolean;
  /** The desktop whose version 1 push subscription may still sit on the root service worker registration. */
  legacyPush: string | null;
}

export class Desktops {
  readonly #deps: DesktopsDeps;
  #state: DesktopsState = { loaded: false, desktops: [], activeId: null, client: null, attempt: null, adding: false, legacyPush: null };
  readonly #listeners = new Set<() => void>();
  /** Store changes run one at a time, in order. */
  #queue: Promise<unknown> = Promise.resolve();
  #unwatch: (() => void) | null = null;

  constructor(deps: DesktopsDeps) {
    this.#deps = deps;
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  readonly getState = (): DesktopsState => this.#state;

  #publish(patch: Partial<DesktopsState>): void {
    this.#state = { ...this.#state, ...patch };
    for (const listener of this.#listeners) listener();
  }

  #run<T>(job: () => Promise<T>): Promise<T> {
    const next = this.#queue.then(job, job);
    this.#queue = next.catch(() => undefined);
    return next;
  }

  /**
   * Reads the store and starts the active desktop. `prefer` (a notification link's desktop) wins
   * over the stored choice when that desktop is still paired. The stored choice may be stale (its
   * pairing gone): the first desktop stands in.
   */
  load(prefer: string | null): Promise<void> {
    return this.#run(async () => {
      const pairings = await this.#deps.store.list();
      const ids = pairings.map((pairing) => pairingId(pairing.gatewayPublic));
      const stored = await this.#deps.store.active();
      const active = resolveActive(ids, prefer !== null && ids.includes(prefer) ? prefer : stored);
      if (active !== stored) await this.#deps.store.setActive(active);
      const legacy = await this.#deps.store.legacyPush();
      if (legacy !== null && !ids.includes(legacy)) await this.#deps.store.clearLegacyPush();
      this.#publish({ loaded: true, desktops: summaries(pairings), legacyPush: legacy !== null && ids.includes(legacy) ? legacy : null });
      if (active !== null) this.#start(active);
    });
  }

  /** Makes `id` the active desktop: the current client stops, a client for `id` starts. */
  switchTo(id: string): Promise<void> {
    return this.#run(async () => {
      if (id === this.#state.activeId && this.#state.client !== null) {
        this.#publish({ adding: false });
        return;
      }
      const pairings = await this.#deps.store.list();
      if (!pairings.some((pairing) => pairingId(pairing.gatewayPublic) === id)) return;
      await this.#deps.store.setActive(id);
      this.#publish({ desktops: summaries(pairings), adding: false, attempt: null });
      this.#start(id);
    });
  }

  /** Opens the pairing screen without an invite ("Pair another desktop"). */
  beginAdd(): void {
    this.#publish({ adding: true });
  }

  cancelAdd(): void {
    this.#publish({ adding: false });
  }

  /**
   * Pairs with the desktop an invite names, on a client of its own. Pairing again with a desktop
   * that is already paired replaces only that record; another desktop is added. Resolves true once
   * it is paired, stored and active (the old client has stopped).
   */
  async pair(inviteText: string, name: string): Promise<boolean> {
    const info = inspectInvite(inviteText);
    if (!info.ok) return false;
    const client = this.#deps.createClient(info.id, this.#deps.store.scoped(info.id));
    this.#publish({ attempt: { id: info.id, client, invite: inviteText } });
    const paired = await client.pair(inviteText, name);
    if (!paired) return false;
    return this.#run(async () => {
      const pairings = await this.#deps.store.list();
      if (this.#state.attempt?.client !== client) {
        // Backed out while the pairing finished: it stays stored, this client is not the app's.
        client.stop();
        this.#publish({ desktops: summaries(pairings) });
        return false;
      }
      await this.#deps.store.setActive(info.id);
      const old = this.#state.client;
      this.#unwatch?.();
      this.#publish({ desktops: summaries(pairings), activeId: info.id, client, attempt: null, adding: false });
      this.#watch(client, info.id);
      old?.stop();
      return true;
    });
  }

  /** Leaves a failed pairing (the screen goes back to its invite form). */
  dismissPairing(): void {
    this.#publish({ attempt: null });
  }

  /** The version 1 push subscription of the migrated desktop moved or was dropped. */
  clearLegacyPush(): Promise<void> {
    return this.#run(async () => {
      await this.#deps.store.clearLegacyPush();
      this.#publish({ legacyPush: null });
    });
  }

  #start(id: string): void {
    const old = this.#state.client;
    this.#unwatch?.();
    const client = this.#deps.createClient(id, this.#deps.store.scoped(id));
    this.#publish({ activeId: id, client });
    this.#watch(client, id);
    old?.stop();
    void client.start();
  }

  /**
   * Follows the client for the end of its pairing: forgotten here (the Device settings), or its
   * record gone (evicted). Either way the next desktop takes over, or the pairing screen shows.
   */
  #watch(client: Client, id: string): void {
    const check = () => {
      if (client.getState().conn.phase !== 'unpaired' || this.#state.client !== client) return;
      this.#unwatch?.();
      this.#unwatch = null;
      void this.#run(() => this.#gone(id));
    };
    this.#unwatch = client.subscribeState(check);
    check();
  }

  async #gone(id: string): Promise<void> {
    const pairings = await this.#deps.store.list();
    const ids = pairings.map((pairing) => pairingId(pairing.gatewayPublic));
    const was = this.#state.desktops.map((desktop) => desktop.id);
    const active = resolveActive(ids, ids.includes(id) ? id : nextActive(was, id));
    await this.#deps.store.setActive(active);
    if (this.#state.legacyPush === id) await this.#deps.store.clearLegacyPush();
    const old = this.#state.client;
    this.#publish({ desktops: summaries(pairings), legacyPush: this.#state.legacyPush === id ? null : this.#state.legacyPush, activeId: active, client: null });
    old?.stop();
    if (active !== null) this.#start(active);
  }
}
