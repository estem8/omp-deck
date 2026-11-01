// The one client object of the app (mobile-pwa.md "State"): it owns the stored pairing, the relay
// link and its Noise session, the request ids and seqs, outcome recovery, feeds and terminal views,
// and publishes one immutable snapshot for `useSyncExternalStore`.
//
// Offline rules: the last known state stays visible (stale) and every mutation is refused at once;
// nothing is queued for later. Journaled requests are never re-sent automatically: after a
// reconnect (and on `resync`) their outcomes are looked up, and an `unknown` one is offered to the
// user for an explicit resend.

import {
  ACK_EVERY_BYTES,
  AckPolicy,
  api,
  type AppRecord,
  CLOSE,
  type DeviceHello,
  fingerprint,
  Keypair,
  MAX_INPUT,
  type PairingInvite,
  parsePairingInvite,
} from '../remote/index.ts';
import type { StoredPairing } from './keystore.ts';
import type { Dialer, Link, LinkClose, PairingResult, PairingStage } from './link.ts';
import { decideRetry, type Stop } from './retry.ts';

/** Optional features this app announces in `DeviceHello.features`. */
export const DEVICE_FEATURES: readonly string[] = [api.FEATURE_S4];

/** Feeds of protocol v1; every other feed needs a gateway announcing `s4`. */
const V1_FEEDS: Partial<Record<api.Feed['feed'], true>> = {
  projects: true,
  sessions: true,
  live: true,
  inbox: true,
  cards: true,
  transcript: true,
  changes: true,
};

/** Ops of protocol v1; every other op needs a gateway announcing `s4`. */
const V1_OPS: Partial<Record<api.OpName, true>> = {
  subscribe: true,
  unsubscribe: true,
  outcome: true,
  models: true,
  diff: true,
  attach: true,
  detach: true,
  ping: true,
  new_session: true,
  resume: true,
  restart: true,
  stop: true,
  kill: true,
  prompt: true,
  interrupt: true,
  switch_model: true,
  set_effort: true,
  action: true,
  answer: true,
  cancel: true,
  inbox_open: true,
  inbox_dismiss: true,
  inbox_clear: true,
  stage: true,
  unstage: true,
  delete_sessions: true,
  copy_session: true,
  move_session: true,
  discard: true,
  delete_untracked: true,
};

/** Whether `feed` is a slice-4 feed. */
export function isS4Feed(feed: api.Feed): boolean {
  return V1_FEEDS[feed.feed] !== true;
}

/** Whether `op` needs a slice-4 gateway (a slice-4 op, or a subscription to a slice-4 feed). */
export function isS4Op(op: api.Op): boolean {
  if (op.op === 'subscribe' || op.op === 'unsubscribe') return isS4Feed(op.feed);
  return V1_OPS[op.op] !== true;
}

/** Whether the gateway behind `welcome` speaks slice 4. */
export function hasS4(welcome: api.Welcome | null): boolean {
  return welcome !== null && welcome.features.includes(api.FEATURE_S4);
}

/** Shown where a slice-4 surface meets an older desktop. */
export const NEEDS_S4 = 'This needs a newer OmpDeck on the desktop. Update OmpDeck there, then reconnect.';

/** Recently finished requests kept for display. */
const RECENT_REQUESTS = 20;
/** A running outcome found after a reconnect is looked up again this often. */
const OUTCOME_POLL_MS = 5_000;
/** Terminal records that arrive before their `attached` reply, per view at most. */
const MAX_ORPHAN_RECORDS = 256;
const MAX_NOTICES = 4;

// ---- dependencies -------------------------------------------------------------------------------

export interface PairingStore {
  load(): Promise<StoredPairing | null>;
  save(pairing: StoredPairing): Promise<void>;
  reserveTimestamp(nowMs: number): Promise<number>;
  clear(): Promise<void>;
}

export type Pairer = (
  invite: PairingInvite,
  keypair: Keypair,
  hello: DeviceHello,
  onStage: (stage: PairingStage) => void,
) => Promise<PairingResult>;

export interface Clock {
  now(): number;
  setTimer(run: () => void, ms: number): number;
  clearTimer(handle: number): void;
}

/** How long forgetting waits for the desktop to confirm the push unsubscribe. */
export const PUSH_DROP_WAIT_MS = 3000;

export interface ClientDeps {
  store: PairingStore;
  dial: Dialer;
  pair: Pairer;
  clock: Clock;
  random: () => number;
  /** Shown to the desktop at pairing (display-safe, ≤ 32 characters). */
  platform: string;
  /** `navigator.storage.persist()`; asked once a pairing is stored. */
  persistStorage: () => Promise<boolean>;
  /** Drops this browser's push subscription and the kept VAPID key (best effort, local only). */
  dropPush?: () => Promise<void>;
}

export const browserClock: Clock = {
  now: () => Date.now(),
  setTimer: (run, ms) => window.setTimeout(run, ms),
  clearTimer: (handle) => window.clearTimeout(handle),
};

// ---- published state ----------------------------------------------------------------------------

export type ConnPhase =
  /** Reading the stored pairing. */
  | 'loading'
  /** No pairing on this device (never paired, forgotten, or evicted). */
  | 'unpaired'
  | 'connecting'
  | 'online'
  /** Disconnected; the next attempt is scheduled at `retryAt`. */
  | 'waiting'
  /** Disconnected; retrying needs the user (see `stop`). */
  | 'stopped';

export interface ConnState {
  phase: ConnPhase;
  /** Failed attempts since the last session. */
  attempt: number;
  retryAt: number | null;
  stop: Stop | null;
  lastClose: LinkClose | null;
  /** A session was established at least once since the app started. */
  everOnline: boolean;
}

export interface PairingSummary {
  device: string;
  gateway: string;
  url: string;
  name: string;
  pairedUnixMs: number;
  /** This phone's key fingerprint (what the desktop showed at approval). */
  fingerprint: string;
  gatewayFingerprint: string;
}

export interface PairingFlow {
  stage: PairingStage | 'failed';
  name: string;
  /** This phone's fresh key: compare with the desktop's approval dialog. */
  fingerprint: string;
  gatewayFingerprint: string;
  relay: string;
  error: string | null;
}

export interface TranscriptState {
  /** Index of `items[0]` in the transcript. */
  start: number;
  items: readonly api.Item[];
}

export type RequestStatus =
  | 'sending'
  | 'accepted'
  | 'running'
  | 'done'
  | 'failed'
  | 'rejected'
  | 'cancelled'
  | 'uncertain'
  | 'recorded'
  | 'unknown'
  | 'expired'
  /** The gateway refused the request outright (an `ApiError` reply). */
  | 'error';

export interface RequestView {
  seq: number;
  label: string;
  op: api.Op;
  status: RequestStatus;
  outcome: api.OutcomeView | null;
  error: api.ApiError | null;
  progress: { stage: string; done: number; total: number | null } | null;
  startedUnixMs: number;
}

export interface ConfirmView {
  live: api.LiveRef;
  request: number;
  label: string;
  draft: string;
}

export interface Notice {
  id: number;
  text: string;
  tone: 'info' | 'warn' | 'error';
}

export interface ClientState {
  conn: ConnState;
  pairing: PairingSummary | null;
  pairingFlow: PairingFlow | null;
  welcome: api.Welcome | null;
  feeds: ReadonlyMap<string, api.FeedValue>;
  transcripts: ReadonlyMap<string, TranscriptState>;
  /** Open requests first, then the recently finished ones (newest first). */
  requests: readonly RequestView[];
  confirms: readonly ConfirmView[];
  models: { models: api.Model[]; truncated: boolean } | null;
  notices: readonly Notice[];
}

/** Whether a request has its final answer. */
export function isFinal(status: RequestStatus): boolean {
  return status !== 'sending' && status !== 'accepted' && status !== 'running';
}

/** A stable key per subscription, equal for a request's feed and the gateway's echo of it. */
export function feedKey(feed: api.Feed): string {
  switch (feed.feed) {
    case 'projects':
    case 'live':
    case 'inbox':
    case 'running':
    case 'usage':
    case 'backup':
    case 'sync':
    case 'updates':
    case 'device':
      return feed.feed;
    case 'sessions':
      return `sessions:${feed.project}:${feed.limit}`;
    case 'cards':
      return `cards:${feed.sessions.join(',')}`;
    case 'transcript':
      return `transcript:${feed.session}:${feed.agent ?? ''}:${feed.tail}`;
    case 'changes':
      return `changes:${feed.project}`;
    case 'inventory':
      return `inventory:${feed.project ?? ''}:${feed.limit}`;
    case 'context':
      return `context:${feed.live.key}:${feed.live.generation}`;
    case 'agents':
      return `agents:${feed.session}`;
    case 'worktrees':
      return `worktrees:${feed.project}`;
  }
}

// ---- errors -------------------------------------------------------------------------------------

export type ClientErrorKind = 'offline' | 'not_allowed' | 'busy' | 'disconnected' | 'api' | 'invalid' | 'unsupported';

export class ClientError extends Error {
  override name = 'ClientError';
  constructor(
    readonly kind: ClientErrorKind,
    message: string,
    readonly api: api.ApiError | null = null,
  ) {
    super(message);
  }
}

/** A readable sentence for an `ApiError`. */
export function describeApiError(error: api.ApiError): string {
  switch (error.code) {
    case 'unsupported':
      return 'The desktop does not support this yet.';
    case 'not_allowed':
      return `This phone lacks the ${error.need} permission.`;
    case 'bad_request':
      return `Refused: ${error.message}`;
    case 'not_found':
      return `Not found: ${error.what}.`;
    case 'stale':
      return 'The session changed meanwhile; try again.';
    case 'rejected':
      return describeRejection(error.reason);
    case 'geometry_owned':
      return 'Another view controls this terminal’s size.';
    case 'too_many_requests':
      return 'Too many requests at once.';
    case 'too_large':
      return 'The answer is too large to send.';
    case 'maintenance':
      return 'The desktop is busy with maintenance.';
    case 'key_reused':
      return 'This request number was already used.';
    case 'expired':
      return 'The request expired.';
    case 'failed':
      return error.message;
  }
}

export function describeRejection(reason: api.RejectedView): string {
  switch (reason.code) {
    case 'stale':
      return 'The session changed meanwhile.';
    case 'exited':
      return 'The session has exited.';
    case 'read_only':
      return 'The session is read-only.';
    case 'detached':
      return 'The session is detached.';
    case 'busy':
      return 'omp is working; wait for it or interrupt.';
    case 'no_bracketed_paste':
      return 'The terminal does not accept pasted text right now.';
    case 'not_injected':
      return 'The text could not be typed into the session.';
    case 'empty':
      return 'Nothing to send.';
    case 'no_change':
      return 'Nothing changed.';
    case 'model_unknown':
      return 'omp does not know that model.';
    case 'key_reused':
      return 'This request number was already used.';
    case 'expired':
      return 'The request expired.';
    case 'input_held':
      return 'Terminal input is held by the desktop.';
    case 'bad_path':
      return 'That file is not in the change list.';
  }
}

// ---- terminal views -----------------------------------------------------------------------------

export interface ViewSink {
  snapshot(bytes: Uint8Array, last: boolean): void;
  output(bytes: Uint8Array): void;
  resize(cols: number, rows: number): void;
  end(end: api.TermEnd): void;
  state(inputHeld: boolean): void;
  refused(reason: api.InputRefusal): void;
}

export interface TermView {
  readonly view: number;
  readonly mode: api.ViewMode;
  /** Types bytes (Ctrl+C sets the `INPUT` flag); false when offline or the view ended. */
  input(bytes: Uint8Array<ArrayBuffer>, ctrlC: boolean): boolean;
  /** Control views only. */
  resize(cols: number, rows: number): boolean;
  detach(): void;
}

type TermRecord = Extract<AppRecord, { kind: 't_snapshot' | 't_output' | 't_resize' | 't_end' }>;

interface ViewEntry {
  sink: ViewSink;
  mode: api.ViewMode;
}

// ---- internals ----------------------------------------------------------------------------------

interface Waiter {
  resolve(resp: api.Resp): void;
  reject(error: ClientError): void;
}

interface OpenRequest {
  view: RequestView;
  resolve(view: RequestView): void;
  /** A scheduled outcome lookup, if any. */
  poll: number | null;
}

const EMPTY_FEEDS: ReadonlyMap<string, api.FeedValue> = new Map();
const EMPTY_TRANSCRIPTS: ReadonlyMap<string, TranscriptState> = new Map();

function statusOf(outcome: api.OutcomeView): RequestStatus {
  return outcome.state === 'unknown' ? 'unknown' : outcome.state;
}

export class Client {
  readonly #deps: ClientDeps;
  #state: ClientState;
  readonly #listeners = new Set<() => void>();

  #stored: StoredPairing | null = null;
  #link: Link | null = null;
  /** Bumped per dial; events of an older link are ignored. */
  #linkId = 0;
  #sessionOpened = false;
  /** Set by `stop`: no link is opened any more (a connect already waiting on the key store gives up). */
  #stopped = false;
  #retryTimer: number | null = null;
  #rateLimitedSince: number | null = null;

  #builder: api.RequestBuilder | null = null;
  #ack = new AckPolicy();
  #acked = 0;
  #unackedBytes = 0;
  #ackTimer: number | null = null;

  readonly #waiters = new Map<number, Waiter>();
  /** Journaled requests without a final outcome, by seq. */
  readonly #open = new Map<number, OpenRequest>();
  #recent: RequestView[] = [];
  readonly #subs = new Map<string, { feed: api.Feed; refs: number }>();
  readonly #views = new Map<number, ViewEntry>();
  readonly #orphans = new Map<number, TermRecord[]>();
  /** The latest `term_state` of views not yet registered. */
  readonly #orphanStates = new Map<number, boolean>();
  #noticeId = 0;
  /** What `presence` last reported (the page's visibility); re-sent after every Welcome. */
  #visible = true;

  constructor(deps: ClientDeps) {
    this.#deps = deps;
    this.#state = {
      conn: { phase: 'loading', attempt: 0, retryAt: null, stop: null, lastClose: null, everOnline: false },
      pairing: null,
      pairingFlow: null,
      welcome: null,
      feeds: EMPTY_FEEDS,
      transcripts: EMPTY_TRANSCRIPTS,
      requests: [],
      confirms: [],
      models: null,
      notices: [],
    };
  }

  // ---- store ----------------------------------------------------------------------------------

  readonly subscribeState = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  readonly getState = (): ClientState => this.#state;

  #set(patch: Partial<ClientState>): void {
    this.#state = { ...this.#state, ...patch };
    for (const listener of this.#listeners) listener();
  }

  #setConn(patch: Partial<ConnState>): void {
    this.#set({ conn: { ...this.#state.conn, ...patch } });
  }

  #publishRequests(): void {
    const open = [...this.#open.values()].map((entry) => entry.view).sort((a, b) => b.seq - a.seq);
    this.#set({ requests: [...open, ...this.#recent] });
  }

  notify(text: string, tone: Notice['tone'] = 'info'): void {
    const notice = { id: ++this.#noticeId, text, tone };
    this.#set({ notices: [...this.#state.notices, notice].slice(-MAX_NOTICES) });
  }

  dismissNotice(id: number): void {
    this.#set({ notices: this.#state.notices.filter((notice) => notice.id !== id) });
  }

  /** Whether the app is online with a Welcome. */
  get online(): boolean {
    return this.#state.conn.phase === 'online' && this.#state.welcome !== null;
  }

  /** Whether `op` may be sent now: online and permitted (every mutation is disabled offline). */
  canSend(op: api.Op): boolean {
    const welcome = this.#state.welcome;
    return this.online && welcome !== null && api.opAllowed(op, welcome.permissions) && (hasS4(welcome) || !isS4Op(op));
  }

  /** Journaled requests still waiting for their final outcome. */
  get openRequests(): number {
    return this.#open.size;
  }

  // ---- lifecycle --------------------------------------------------------------------------------

  /** Loads the stored pairing and connects; without one the app shows pairing. */
  async start(): Promise<void> {
    let stored: StoredPairing | null = null;
    try {
      stored = await this.#deps.store.load();
    } catch (error) {
      this.notify(`Could not read this phone’s pairing: ${String(error)}`, 'error');
    }
    this.#adopt(stored);
    if (stored === null) {
      this.#setConn({ phase: 'unpaired' });
      return;
    }
    void this.#connect();
  }

  #adopt(stored: StoredPairing | null): void {
    this.#stored = stored;
    if (stored === null) {
      this.#set({ pairing: null });
      return;
    }
    const keypair = Keypair.fromSecret(stored.secret);
    const summary: PairingSummary = {
      device: stored.device,
      gateway: stored.gateway,
      url: stored.url,
      name: stored.name,
      pairedUnixMs: stored.pairedUnixMs,
      fingerprint: fingerprint(keypair.public),
      gatewayFingerprint: fingerprint(stored.gatewayPublic),
    };
    keypair.wipe();
    this.#set({ pairing: summary });
  }

  /** Reconnects at once (a user action after a stop, or a wake-up while waiting). */
  reconnectNow(): void {
    const phase = this.#state.conn.phase;
    if (this.#stored === null || phase === 'online' || phase === 'connecting') return;
    this.#clearRetry();
    this.#rateLimitedSince = null;
    void this.#connect();
  }

  /** A hint that the network or the page came back: retry now if a retry is pending. */
  wake(): void {
    if (this.#state.conn.phase === 'waiting') this.reconnectNow();
  }

  /**
   * Stops notifications for this pairing: tells the desktop when online (best effort, bounded),
   * then always drops the browser's own push subscription and key.
   */
  async dropPush(): Promise<void> {
    if (this.online) {
      const timeout = Promise.withResolvers<void>();
      const timer = this.#deps.clock.setTimer(() => timeout.resolve(), PUSH_DROP_WAIT_MS);
      await Promise.race([this.request({ op: 'push_unsubscribe' }).then(() => undefined, () => undefined), timeout.promise]);
      this.#deps.clock.clearTimer(timer);
    }
    await this.#deps.dropPush?.().catch((error: unknown) => {
      this.notify(`Could not remove this browser's push subscription: ${error instanceof Error ? error.message : String(error)}`, 'warn');
    });
  }

  /** Forgets this phone's pairing (user action). The desktop keeps the device until revoked there. */
  async forget(): Promise<void> {
    await this.dropPush();
    this.#disconnect();
    await this.#deps.store.clear();
    this.#adopt(null);
    this.#failOpenRequests();
    this.#set({
      welcome: null,
      feeds: EMPTY_FEEDS,
      transcripts: EMPTY_TRANSCRIPTS,
      confirms: [],
      models: null,
    });
    this.#setConn({ phase: 'unpaired', attempt: 0, retryAt: null, stop: null, lastClose: null });
  }

  /**
   * Ends this client for good (the app switched to another desktop): closes the link cleanly and
   * never dials again. The stored pairing stays; a new `Client` serves the desktop when it is
   * chosen again.
   */
  stop(): void {
    this.#stopped = true;
    this.#disconnect();
    this.#failOpenRequests();
  }

  /** Closes the current link on purpose: its late events are ignored. */
  #disconnect(): void {
    this.#clearRetry();
    const link = this.#link;
    if (link === null) return;
    this.#linkId++;
    this.#teardown();
    link.close(CLOSE.NORMAL);
  }

  #clearRetry(): void {
    if (this.#retryTimer !== null) this.#deps.clock.clearTimer(this.#retryTimer);
    this.#retryTimer = null;
  }

  async #connect(): Promise<void> {
    const stored = this.#stored;
    if (stored === null || this.#stopped) return;
    this.#setConn({ phase: 'connecting', retryAt: null, stop: null });
    let ts: number;
    try {
      ts = await this.#deps.store.reserveTimestamp(this.#deps.clock.now());
    } catch {
      // The key is gone (evicted or forgotten elsewhere): a missing key means re-pair.
      this.#adopt(null);
      this.#setConn({ phase: 'unpaired' });
      return;
    }
    if (this.#stopped) return;
    const id = ++this.#linkId;
    this.#sessionOpened = false;
    this.#ack = new AckPolicy();
    this.#acked = 0;
    this.#unackedBytes = 0;
    const hello: DeviceHello = { name: stored.name, platform: this.#deps.platform, ts, features: DEVICE_FEATURES };
    this.#link = this.#deps.dial(
      { url: stored.url, room: stored.room, token: stored.token, secret: stored.secret, gatewayPublic: stored.gatewayPublic, hello },
      {
        open: () => {
          if (id === this.#linkId) this.#sessionOpened = true;
        },
        message: (records, size) => {
          if (id === this.#linkId) this.#onMessage(records, size);
        },
        closed: (info) => {
          if (id === this.#linkId) this.#onClosed(info);
        },
      },
    );
  }

  /** Drops everything bound to the current link; returns whether its session had opened. */
  #teardown(): boolean {
    this.#link = null;
    this.#builder = null;
    if (this.#ackTimer !== null) this.#deps.clock.clearTimer(this.#ackTimer);
    this.#ackTimer = null;
    for (const waiter of this.#waiters.values()) {
      waiter.reject(new ClientError('disconnected', 'The connection to the desktop was lost.'));
    }
    this.#waiters.clear();
    for (const [view, entry] of this.#views) {
      entry.sink.end({ reason: 'disconnected' });
      this.#views.delete(view);
    }
    this.#orphans.clear();
    this.#orphanStates.clear();
    for (const entry of this.#open.values()) {
      if (entry.poll !== null) this.#deps.clock.clearTimer(entry.poll);
      entry.poll = null;
    }
    const opened = this.#sessionOpened;
    this.#sessionOpened = false;
    return opened;
  }

  #onClosed(info: LinkClose): void {
    const opened = this.#teardown();
    const now = this.#deps.clock.now();
    const attempt = opened ? 0 : this.#state.conn.attempt;
    if (info.code === CLOSE.RATE_LIMITED) this.#rateLimitedSince ??= now;
    else this.#rateLimitedSince = null;
    const decision = decideRetry(info.code, opened, {
      attempt,
      rateLimitedSince: this.#rateLimitedSince,
      now,
      random: this.#deps.random,
    });
    if (!decision.retry) {
      this.#setConn({ phase: 'stopped', stop: decision.stop, lastClose: info, retryAt: null, attempt: attempt + 1 });
      return;
    }
    this.#setConn({ phase: 'waiting', lastClose: info, retryAt: now + decision.delayMs, attempt: attempt + 1 });
    this.#retryTimer = this.#deps.clock.setTimer(() => {
      this.#retryTimer = null;
      void this.#connect();
    }, decision.delayMs);
  }

  // ---- pairing ----------------------------------------------------------------------------------

  /**
   * Pairs with the desktop whose invite is `inviteText` (the `#pair=` fragment), with a fresh key.
   * Resolves true once paired and connecting.
   */
  async pair(inviteText: string, name: string): Promise<boolean> {
    let invite: PairingInvite;
    try {
      invite = parsePairingInvite(inviteText);
    } catch (error) {
      this.notify(`This pairing link is not valid: ${error instanceof Error ? error.message : String(error)}`, 'error');
      return false;
    }
    const keypair = Keypair.generate();
    const flow: PairingFlow = {
      stage: 'connecting',
      name,
      fingerprint: fingerprint(keypair.public),
      gatewayFingerprint: fingerprint(invite.gatewayPublic),
      relay: invite.url,
      error: null,
    };
    this.#set({ pairingFlow: flow });
    const ts = this.#deps.clock.now();
    try {
      const result = await this.#deps.pair(
        invite,
        keypair,
        { name, platform: this.#deps.platform, ts, features: DEVICE_FEATURES },
        (stage) => this.#set({ pairingFlow: { ...flow, stage } }),
      );
      const stored: StoredPairing = {
        secret: keypair.secretBytes(),
        gatewayPublic: invite.gatewayPublic,
        url: invite.url,
        room: invite.room,
        token: result.token,
        device: result.device,
        gateway: result.gateway,
        name,
        last: ts,
        pairedUnixMs: ts,
      };
      this.#disconnect();
      await this.#deps.store.save(stored);
      stored.secret.fill(0);
      void this.#deps.persistStorage().catch(() => false);
      const saved = await this.#deps.store.load();
      this.#adopt(saved);
      this.#set({ pairingFlow: null, welcome: null, feeds: EMPTY_FEEDS, transcripts: EMPTY_TRANSCRIPTS });
      this.#setConn({ attempt: 0, stop: null, lastClose: null });
      void this.#connect();
      return true;
    } catch (error) {
      this.#set({
        pairingFlow: { ...flow, stage: 'failed', error: error instanceof Error ? error.message : String(error) },
      });
      return false;
    } finally {
      keypair.wipe();
    }
  }

  dismissPairing(): void {
    this.#set({ pairingFlow: null });
  }

  // ---- inbound ----------------------------------------------------------------------------------

  #onMessage(records: AppRecord[], size: number): void {
    for (const record of records) {
      try {
        this.#onRecord(record);
      } catch (error) {
        // An undecodable gateway JSON body: the gateway broke the protocol; close and reconnect.
        this.notify(`The desktop sent something this app cannot read (${String(error)}).`, 'error');
        this.#link?.close(CLOSE.APP_PROTOCOL);
        return;
      }
    }
    this.#unackedBytes += size;
    const now = this.#deps.clock.now();
    let due = this.#ack.onMessage(size, now);
    const limits = this.#state.welcome?.limits;
    if (due === null && limits !== undefined) {
      const tightMessages = (this.#ack.processed - this.#acked) * 2 >= limits.window_messages;
      const tightBytes = this.#unackedBytes * 2 >= Math.min(limits.window_bytes, ACK_EVERY_BYTES * 2);
      if (tightMessages || tightBytes) due = this.#ack.flush();
    }
    if (due !== null) {
      this.#sendAck(due);
      return;
    }
    this.#scheduleAck();
  }

  #sendAck(messages: number): void {
    this.#acked = messages;
    this.#unackedBytes = 0;
    if (this.#ackTimer !== null) this.#deps.clock.clearTimer(this.#ackTimer);
    this.#ackTimer = null;
    this.#link?.send([{ kind: 'ack', messages }]);
  }

  #scheduleAck(): void {
    if (this.#ackTimer !== null) return;
    const deadline = this.#ack.deadline();
    if (deadline === null) return;
    this.#ackTimer = this.#deps.clock.setTimer(
      () => {
        this.#ackTimer = null;
        const due = this.#ack.due(this.#deps.clock.now());
        if (due !== null) this.#sendAck(due);
        else this.#scheduleAck();
      },
      Math.max(0, deadline - this.#deps.clock.now()),
    );
  }

  #onRecord(record: AppRecord): void {
    switch (record.kind) {
      case 'welcome':
        this.#onWelcome(api.decodeWelcome(record.body));
        return;
      case 'reply':
        this.#onReply(api.decodeReply(record.body));
        return;
      case 'event':
        this.#onEvent(api.decodeEvent(record.body));
        return;
      case 't_snapshot':
      case 't_output':
      case 't_resize':
      case 't_end':
        this.#onTerm(record);
        return;
      default:
        // Device record kinds never arrive (the decoder refuses them).
        return;
    }
  }

  #onWelcome(welcome: api.Welcome): void {
    const previous = this.#state.welcome;
    this.#builder = new api.RequestBuilder(welcome);
    const epochChanged = previous !== null && previous.epoch !== welcome.epoch;
    this.#set({
      welcome,
      // Live refs are bound to the gateway epoch: a new epoch invalidates every live view.
      confirms: epochChanged ? [] : this.#state.confirms,
      models: epochChanged ? null : this.#state.models,
    });
    this.#setConn({ phase: 'online', attempt: 0, retryAt: null, stop: null, everOnline: true });
    this.#rateLimitedSince = null;
    for (const { feed } of this.#subs.values()) this.#sendSubscribe(feed);
    this.#sendPresence();
    this.#recoverOutcomes();
  }

  #onReply(reply: api.Reply): void {
    const waiter = this.#waiters.get(reply.id);
    if (waiter === undefined) return;
    this.#waiters.delete(reply.id);
    if ('ok' in reply) waiter.resolve(reply.ok);
    else waiter.reject(new ClientError('api', describeApiError(reply.err), reply.err));
  }

  #onEvent(event: api.Event): void {
    switch (event.type) {
      case 'feed':
        this.#onFeed(event.feed, event.value);
        return;
      case 'outcome':
        this.#applyOutcome(event.seq, event.outcome);
        return;
      case 'progress': {
        const entry = this.#open.get(event.seq);
        if (entry === undefined) return;
        entry.view = { ...entry.view, progress: { stage: event.stage, done: event.done, total: event.total } };
        this.#publishRequests();
        return;
      }
      case 'confirm': {
        const others = this.#state.confirms.filter(
          (confirm) => !(confirm.live.key === event.live.key && confirm.request === event.request),
        );
        this.#set({
          confirms: [...others, { live: event.live, request: event.request, label: event.label, draft: event.draft }],
        });
        return;
      }
      case 'term_state': {
        const entry = this.#views.get(event.view);
        // Arrives packed with the `attached` reply, before the view registers: keep the latest.
        if (entry === undefined) this.#orphanStates.set(event.view, event.input_held);
        else entry.sink.state(event.input_held);
        return;
      }
      case 'input_refused':
        this.#views.get(event.view)?.sink.refused(event.reason);
        return;
      case 'model_unconfirmed':
        this.notify(
          `omp did not confirm the switch to ${event.selector ?? 'the model'}${event.level === null ? '' : ` (${event.level})`}.`,
          'warn',
        );
        return;
      case 'resync':
        this.#recoverOutcomes();
        return;
      default:
        // `unknown_event`: a newer gateway's addition.
        return;
    }
  }

  #onFeed(feed: api.Feed, value: api.FeedValue): void {
    const key = feedKey(feed);
    if (!this.#subs.has(key)) return;
    if (value.feed === 'transcript') {
      const current = this.#state.transcripts.get(key);
      let next: TranscriptState;
      if (value.reset || current === undefined) {
        next = { start: value.start, items: value.items };
      } else {
        const offset = value.start - current.start;
        if (offset < 0 || offset > current.items.length) {
          next = { start: value.start, items: value.items };
        } else {
          next = { start: current.start, items: [...current.items.slice(0, offset), ...value.items] };
        }
      }
      const transcripts = new Map(this.#state.transcripts);
      transcripts.set(key, next);
      this.#set({ transcripts });
      return;
    }
    if (value.feed === 'live') this.#reconcileConfirms(value.sessions);
    const feeds = new Map(this.#state.feeds);
    feeds.set(key, value);
    this.#set({ feeds });
  }

  /**
   * Re-learns pending draft confirmations from `live` (a reconnecting device missed their
   * events) and drops the ones no longer awaiting an answer.
   */
  #reconcileConfirms(sessions: readonly api.LiveSession[]): void {
    const confirms: ConfirmView[] = [];
    for (const session of sessions) {
      for (const own of session.requests) {
        if (own.delivery.state !== 'awaiting_confirm') continue;
        confirms.push({ live: session.live, request: own.request, label: own.label, draft: own.delivery.draft });
      }
    }
    const same =
      confirms.length === this.#state.confirms.length &&
      confirms.every((confirm, index) => {
        const other = this.#state.confirms[index];
        return other !== undefined && other.request === confirm.request && other.live.key === confirm.live.key;
      });
    if (!same) this.#set({ confirms });
  }

  #onTerm(record: TermRecord): void {
    const entry = this.#views.get(record.view);
    if (entry === undefined) {
      const queued = this.#orphans.get(record.view) ?? [];
      if (queued.length < MAX_ORPHAN_RECORDS) queued.push(record);
      this.#orphans.set(record.view, queued);
      return;
    }
    this.#deliverTerm(entry, record);
  }

  #deliverTerm(entry: ViewEntry, record: TermRecord): void {
    switch (record.kind) {
      case 't_snapshot':
        entry.sink.snapshot(record.bytes, record.last);
        return;
      case 't_output':
        entry.sink.output(record.bytes);
        return;
      case 't_resize':
        entry.sink.resize(record.cols, record.rows);
        return;
      case 't_end':
        this.#views.delete(record.view);
        entry.sink.end(api.decodeTermEnd(record.end));
        return;
    }
  }

  // ---- outbound ---------------------------------------------------------------------------------

  #send(records: AppRecord[]): void {
    const ack = this.#ack.flush();
    if (ack !== null) {
      this.#acked = ack;
      this.#unackedBytes = 0;
      records.unshift({ kind: 'ack', messages: ack });
    }
    this.#link?.send(records);
  }

  #build(op: api.Op): api.Request {
    if (!this.online || this.#builder === null) {
      throw new ClientError('offline', 'Offline: reconnect to the desktop first.');
    }
    const welcome = this.#state.welcome;
    if (welcome !== null && !api.opAllowed(op, welcome.permissions)) {
      throw new ClientError('not_allowed', `This phone lacks the ${api.opClass(op)} permission.`);
    }
    if (!hasS4(welcome) && isS4Op(op)) throw new ClientError('unsupported', NEEDS_S4);
    if (this.#waiters.size >= (welcome?.limits.max_inflight ?? api.MAX_INFLIGHT)) {
      throw new ClientError('busy', 'Too many requests at once; wait a moment.');
    }
    try {
      return this.#builder.build(op);
    } catch (error) {
      throw new ClientError('invalid', error instanceof Error ? error.message : String(error));
    }
  }

  #dispatch(request: api.Request): Promise<api.Resp> {
    const { promise, resolve, reject } = Promise.withResolvers<api.Resp>();
    this.#waiters.set(request.id, { resolve, reject });
    this.#send([api.requestRecord(request)]);
    return promise;
  }

  /** A non-journaled request (View ops, `attach`, `detach`). Refused at once when offline. */
  request(op: api.Op): Promise<api.Resp> {
    try {
      return this.#dispatch(this.#build(op));
    } catch (error) {
      return Promise.reject(error);
    }
  }

  /**
   * A journaled mutation. Resolves with the request's final state (it may take a reconnect and an
   * outcome lookup); rejects at once when it cannot be sent (offline, not permitted).
   */
  mutate(op: api.Op, label: string): Promise<RequestView> {
    let request: api.Request;
    try {
      request = this.#build(op);
    } catch (error) {
      return Promise.reject(error);
    }
    const seq = request.seq;
    if (seq === null || seq === undefined) return Promise.reject(new ClientError('invalid', `${op.op} is not journaled`));
    const { promise, resolve } = Promise.withResolvers<RequestView>();
    const view: RequestView = {
      seq,
      label,
      op,
      status: 'sending',
      outcome: null,
      error: null,
      progress: null,
      startedUnixMs: this.#deps.clock.now(),
    };
    this.#open.set(seq, { view, resolve, poll: null });
    this.#publishRequests();
    this.#dispatch(request).then(
      (resp) => {
        const entry = this.#open.get(seq);
        if (entry === undefined || resp.type !== 'accepted') return;
        if (entry.view.status === 'sending') {
          entry.view = { ...entry.view, status: 'accepted' };
          this.#publishRequests();
        }
      },
      (error: unknown) => {
        if (error instanceof ClientError && error.kind === 'api' && error.api !== null) {
          this.#finish(seq, { status: 'error', error: error.api });
        }
        // `disconnected`: the outcome is looked up after the reconnect.
      },
    );
    return promise;
  }

  /** Sends a request whose outcome was `unknown` (it never reached the gateway) again, with a new seq. */
  resend(seq: number): Promise<RequestView> | null {
    const index = this.#recent.findIndex((view) => view.seq === seq && view.status === 'unknown');
    const previous = this.#recent[index];
    if (previous === undefined) return null;
    this.#recent = this.#recent.filter((_, i) => i !== index);
    this.#publishRequests();
    return this.mutate(previous.op, previous.label);
  }

  dismissRequest(seq: number): void {
    this.#recent = this.#recent.filter((view) => view.seq !== seq);
    this.#publishRequests();
  }

  #applyOutcome(seq: number, outcome: api.OutcomeView): void {
    const entry = this.#open.get(seq);
    if (entry === undefined) return;
    if (outcome.state === 'running') {
      entry.view = { ...entry.view, status: 'running', outcome };
      this.#publishRequests();
      return;
    }
    this.#finish(seq, { status: statusOf(outcome), outcome });
  }

  #finish(seq: number, patch: Partial<RequestView>): void {
    const entry = this.#open.get(seq);
    if (entry === undefined) return;
    if (entry.poll !== null) this.#deps.clock.clearTimer(entry.poll);
    this.#open.delete(seq);
    const view = { ...entry.view, ...patch };
    this.#recent = [view, ...this.#recent].slice(0, RECENT_REQUESTS);
    if (view.op.op === 'answer') {
      const { live, request } = view.op;
      this.#set({
        confirms: this.#state.confirms.filter((c) => !(c.live.key === live.key && c.request === request)),
      });
    }
    this.#publishRequests();
    entry.resolve(view);
  }

  #failOpenRequests(): void {
    for (const seq of [...this.#open.keys()]) {
      this.#finish(seq, { status: 'unknown' });
    }
  }

  /** Looks up the outcome of every open journaled request (after a reconnect or a `resync`). */
  #recoverOutcomes(): void {
    for (const seq of this.#open.keys()) this.#lookup(seq);
  }

  #lookup(seq: number): void {
    const entry = this.#open.get(seq);
    if (entry === undefined || !this.online) return;
    if (entry.poll !== null) this.#deps.clock.clearTimer(entry.poll);
    entry.poll = null;
    this.request({ op: 'outcome', seq }).then(
      (resp) => {
        if (resp.type !== 'outcome') return;
        this.#applyOutcome(seq, resp.outcome);
        const still = this.#open.get(seq);
        if (still !== undefined && still.poll === null) {
          still.poll = this.#deps.clock.setTimer(() => {
            const current = this.#open.get(seq);
            if (current !== undefined) current.poll = null;
            this.#lookup(seq);
          }, OUTCOME_POLL_MS);
        }
      },
      (error: unknown) => {
        if (error instanceof ClientError && error.kind === 'api' && error.api !== null) {
          this.#finish(seq, { status: error.api.code === 'expired' ? 'expired' : 'error', error: error.api });
        }
      },
    );
  }

  // ---- feeds ------------------------------------------------------------------------------------

  /**
   * Follows `feed` while the returned function has not been called (reference counted). Survives
   * reconnects: every subscription is re-sent after each Welcome. Returns null when the
   * gateway's `max_feeds` would be exceeded.
   */
  follow(feed: api.Feed): (() => void) | null {
    const key = feedKey(feed);
    const existing = this.#subs.get(key);
    if (existing !== undefined) {
      existing.refs++;
    } else {
      const max = this.#state.welcome?.limits.max_feeds ?? api.MAX_FEEDS;
      if (this.#subs.size >= max) return null;
      this.#subs.set(key, { feed, refs: 1 });
      if (this.online) this.#sendSubscribe(feed);
    }
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.#release(key);
    };
  }

  #release(key: string): void {
    const sub = this.#subs.get(key);
    if (sub === undefined) return;
    sub.refs--;
    if (sub.refs > 0) return;
    this.#subs.delete(key);
    if (this.online) {
      this.request({ op: 'unsubscribe', feed: sub.feed }).catch(() => undefined);
    }
    if (this.#state.transcripts.has(key)) {
      const transcripts = new Map(this.#state.transcripts);
      transcripts.delete(key);
      this.#set({ transcripts });
    }
  }

  #sendSubscribe(feed: api.Feed): void {
    // An older desktop does not know slice-4 feeds: the subscription stays, unsent (and valueless).
    if (!this.s4 && isS4Feed(feed)) return;
    this.request({ op: 'subscribe', feed }).catch((error: unknown) => {
      if (error instanceof ClientError && error.kind === 'api') this.notify(error.message, 'warn');
    });
  }

  /** Whether the connected gateway speaks slice 4 (false offline before the first Welcome). */
  get s4(): boolean {
    return hasS4(this.#state.welcome);
  }

  /**
   * Reports the page's visibility (`document.visibilityState === 'visible'`): the desktop skips
   * push notifications while a connected device is visible. Remembered and re-sent after every
   * reconnect.
   */
  setVisible(visible: boolean): void {
    if (this.#visible === visible) return;
    this.#visible = visible;
    this.#sendPresence();
  }

  /** Resolves once the app is online with a Welcome; rejects with an `AbortError` when `signal` aborts. */
  whenOnline(signal?: AbortSignal): Promise<void> {
    if (this.online) return Promise.resolve();
    const { promise, resolve, reject } = Promise.withResolvers<void>();
    const done = () => {
      unsubscribe();
      signal?.removeEventListener('abort', onAbort);
    };
    const onAbort = () => {
      done();
      reject(new DOMException('Cancelled while offline.', 'AbortError'));
    };
    const unsubscribe = this.subscribeState(() => {
      if (!this.online) return;
      done();
      resolve();
    });
    if (signal?.aborted) onAbort();
    else signal?.addEventListener('abort', onAbort);
    return promise;
  }

  #sendPresence(): void {
    if (!this.online || !this.s4) return;
    this.request({ op: 'presence', visible: this.#visible }).catch(() => undefined);
  }

  /** The model list (cached per gateway epoch). */
  async loadModels(force = false): Promise<void> {
    if (!force && this.#state.models !== null) return;
    const resp = await this.request({ op: 'models' });
    if (resp.type === 'models') this.#set({ models: { models: resp.models, truncated: resp.truncated } });
  }

  // ---- terminal views ---------------------------------------------------------------------------

  /** Attaches a terminal view; `sink` receives its snapshot, output and end. */
  async attach(live: api.LiveRef, mode: api.ViewMode, sink: ViewSink): Promise<TermView> {
    const resp = await this.request({ op: 'attach', live, mode });
    if (resp.type !== 'attached') throw new ClientError('invalid', 'unexpected answer to attach');
    const view = resp.view;
    const entry: ViewEntry = { sink, mode: resp.mode };
    this.#views.set(view, entry);
    const queued = this.#orphans.get(view);
    this.#orphans.delete(view);
    const held = this.#orphanStates.get(view);
    this.#orphanStates.delete(view);
    for (const record of queued ?? []) {
      if (!this.#views.has(view)) break;
      this.#deliverTerm(entry, record);
    }
    if (held !== undefined && this.#views.has(view)) entry.sink.state(held);
    return {
      view,
      mode: resp.mode,
      input: (bytes, ctrlC) => {
        if (!this.online || !this.#views.has(view) || entry.mode === 'observe') return false;
        if (bytes.length === 0 && !ctrlC) return true;
        const records: AppRecord[] = [];
        if (bytes.length === 0) records.push({ kind: 'input', view, ctrlC, bytes });
        for (let offset = 0; offset < bytes.length; offset += MAX_INPUT) {
          records.push({ kind: 'input', view, ctrlC, bytes: bytes.subarray(offset, offset + MAX_INPUT) });
        }
        this.#send(records);
        return true;
      },
      resize: (cols, rows) => {
        if (!this.online || !this.#views.has(view) || entry.mode !== 'control') return false;
        this.#send([{ kind: 'resize', view, cols, rows }]);
        return true;
      },
      detach: () => {
        if (!this.#views.delete(view)) return;
        this.#orphans.delete(view);
        this.#orphanStates.delete(view);
        if (this.online) this.request({ op: 'detach', view }).catch(() => undefined);
      },
    };
  }
}
