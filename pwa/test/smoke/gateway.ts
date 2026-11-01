// The smoke gateway's application side: answers device requests from the fixtures, keeps
// subscriptions per connection, journals mutations (accepted → progress → outcome events) and
// pushes feed updates the way the real gateway does. Transport-free: `serve.ts` feeds it decoded
// records and sends what it emits; the unit test drives it directly.

import { type AppRecord, api } from '../../src/remote/index.ts';
import { feedKey, isS4Op } from '../../src/app/client.ts';
import * as fx from './fixtures.ts';

const MIB = 1024 * 1024;
const utf8 = (text: string): Uint8Array<ArrayBuffer> => new TextEncoder().encode(text) as Uint8Array<ArrayBuffer>;
const toB64 = (bytes: Uint8Array): string => Buffer.from(bytes).toString('base64');

export interface GatewayOptions {
  /** Whether the gateway announces slice 4 (`Welcome.features`). */
  s4: boolean;
  /** The PNG served for transcript images. */
  png: Uint8Array;
  /** Web Push VAPID public key (base64url), or none. */
  pushKey: string | null;
  /** Delay of each `upload_chunk` answer, ms. */
  uploadDelayMs: number;
  /** Delay before a journaled op's outcome, ms. */
  outcomeDelayMs: number;
  /** The version the gateway announces (null: the default for its variant). */
  version: string | null;
}

export const DEFAULT_OPTIONS: GatewayOptions = {
  s4: true,
  png: new Uint8Array(0),
  pushKey: null,
  uploadDelayMs: 700,
  outcomeDelayMs: 450,
  version: null,
};

/** What a connection sends to its device. */
export type Sender = (records: AppRecord[]) => void;

interface Download {
  bytes: Uint8Array;
}

interface Upload {
  name: string;
  size: number;
  received: number;
}

/** Shared desktop state, the same for every connection. */
export class SmokeGateway {
  readonly opts: GatewayOptions;
  readonly t0 = Date.now();
  readonly epoch = '0badc0ffee0ddba11fee1dead5eed000';
  readonly #conns = new Set<SmokeConnection>();

  /** Device connections open right now (the e2e run checks that only the active desktop has one). */
  get connections(): number {
    return this.#conns.size;
  }

  nextSeq = 100;
  readonly outcomes = new Map<number, api.OutcomeView>();
  readonly pendingUi = new Map<string, api.UiRequestView | null>(fx.LIVE.map((spec) => [spec.session, spec.ui]));
  readonly activity = new Map<string, fx.LiveSpec['state']>(fx.LIVE.map((spec) => [spec.session, spec.state]));
  readonly extraItems = new Map<string, api.Item[]>();
  readonly deleted = new Set<string>();
  readonly killed = new Set<string>();
  readonly staged = new Map<string, Set<string>>();
  readonly downloads = new Map<string, Download>();
  readonly uploads = new Map<string, Upload>();
  readonly finishedUploads = new Map<string, string>();
  #handles = 0;
  deviceName = 'Pixel 9';
  push: api.PushStateView = { subscribed: false, prefs: fx.DEFAULT_PREFS, since_unix_ms: null };
  appPhase: api.UpdatePhase = { state: 'idle' };
  ompChecking = false;
  backupBusy: string | null = null;
  backupDestination: api.BackupDestinationView = { kind: 'drive', label: 'Google Drive' };
  /** Controlled listing replies let browser tests hold an old destination response in flight. */
  backupListing: ((op: Extract<api.Op, { op: 'backup_list' | 'sync_backups' }>) => api.Resp | Promise<api.Resp>) | null = null;
  syncBusy: string | null = null;
  syncRows: api.SyncRowView[];
  usageFetching = false;
  /** Everything typed into any terminal view (the e2e run checks it). */
  terminalInput = '';

  constructor(opts: Partial<GatewayOptions> = {}) {
    this.opts = { ...DEFAULT_OPTIONS, ...opts };
    this.syncRows = fx.syncRows(this.t0);
  }

  connect(send: Sender, device: string, name: string): SmokeConnection {
    const conn = new SmokeConnection(this, send);
    this.#conns.add(conn);
    if (name.length > 0) this.deviceName = name;
    const welcome: api.Welcome = {
      protocol: api.PROTOCOL,
      gateway_version: this.opts.version ?? (this.opts.s4 ? '0.10.0' : '0.9.2'),
      epoch: this.epoch,
      device,
      name: this.deviceName,
      permissions: { control: true, manage: true },
      next_seq: this.nextSeq,
      limits: {
        max_message: api.MAX_REPLY + 5,
        window_bytes: api.WINDOW_BYTES,
        window_messages: api.WINDOW_MESSAGES,
        max_inflight: api.MAX_INFLIGHT,
        max_views: api.MAX_VIEWS,
        max_feeds: api.MAX_FEEDS,
      },
      features: this.opts.s4 ? [api.FEATURE_S4] : [],
    };
    if (this.opts.s4 && this.opts.pushKey !== null) welcome.push_key = this.opts.pushKey;
    send([{ kind: 'welcome', body: utf8(JSON.stringify(welcome)) }]);
    return conn;
  }

  disconnect(conn: SmokeConnection): void {
    this.#conns.delete(conn);
  }

  now(): number {
    return Date.now();
  }

  handle(prefix: string): string {
    return `${prefix}-${(++this.#handles).toString(16).padStart(6, '0')}`;
  }

  /** Re-sends every subscribed feed of `kind` to every connection. */
  changed(...kinds: api.Feed['feed'][]): void {
    for (const conn of this.#conns) conn.refresh(kinds);
  }

  liveSessions(): api.LiveSession[] {
    return fx.LIVE.filter((spec) => !this.deleted.has(spec.session)).map((spec) =>
      fx.liveSession({ ...spec, ui: this.pendingUi.get(spec.session) ?? null, state: this.activity.get(spec.session) ?? spec.state }),
    );
  }

  specByLive(live: api.LiveRef): fx.LiveSpec | undefined {
    return fx.LIVE.find((spec) => fx.liveRef(spec).key === live.key);
  }

  items(session: string, agent: string | null): api.Item[] {
    return [...fx.transcript(this.t0, session, agent), ...(agent === null ? (this.extraItems.get(session) ?? []) : [])];
  }

  feedValue(feed: api.Feed): api.FeedValue | null {
    const now = this.t0;
    switch (feed.feed) {
      case 'projects':
        return { feed: 'projects', projects: fx.projects(now), truncated: false };
      case 'sessions': {
        const value = fx.sessions(now, feed.project, feed.limit);
        value.sessions = value.sessions.filter((s) => !this.deleted.has(s.id));
        return value;
      }
      case 'live':
        return { feed: 'live', sessions: this.liveSessions(), truncated: false };
      case 'inbox':
        return { feed: 'inbox', inbox: fx.inbox(now), truncated: false };
      case 'cards':
        return { feed: 'cards', cards: feed.sessions.map((s) => fx.card(s)), truncated: false };
      case 'transcript': {
        const items = this.items(feed.session, feed.agent ?? null);
        const start = Math.max(0, items.length - feed.tail);
        return { feed: 'transcript', start, items: items.slice(start), reset: true };
      }
      case 'changes': {
        const value = fx.changes(feed.project);
        const staged = this.staged.get(feed.project);
        if (value.changes !== null && staged !== undefined) {
          value.changes.files = value.changes.files.map((f) => (staged.has(f.path) ? { ...f, staged: f.unstaged ?? f.staged ?? 'added', unstaged: null, untracked: false } : f));
        }
        return value;
      }
      case 'running': {
        const value = fx.running(now);
        value.sessions = value.sessions.filter((r) => !this.killed.has(r.id));
        return value;
      }
      case 'inventory': {
        const value = fx.inventory(now, feed.project ?? null, feed.limit);
        value.entries = value.entries.filter((e) => !this.deleted.has(e.id));
        return value;
      }
      case 'context': {
        const spec = this.specByLive(feed.live);
        return { feed: 'context', live: feed.live, context: spec === undefined ? null : fx.context(spec, this.now()) };
      }
      case 'usage':
        return { ...fx.usage(now), fetching: this.usageFetching };
      case 'agents':
        return { feed: 'agents', session: feed.session, agents: fx.agents(now, feed.session), truncated: false };
      case 'worktrees':
        return fx.worktrees(now, feed.project);
      case 'backup':
        return { ...fx.backupFeed(now, this.backupBusy), destination: this.backupDestination };
      case 'sync':
        return { feed: 'sync', rows: this.syncRows, interval: 'hourly', busy: this.syncBusy, truncated: false };
      case 'updates':
        return fx.updates(now, this.appPhase, this.ompChecking);
      case 'device':
        return {
          feed: 'device',
          name: this.deviceName,
          permissions: { control: true, manage: true },
          push: this.push,
          gateway_version: this.opts.version ?? (this.opts.s4 ? '0.10.0' : '0.9.2'),
          mode: 'background',
        };
    }
  }

  /** Registers a download of `bytes`; answers its handle. */
  download(name: string, mime: string, bytes: Uint8Array): api.Resp {
    const download = this.handle('dl');
    this.downloads.set(download, { bytes });
    return { type: 'download', download, name, mime, size: bytes.length };
  }

  exportHtml(session: string): Uint8Array {
    const rows = this.items(session, null)
      .map((item) => `<article class="${item.type}"><pre>${'text' in item ? item.text.replace(/[<&]/g, (c) => (c === '<' ? '&lt;' : '&amp;')) : item.type}</pre></article>`)
      .join('\n');
    // Large enough to need several 512 KiB chunks, like a real export with inlined assets.
    const filler = `<!-- ${'omp export padding '.repeat(80_000)} -->`;
    return utf8(`<!doctype html><html><head><meta charset="utf-8"><title>${session}</title></head><body>${rows}${filler}</body></html>`);
  }
}

/** One device connection: its subscriptions and terminal views. */
export class SmokeConnection {
  readonly #gw: SmokeGateway;
  readonly #send: Sender;
  readonly #subs = new Map<string, api.Feed>();
  readonly #views = new Map<number, string>();
  #nextView = 1;
  closed = false;

  constructor(gateway: SmokeGateway, send: Sender) {
    this.#gw = gateway;
    this.#send = send;
  }

  close(): void {
    this.closed = true;
    this.#gw.disconnect(this);
  }

  #emit(records: AppRecord[]): void {
    if (!this.closed) this.#send(records);
  }

  #event(event: api.Event): AppRecord {
    return { kind: 'event', body: utf8(JSON.stringify(event)) };
  }

  #reply(id: number, result: { ok: api.Resp } | { err: api.ApiError }): void {
    this.#emit([{ kind: 'reply', body: utf8(JSON.stringify({ id, ...result })) }]);
  }

  #sendFeed(feed: api.Feed): void {
    const value = this.#gw.feedValue(feed);
    if (value !== null) this.#emit([this.#event({ type: 'feed', feed, value })]);
  }

  refresh(kinds: readonly api.Feed['feed'][]): void {
    for (const feed of this.#subs.values()) if (kinds.includes(feed.feed)) this.#sendFeed(feed);
  }

  /** Appends `items` to `session`'s transcript and tells its subscribers. */
  appendItems(session: string, items: api.Item[]): void {
    const extra = this.#gw.extraItems.get(session) ?? [];
    const start = this.#gw.items(session, null).length;
    this.#gw.extraItems.set(session, [...extra, ...items]);
    this.#gw.changed('live');
    // Every connection following that transcript gets the continuation.
    for (const feed of this.#subs.values()) {
      if (feed.feed === 'transcript' && feed.session === session && (feed.agent ?? null) === null) {
        this.#emit([this.#event({ type: 'feed', feed, value: { feed: 'transcript', start, items, reset: false } })]);
      }
    }
  }

  receive(records: readonly AppRecord[]): void {
    for (const record of records) {
      switch (record.kind) {
        case 'request':
          this.#request(JSON.parse(new TextDecoder().decode(record.body)) as api.Request);
          break;
        case 'input':
          this.#input(record.view, record.ctrlC, record.bytes);
          break;
        default:
          break;
      }
    }
  }

  #input(view: number, ctrlC: boolean, bytes: Uint8Array): void {
    if (!this.#views.has(view)) return;
    const text = new TextDecoder().decode(bytes);
    this.#gw.terminalInput += ctrlC ? '^C' : text;
    const echo = ctrlC ? '^C\r\n\x1b[36m❯\x1b[0m ' : text.replace(/\r/g, '\r\n\x1b[2m(omp is thinking…)\x1b[0m\r\n\x1b[36m❯\x1b[0m ');
    this.#emit([{ kind: 't_output', view, bytes: utf8(echo) }]);
  }

  #request(request: api.Request): void {
    const bad = api.checkRequest(request);
    if (bad !== null) {
      this.#reply(request.id, { err: { code: 'bad_request', message: bad } });
      return;
    }
    const op = request.op;
    const gw = this.#gw;
    if (!gw.opts.s4 && isS4Op(op)) {
      this.#reply(request.id, { err: { code: 'unsupported' } });
      return;
    }
    const seq = request.seq ?? null;
    if (seq !== null) {
      gw.nextSeq = Math.max(gw.nextSeq, seq + 1);
      this.#journal(request.id, seq, op);
      return;
    }
    const answer = this.#view(op);
    if (answer instanceof Promise) {
      void answer.then((result) => this.#reply(request.id, result));
    } else {
      this.#reply(request.id, answer);
    }
  }

  /** Non-journaled requests. */
  #view(op: api.Op): { ok: api.Resp } | { err: api.ApiError } | Promise<{ ok: api.Resp } | { err: api.ApiError }> {
    const gw = this.#gw;
    const unit = { ok: { type: 'unit' } } as const;
    switch (op.op) {
      case 'subscribe': {
        this.#subs.set(feedKey(op.feed), op.feed);
        queueMicrotask(() => this.#sendFeed(op.feed));
        return unit;
      }
      case 'unsubscribe':
        this.#subs.delete(feedKey(op.feed));
        return unit;
      case 'outcome':
        return { ok: { type: 'outcome', outcome: gw.outcomes.get(op.seq) ?? { state: 'unknown' } } };
      case 'models':
        return { ok: { type: 'models', models: fx.models(), truncated: false } };
      case 'diff':
        return { ok: { type: 'diff', diff: fx.diff() } };
      case 'attach': {
        const spec = gw.specByLive(op.live);
        if (spec === undefined) return { err: { code: 'stale', current: null } };
        const view = this.#nextView++;
        this.#views.set(view, spec.session);
        queueMicrotask(() =>
          this.#emit([
            { kind: 't_resize', view, cols: 80, rows: 24 },
            { kind: 't_snapshot', view, last: true, bytes: utf8(fx.terminalSnapshot(spec.title)) },
          ]),
        );
        return { ok: { type: 'attached', view, mode: op.mode } };
      }
      case 'detach':
        this.#views.delete(op.view);
        return unit;
      case 'ping':
        return { ok: { type: 'pong' } };
      case 'last_reply':
        return { ok: { type: 'text', text: fx.card(op.session).last_reply ?? '', truncated: false } };
      case 'export':
        return { ok: gw.download(`${op.session}.html`, 'text/html', gw.exportHtml(op.session)) };
      case 'blob':
        return { ok: gw.download(`${op.blob}.png`, 'image/png', gw.opts.png) };
      case 'download': {
        const download = gw.downloads.get(op.download);
        if (download === undefined) return { err: { code: 'not_found', what: 'download' } };
        const end = Math.min(download.bytes.length, op.offset + api.MAX_TRANSFER_CHUNK);
        return { ok: { type: 'chunk', data: toB64(download.bytes.subarray(op.offset, end)), eof: end >= download.bytes.length } };
      }
      case 'tree':
        return { ok: fx.tree(gw.t0, op.session, op.filter, op.cursor ?? null, op.limit) };
      case 'browse':
        return { ok: fx.browse(op.dir ?? null) };
      case 'usage_activity':
        return { ok: { type: 'activity', days: fx.activity(gw.t0, op.days) } };
      case 'usage_refresh':
        gw.usageFetching = true;
        gw.changed('usage');
        setTimeout(() => {
          gw.usageFetching = false;
          gw.changed('usage');
        }, 1_200);
        return unit;
      case 'omp_settings':
        return { ok: { type: 'omp_settings', groups: fx.ompSettings(), truncated: false } };
      case 'backup_list':
        return gw.backupListing === null
          ? { ok: { type: 'backup_list', hosts: fx.backupList(gw.t0), truncated: false } }
          : Promise.resolve(gw.backupListing(op)).then((ok) => ({ ok }));
      case 'sync_backups':
        return gw.backupListing === null
          ? { ok: { type: 'sync_backups', backups: fx.syncBackups(gw.t0, op.project ?? null), truncated: false } }
          : Promise.resolve(gw.backupListing(op)).then((ok) => ({ ok }));
      case 'presence':
        return unit;
      case 'push_subscribe':
        gw.push = { ...gw.push, subscribed: true, since_unix_ms: gw.now() };
        gw.changed('device');
        return unit;
      case 'push_unsubscribe':
        gw.push = { ...gw.push, subscribed: false, since_unix_ms: null };
        gw.changed('device');
        return unit;
      case 'push_prefs':
        gw.push = { ...gw.push, prefs: op.prefs };
        gw.changed('device');
        return unit;
      case 'device_rename':
        gw.deviceName = op.name;
        gw.changed('device');
        return unit;
      case 'unpair':
        return unit;
      case 'upload_begin': {
        if (op.size > api.MAX_UPLOAD_FILE) return { err: { code: 'too_large' } };
        const upload = gw.handle('up');
        gw.uploads.set(upload, { name: op.name, size: op.size, received: 0 });
        return { ok: { type: 'upload', upload } };
      }
      case 'upload_chunk': {
        const upload = gw.uploads.get(op.upload);
        if (upload === undefined) return { err: { code: 'not_found', what: 'upload' } };
        if (op.offset !== upload.received) return { err: { code: 'bad_request', message: `expected offset ${upload.received}` } };
        const bytes = Buffer.from(op.data, 'base64').length;
        const { promise, resolve } = Promise.withResolvers<{ ok: api.Resp }>();
        setTimeout(() => {
          upload.received += bytes;
          if (upload.received >= upload.size) {
            gw.uploads.delete(op.upload);
            gw.finishedUploads.set(op.upload, upload.name);
          }
          resolve(unit);
        }, gw.opts.uploadDelayMs);
        return promise;
      }
      case 'upload_cancel':
        gw.uploads.delete(op.upload);
        return unit;
      case 'update_check':
        gw.ompChecking = true;
        gw.appPhase = { state: 'checking' };
        gw.changed('updates');
        setTimeout(() => {
          gw.ompChecking = false;
          gw.appPhase = { state: 'idle' };
          gw.changed('updates');
        }, 1_000);
        return unit;
      default:
        return { err: { code: 'bad_request', message: `${op.op} needs a seq` } };
    }
  }

  /** Journaled requests: `accepted`, optional progress, then the outcome event. */
  #journal(id: number, seq: number, op: api.Op): void {
    const gw = this.#gw;
    const previous = gw.outcomes.get(seq);
    this.#reply(id, { ok: { type: 'accepted', seq, duplicate: previous !== undefined } });
    if (previous !== undefined) return;
    gw.outcomes.set(seq, { state: 'running' });
    const steps = PROGRESS[op.op];
    const done = (outcome: api.OutcomeView) => {
      gw.outcomes.set(seq, outcome);
      this.#emit([this.#event({ type: 'outcome', seq, outcome })]);
    };
    const finish = () => {
      try {
        done(this.#apply(op));
      } catch (error) {
        done({ state: 'failed', message: error instanceof Error ? error.message : String(error) });
      }
    };
    if (steps === undefined) {
      setTimeout(finish, gw.opts.outcomeDelayMs);
      return;
    }
    const [stage, total] = steps;
    for (let step = 1; step <= 4; step++) {
      setTimeout(() => {
        this.#emit([this.#event({ type: 'progress', seq, stage, done: Math.round((total * step) / 4), total })]);
        if (op.op === 'app_update' && op.step === 'download') {
          gw.appPhase = { state: 'downloading', done: Math.round((total * step) / 4), total };
          gw.changed('updates');
        }
      }, step * 400);
    }
    setTimeout(finish, 5 * 400);
  }

  /** Applies a journaled op to the shared state and returns its outcome. */
  #apply(op: api.Op): api.OutcomeView {
    const gw = this.#gw;
    const ok = (result: api.DoneView): api.OutcomeView => ({ state: 'done', result });
    const unit = ok({ type: 'unit' });
    const someLive = (session: string): api.DoneView => {
      const spec = fx.LIVE.find((s) => s.session === session) ?? (fx.LIVE[1] as fx.LiveSpec);
      return { type: 'session', live: fx.liveRef(spec) };
    };
    switch (op.op) {
      case 'new_session':
      case 'new_session_in':
      case 'restart':
      case 'update_run':
        return ok(someLive('s-idle'));
      case 'resume':
        return ok(someLive(op.session));
      case 'worktree_create':
        return op.open ? ok(someLive('s-exited')) : unit;
      case 'prompt':
      case 'compose':
      case 'review': {
        const spec = gw.specByLive(op.live);
        if (spec === undefined) return { state: 'rejected', reason: { code: 'stale', current: null } };
        const text =
          op.op === 'review'
            ? `Review comments:\n${op.comments.map((c) => `- ${c.file}:${c.start}-${c.end}: ${c.text}`).join('\n')}${op.general ? `\n\n${op.general}` : ''}`
            : op.text;
        const attachments = op.op === 'compose' ? op.attachments.map((a) => gw.finishedUploads.get(a) ?? a) : [];
        const shown = attachments.length === 0 ? text : `${attachments.map((a) => `"~/.local/share/omp-deck/attachments/${a}"`).join(' ')} ${text}`;
        this.appendItems(spec.session, [{ type: 'user', text: shown, from_agent: false, at_unix_ms: gw.now() }]);
        gw.activity.set(spec.session, 'busy');
        gw.changed('live');
        setTimeout(() => {
          this.appendItems(spec.session, [
            {
              type: 'assistant',
              text: `On it. ${attachments.length > 0 ? `I can see ${attachments.join(', ')}. ` : ''}Starting with the files you mentioned.`,
              model: spec.model,
              at_unix_ms: gw.now(),
            },
          ]);
          gw.activity.set(spec.session, spec.state === 'busy' ? 'busy' : 'idle');
          gw.changed('live');
        }, 1_500);
        return ok({ type: 'submitted', request: 1, label: op.op === 'review' ? 'Review' : 'Prompt', delivery: { state: 'delivered' } });
      }
      case 'answer_ui': {
        const spec = gw.specByLive(op.live);
        if (spec === undefined) return { state: 'rejected', reason: { code: 'stale', current: null } };
        // Like the desktop: the outcome event goes out first, the live feed refresh follows.
        setTimeout(() => {
          gw.pendingUi.set(spec.session, null);
          gw.activity.set(spec.session, 'busy');
          gw.changed('live');
        }, 50);
        setTimeout(() => {
          gw.activity.set(spec.session, 'idle');
          gw.changed('live');
        }, 2_000);
        return ok({ type: 'answered', applied: true });
      }
      case 'answer':
        return ok({ type: 'answered', applied: true });
      case 'delete_sessions':
        for (const session of op.sessions) gw.deleted.add(session);
        gw.changed('sessions', 'inventory', 'live', 'projects');
        return ok({ type: 'deleted', results: op.sessions.map((session) => ({ session, error: null })) });
      case 'copy_session':
      case 'move_session':
        return ok({ type: 'file', session: op.op === 'copy_session' ? `${op.session}-copy` : op.session });
      case 'fork_at':
        return ok({ type: 'file', session: `${op.session}-fork` });
      case 'running_kill':
        for (const id of op.ids) gw.killed.add(id);
        gw.changed('running');
        return ok({ type: 'killed', results: op.ids.map((id) => ({ id, error: null })) });
      case 'stage':
      case 'unstage': {
        const set = gw.staged.get(op.project) ?? new Set<string>();
        for (const file of op.files) op.op === 'stage' ? set.add(file) : set.delete(file);
        gw.staged.set(op.project, set);
        gw.changed('changes');
        return unit;
      }
      case 'backup_restore_prepare':
        return ok({ type: 'restore_plan', handle: 7, plan: fx.backupPlan(), truncated: false });
      case 'sync_restore_prepare':
        return ok({ type: 'restore_plan', handle: 8, plan: fx.syncPlan(gw.t0), truncated: false });
      case 'sync_set':
        gw.syncRows = gw.syncRows.map((row) => (row.project === op.project ? { ...row, parts: op.parts, excluded: op.excluded } : row));
        gw.changed('sync');
        return unit;
      case 'sync_add': {
        const display = op.dir.startsWith('dir:') ? op.dir.slice(4) : op.dir;
        gw.syncRows = [...gw.syncRows, { project: `p-${display.split('/').at(-1) ?? 'new'}`, display, parts: { omp: true, sessions: true, memory: true }, excluded: [], last_backup_unix_ms: null }];
        gw.changed('sync');
        return unit;
      }
      case 'sync_remove':
        gw.syncRows = gw.syncRows.filter((row) => row.project !== op.project);
        gw.changed('sync');
        return unit;
      case 'sync_backup':
        gw.syncRows = gw.syncRows.map((row) => (row.project === op.project ? { ...row, last_backup_unix_ms: gw.now() } : row));
        gw.changed('sync');
        return unit;
      case 'app_update':
        gw.appPhase = op.step === 'download' ? { state: 'idle' } : { state: 'needs_manual' };
        gw.changed('updates');
        return op.step === 'apply' ? { state: 'rejected', reason: { code: 'read_only' } } : unit;
      case 'interrupt':
      case 'stop':
      case 'kill': {
        const spec = gw.specByLive(op.live);
        if (spec !== undefined) gw.activity.set(spec.session, op.op === 'interrupt' ? 'idle' : 'exited');
        gw.changed('live');
        return unit;
      }
      default:
        return unit;
    }
  }
}

/** Long-running ops report progress: stage and total. */
const PROGRESS: Partial<Record<api.OpName, readonly [string, number]>> = {
  backup_run: ['uploading', 4_812_331],
  sync_backup: ['packing', 38_400_000],
  update_run: ['installing', 4],
  app_update: ['downloading', 84 * MIB],
  backup_restore_apply: ['restoring', 12],
  sync_restore_apply: ['restoring', 5],
  worktree_finish: ['merging', 3],
};
