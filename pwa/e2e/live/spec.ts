// The live gate's steps: the built app served by the real relay, paired with a real gateway
// that drives real omp on a scripted model (crates/deck-service/tests/pwa_live_stack.rs). One
// phone context runs every step in order (the pairing lives in its IndexedDB). Any console
// error, page error or CSP violation fails the run.

import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { BrowserContext, Locator, Page } from 'playwright-core';
import { persistentPhone } from '../../test/smoke/browser.ts';

/** What the launcher writes to `$OMPDECK_LIVE_URL_FILE`. */
export interface Stack {
  /** The desktop's pairing link (`<origin>/#pair=v1.…`). */
  link: string;
  /** `http://127.0.0.1:<relay port>`: the relay serving the app. */
  origin: string;
  /** `ws://127.0.0.1:<relay port>` */
  relay: string;
  /** The launcher's control endpoint. */
  control: string;
  /** The fixture project as the phone shows it. */
  project: string;
  /** Two-desktop runs: the key of a second relay room, for the second gateway (null otherwise). */
  second_room_key?: string | null;
}

interface Message {
  role: string;
  text: string;
}

interface ModelRequest {
  index: number;
  tools: boolean;
  last_user: string;
  images: string[];
  messages: Message[];
}

interface Requests {
  count: number;
  requests: ModelRequest[];
}

interface DeviceRow {
  name: string;
  platform: string;
  fingerprint: string;
  control: boolean;
  manage: boolean;
  online: boolean;
  push: boolean;
}

interface Pushed {
  path: string;
  payload?: { web_push?: number; notification?: { title?: string; body?: string; navigate?: string } };
  error?: string;
}

interface Subscription {
  endpoint: string;
  p256dh: string;
  auth: string;
}

const OUT = process.env['LIVE_OUT'] ?? '/tmp/pwa-smoke/live';
const STEP = 30_000;
/** omp's first start in a fresh sandbox (and a turn on the scripted model). */
const SLOW = 120_000;

export class LiveRun {
  readonly #stack: Stack;
  readonly errors: string[] = [];
  readonly csp: string[] = [];
  readonly sockets: { url: string; received: number; closed: boolean; error: string | null }[] = [];
  readonly report: string[] = [];
  readonly shots: string[] = [];
  #page: Page | null = null;

  constructor(stack: Stack) {
    this.#stack = stack;
  }

  get page(): Page {
    if (this.#page === null) throw new Error('no page');
    return this.#page;
  }

  note(step: string, observed: string): void {
    const line = `✓ ${step}: ${observed}`;
    this.report.push(line);
    console.log(line);
  }

  async shot(name: string): Promise<void> {
    const path = join(OUT, `${name}.png`);
    await this.page.screenshot({ path });
    this.shots.push(path);
  }

  async ctl<T>(path: string, init?: RequestInit): Promise<T> {
    const response = await fetch(`${this.#stack.control}${path}`, init);
    const body = await response.text();
    if (!response.ok) throw new Error(`control ${path}: ${response.status} ${body}`);
    return JSON.parse(body) as T;
  }

  async requests(after: number): Promise<Requests> {
    return this.ctl<Requests>(`/requests?after=${after}`);
  }

  /** Polls the model's requests (from `after` on) until one satisfies `f`. */
  async modelRequest(after: number, what: string, f: (request: ModelRequest) => boolean, timeout = SLOW): Promise<ModelRequest> {
    const end = Date.now() + timeout;
    for (;;) {
      const found = (await this.requests(after)).requests.find(f);
      if (found !== undefined) return found;
      if (Date.now() > end) throw new Error(`the model never received ${what}`);
      await Bun.sleep(200);
    }
  }

  /** Waits until the desktop's Mobile access view shows the device's push state as `want`. */
  async devicePush(fingerprint: string, want: boolean): Promise<void> {
    let last = '';
    for (const end = Date.now() + STEP; Date.now() < end; await Bun.sleep(200)) {
      const mobile = await this.ctl<{ devices: DeviceRow[] }>('/mobile');
      if (mobile.devices.some((entry) => entry.fingerprint === fingerprint && entry.push === want)) return;
      last = JSON.stringify(mobile.devices);
    }
    throw new Error(`the desktop never showed push ${want ? 'on' : 'off'} for ${fingerprint}: ${last}`);
  }

  async open(context: BrowserContext): Promise<void> {
    await this.attach(await context.newPage());
  }

  /** Watches `page` (console and page errors, WebSockets, CSP violations) and makes it current. */
  async attach(page: Page): Promise<void> {
    this.#page = page;
    page.on('pageerror', (error) => this.errors.push(`pageerror: ${error.message}`));
    page.on('console', (message) => {
      if (message.type() === 'error') this.errors.push(`console: ${message.text()}`);
    });
    page.on('websocket', (socket) => {
      const entry = { url: socket.url(), received: 0, closed: false, error: null as string | null };
      this.sockets.push(entry);
      socket.on('framereceived', () => {
        entry.received += 1;
      });
      socket.on('socketerror', (error) => {
        entry.error = error;
      });
      socket.on('close', () => {
        entry.closed = true;
      });
    });
    await page.exposeFunction('__liveCspViolation', (violation: string) => {
      this.csp.push(violation);
    });
    await page.addInitScript(() => {
      document.addEventListener('securitypolicyviolation', (event) => {
        const report: unknown = Reflect.get(window, '__liveCspViolation');
        if (typeof report === 'function') void report(`${event.violatedDirective} blocked ${event.blockedURI} (${event.sourceFile}:${event.lineNumber})`);
      });
    });
  }

  /** The composer's text field, once omp takes a prompt. */
  composer(): Locator {
    return this.page.locator('.composer');
  }

  /** Sends `text` and waits until the composer took it (cleared); otherwise fails with what the app showed. */
  async send(text: string): Promise<void> {
    const field = this.page.getByLabel('Message');
    await field.fill(text);
    await this.page.locator('.composer button.send').click({ timeout: SLOW });
    for (const end = Date.now() + SLOW; (await field.inputValue()) !== ''; await Bun.sleep(200)) {
      const toasts = await this.page.locator('.toast').allInnerTexts();
      if (toasts.some((toast) => /did not|failed|refused|not allowed|cannot/i.test(toast)) || Date.now() > end) {
        const live = await this.ctl<unknown>('/live').catch((error: unknown) => String(error));
        const model = await this.requests(0).catch(() => null);
        throw new Error(
          `sending “${text}” did not go through; toasts: ${JSON.stringify(toasts)}; gateway: ${JSON.stringify(live)}; model requests: ${model?.count ?? '?'}`,
        );
      }
    }
  }

  /** Waits for a transcript text matching `pattern`; returns it. */
  async transcript(pattern: RegExp, timeout = SLOW): Promise<string> {
    const found = this.page.getByText(pattern).first();
    await found.waitFor({ timeout });
    return (await found.innerText()).trim();
  }

  async idle(): Promise<void> {
    await this.page.locator('.composer.mode-send').waitFor({ timeout: SLOW });
  }

  /** Pretends the page went to the background (headless Chromium keeps it visible). */
  async setHidden(hidden: boolean): Promise<void> {
    await this.page.evaluate((hide) => {
      if (hide) {
        Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' });
        Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
      } else {
        Reflect.deleteProperty(document, 'visibilityState');
        Reflect.deleteProperty(document, 'hidden');
      }
      document.dispatchEvent(new Event('visibilitychange'));
    }, hidden);
  }
}

export function assert(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

async function vapidPublicKey(): Promise<string> {
  const pair = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign', 'verify']);
  const raw = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey));
  return Buffer.from(raw).toString('base64url');
}

/** Tries a real `pushManager.subscribe` (the browser's own push service). */
async function probeRealPush(page: Page): Promise<{ ok: true; endpoint: string } | { ok: false; error: string }> {
  const key = await vapidPublicKey();
  return page.evaluate(async (applicationServerKey) => {
    const registration = await navigator.serviceWorker.ready;
    const attempt = registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey }).then(
      async (subscription) => {
        const endpoint = subscription.endpoint;
        await subscription.unsubscribe();
        return { ok: true as const, endpoint };
      },
      (error: unknown) => ({ ok: false as const, error: error instanceof Error ? `${error.name}: ${error.message}` : String(error) }),
    );
    const { promise: timeout, resolve } = Promise.withResolvers<{ ok: false; error: string }>();
    setTimeout(() => resolve({ ok: false, error: 'no answer within 20 s' }), 20_000);
    return Promise.race([attempt, timeout]);
  }, key);
}

/** Replaces `PushManager` with one handing out `subscription` (the launcher's loopback receiver). */
async function stubPush(page: Page, subscription: Subscription): Promise<void> {
  await page.evaluate((sub) => {
    const bytes = (text: string): ArrayBuffer => {
      const b64 = text.replace(/-/g, '+').replace(/_/g, '/');
      const binary = atob(b64 + '='.repeat((4 - (b64.length % 4)) % 4));
      return Uint8Array.from(binary, (c) => c.charCodeAt(0)).buffer;
    };
    const keyBuffer = (key: BufferSource | string | null | undefined): ArrayBuffer | null => {
      if (key === null || key === undefined) return null;
      if (typeof key === 'string') return bytes(key);
      if (key instanceof ArrayBuffer) return key;
      const view = new Uint8Array(key.buffer, key.byteOffset, key.byteLength);
      return view.slice().buffer;
    };
    let current: PushSubscription | null = null;
    const make = (key: ArrayBuffer | null): PushSubscription =>
      ({
        endpoint: sub.endpoint,
        expirationTime: null,
        options: { userVisibleOnly: true, applicationServerKey: key },
        getKey: (name: PushEncryptionKeyName) => bytes(name === 'p256dh' ? sub.p256dh : sub.auth),
        unsubscribe: async () => {
          current = null;
          return true;
        },
        toJSON: () => ({ endpoint: sub.endpoint, expirationTime: null, keys: { p256dh: sub.p256dh, auth: sub.auth } }),
      }) as unknown as PushSubscription;
    PushManager.prototype.subscribe = async function (options?: PushSubscriptionOptionsInit): Promise<PushSubscription> {
      current = make(keyBuffer(options?.applicationServerKey));
      return current;
    };
    PushManager.prototype.getSubscription = async function (): Promise<PushSubscription | null> {
      return current;
    };
  }, subscription);
}

async function notificationsScreen(page: Page): Promise<void> {
  await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
  await page.locator('button.hub-row', { hasText: 'Notifications' }).click();
  await page.getByRole('switch', { name: 'Push notifications' }).waitFor();
}

async function setPush(page: Page, on: boolean): Promise<void> {
  const toggle = page.getByRole('switch', { name: 'Push notifications' });
  await toggle.click();
  await page.locator(`.switch[aria-checked="${on}"]`).first().waitFor({ timeout: STEP });
  await page.getByText(on ? 'subscribed' : 'not subscribed', { exact: true }).waitFor({ timeout: STEP });
}

export async function runLive(stack: Stack): Promise<LiveRun> {
  mkdirSync(OUT, { recursive: true });
  const live = new LiveRun(stack);
  const profile = mkdtempSync(join(tmpdir(), 'pwa-live-profile-'));
  try {
    const context = await persistentPhone(profile);
    try {
      context.setDefaultTimeout(STEP);
      await context.grantPermissions(['notifications'], { origin: stack.origin });
      await live.open(context);
      await steps(live, stack, live.page);
    } catch (error) {
      await live.shot('failure').catch(() => undefined);
      throw error;
    } finally {
      await context.close();
    }
  } finally {
    rmSync(profile, { recursive: true, force: true });
  }
  return live;
}

async function steps(live: LiveRun, stack: Stack, page: Page): Promise<void> {
  // -- pair --
  await page.goto(stack.link);
  await page.getByRole('button', { name: 'Pair this phone' }).waitFor();
  const href = page.url();
  assert(!href.includes('#pair='), `#pair= still in location.href: ${href}`);
  await live.shot('01-pair-invite');
  await page.getByRole('button', { name: 'Pair this phone' }).click();
  await page.getByText('Approve on your desktop').waitFor();
  const phoneFingerprint = (await page.locator('.fingerprint').innerText()).trim();
  let desktopFingerprint: string | null = null;
  for (const end = Date.now() + STEP; desktopFingerprint === null; ) {
    desktopFingerprint = (await live.ctl<{ fingerprint: string | null }>('/pairing')).fingerprint;
    if (desktopFingerprint === null) {
      assert(Date.now() < end, 'the desktop never showed an approval request');
      await Bun.sleep(100);
    }
  }
  await live.shot('02-pair-fingerprint');
  assert(phoneFingerprint === desktopFingerprint, `fingerprints differ: phone ${phoneFingerprint}, desktop ${desktopFingerprint}`);
  await live.ctl('/approve', { method: 'POST', body: phoneFingerprint });
  await page.getByText(/Connected/).first().waitFor({ timeout: STEP });
  assert(!page.url().includes('#pair='), `#pair= back in location.href: ${page.url()}`);
  const mobile = await live.ctl<{ devices: DeviceRow[] }>('/mobile');
  const device = mobile.devices.find((entry) => entry.fingerprint === phoneFingerprint);
  assert(device !== undefined, `the desktop lists no device ${phoneFingerprint}: ${JSON.stringify(mobile)}`);
  assert(device.control && device.manage, `permissions ${JSON.stringify(device)}`);
  live.note('pair', `fingerprint ${phoneFingerprint} matched the desktop's approval dialog; approved View+Control+Manage as “${device.name}”; location ${page.url()}`);

  // -- the relay's WebSocket under its CSP --
  const deviceSocket = live.sockets.find((socket) => socket.url.startsWith(`${stack.relay}/v1/device`) && socket.received > 0 && !socket.closed);
  assert(deviceSocket !== undefined, `no open device WebSocket with traffic: ${JSON.stringify(live.sockets)}`);
  live.note('websocket', `${deviceSocket.url} open under the relay's CSP, ${deviceSocket.received} frames received so far`);

  // -- sessions --
  const projectRow = page.locator('button.name', { hasText: stack.project }).first();
  await projectRow.waitFor({ timeout: STEP });
  if ((await projectRow.getAttribute('aria-expanded')) !== 'true') await projectRow.click();
  await page.getByText('Seeded', { exact: true }).first().waitFor();
  await live.shot('03-sessions');
  live.note('sessions', `project ${stack.project} listed with its seeded session “Seeded”`);

  // -- push: enable (real push service first, then the stubbed one) --
  await notificationsScreen(page);
  const probe = await probeRealPush(page);
  if (probe.ok) {
    // The real push service answered: the desktop must accept its endpoint (SSRF guard).
    await setPush(page, true);
    await live.devicePush(phoneFingerprint, true);
    live.note('push (real)', `pushManager.subscribe → ${new URL(probe.endpoint).host}; the desktop accepted it`);
    await setPush(page, false);
  } else {
    live.note('push (real)', `headless Chromium cannot subscribe to the real push service (${probe.error}); stubbing PushManager`);
  }
  const subscription = await live.ctl<Subscription>('/push-subscription');
  await stubPush(page, subscription);
  await setPush(page, true);
  await live.devicePush(phoneFingerprint, true);
  await live.shot('04-push-on');
  live.note('push (subscribe)', `stubbed subscription to ${subscription.endpoint} sent; the desktop reports the device subscribed`);

  // -- prompt: a new session in the fixture project --
  await page.getByRole('button', { name: 'Back' }).click();
  await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Sessions/ }).click();
  await page.getByRole('button', { name: `New session in ${stack.project}` }).click();
  // While omp starts the composer offers Insert only; Send appears once omp is idle.
  const sawStarting = await page
    .getByText('omp is starting…')
    .waitFor({ timeout: 10_000 })
    .then(
      () => true,
      () => false,
    );
  if (sawStarting) await live.shot('05a-starting');
  await live.idle();
  live.note('new session', `${sawStarting ? 'composer showed “omp is starting…” (Insert only), then ' : ''}Send once omp was idle`);
  let seen = (await live.requests(0)).count;
  await live.send('PROMPT-MARK hello from the phone');
  const reply = await live.transcript(/LIVE-REPLY to: PROMPT-MARK hello from the phone/);
  await live.idle();
  await live.shot('05-prompt-reply');
  live.note('prompt', `transcript shows “${reply}”`);

  // -- ask --
  seen = (await live.requests(0)).count;
  await live.send('ASK-MARK choose one');
  const card = page.locator('.ui-card', { hasText: 'Pick a color?' });
  await card.waitFor({ timeout: SLOW });
  await live.shot('06-ask-card');
  await card.locator('button.option', { hasText: 'blue' }).click();
  await card.getByRole('button', { name: /Submit answers/ }).click();
  await card.waitFor({ state: 'detached' });
  const asked = await live.modelRequest(seen, 'the ask result', (request) => request.last_user.includes('ASK-MARK') && request.messages.some((m) => m.role === 'tool'));
  const toolText = asked.messages.filter((m) => m.role === 'tool').map((m) => m.text).join(' ');
  assert(toolText.includes('blue') && !toolText.includes('red'), `the ask result: ${toolText}`);
  const askReply = await live.transcript(/ASK-REPLY/);
  assert(askReply.includes('blue'), `the transcript's ask reply: ${askReply}`);
  await live.idle();
  await live.shot('07-ask-answered');
  live.note('ask', `answered “blue” on the approval card; model got tool result “${toolText.slice(0, 80)}”; transcript shows “${askReply.slice(0, 100)}”`);

  // -- steer --
  seen = (await live.requests(0)).count;
  await live.send('STEER-MARK read the notes');
  await page.locator('.composer.mode-steer').waitFor({ timeout: SLOW });
  await live.modelRequest(seen, 'the steered turn', (request) => request.tools && request.last_user.includes('STEER-MARK'));
  await live.send('STEER-TEXT change course');
  await page.getByText('Steered omp.').waitFor();
  await live.shot('08-steer-sent');
  const steered = await live.modelRequest(seen, 'the steer', (request) => request.messages.some((m) => m.role === 'user' && m.text.includes('STEER-TEXT')));
  const roles = steered.messages.map((m) => m.role);
  const markAt = steered.messages.findIndex((m) => m.role === 'user' && m.text.includes('STEER-MARK'));
  const steerAt = steered.messages.findIndex((m) => m.role === 'user' && m.text.includes('STEER-TEXT'));
  assert(markAt >= 0 && steerAt > markAt, `steer order: ${JSON.stringify(roles)}`);
  const between = roles.slice(markAt + 1, steerAt);
  await live.transcript(/LIVE-REPLY to: STEER-TEXT change course/);
  await live.idle();
  live.note('steer', `steered while busy; the model's request #${steered.index} carries “STEER-TEXT change course” after [${between.join(', ')}] of the running turn`);

  // -- attachment --
  const png = new Uint8Array(await (await fetch(`${stack.control}/png`)).arrayBuffer());
  seen = (await live.requests(0)).count;
  await page.locator('.composer input[type=file]:not([capture])').setInputFiles({ name: 'gradient.png', mimeType: 'image/png', buffer: Buffer.from(png) });
  await page.locator('.attach-chip.done').waitFor();
  await live.shot('09-attachment-ready');
  await live.send('IMG-MARK what is in this picture');
  const withImage = await live.modelRequest(seen, 'the image', (request) => request.tools && request.last_user.includes('IMG-MARK'));
  assert(withImage.images.length === 1, `images: ${withImage.images.length}`);
  const expected = `data:image/png;base64,${Buffer.from(png).toString('base64')}`;
  assert(withImage.images[0] === expected, `the model's image differs (${withImage.images[0]?.slice(0, 60)}…)`);
  const imgReply = await live.transcript(/IMG-REPLY saw 1 image/);
  await live.idle();
  await live.shot('10-attachment-reply');
  live.note('attachment', `${png.length}-byte PNG reached the model byte-identical; transcript shows “${imgReply}”`);

  // -- a big upload through the real relay's device rate limit (1 MiB/s, 4 MiB burst) --
  const socketsBefore = live.sockets.length;
  const closedBefore = live.sockets.filter((entry) => entry.closed).length;
  const bigSize = 12 * 1024 * 1024;
  const big = Buffer.alloc(bigSize);
  for (let i = 0; i < bigSize; i += 4096) big.writeUInt32LE((i * 2654435761) >>> 0, i);
  const uploadStart = Date.now();
  await page.locator('.composer input[type=file]:not([capture])').setInputFiles({ name: 'big.bin', mimeType: 'application/octet-stream', buffer: big });
  await page.locator('.attach-chip.done').waitFor({ timeout: 90_000 });
  const uploadSeconds = (Date.now() - uploadStart) / 1000;
  assert(live.sockets.length === socketsBefore, `the big upload reconnected: ${JSON.stringify(live.sockets)}`);
  assert(live.sockets.filter((entry) => entry.closed).length === closedBefore, 'a socket closed during the big upload (4029?)');
  assert(uploadSeconds > 6, `the 12 MiB upload was not paced (${uploadSeconds}s)`);
  await page.locator('.attach-chip.done .attach-x').click();
  live.note('big-upload', `12 MiB uploaded through the real relay in ${uploadSeconds.toFixed(1)}s with no close or reconnect`);

  // -- push: a finished turn while the app is in the background --
  const before = (await live.ctl<Pushed[]>('/pushes')).length;
  assert(before === 0, `pushes while the app was visible: ${before}`);
  await live.setHidden(true);
  await live.send('PUSH-MARK take your time');
  await live.transcript(/PUSH-REPLY finished after a while/);
  let pushes: Pushed[] = [];
  for (const end = Date.now() + STEP; pushes.length === 0; ) {
    pushes = await live.ctl<Pushed[]>('/pushes');
    if (pushes.length === 0) {
      assert(Date.now() < end, 'no push for the finished turn');
      await Bun.sleep(200);
    }
  }
  await Bun.sleep(3000);
  pushes = await live.ctl<Pushed[]>('/pushes');
  await live.setHidden(false);
  await live.idle();
  assert(pushes.length === 1, `pushes for one finished turn: ${JSON.stringify(pushes)}`);
  const pushed = pushes[0];
  assert(pushed !== undefined && pushed.error === undefined && pushed.payload !== undefined, `the push: ${JSON.stringify(pushed)}`);
  const notification = pushed.payload.notification;
  assert(notification?.title === 'OmpDeck' && typeof notification.navigate === 'string', `the push payload: ${JSON.stringify(pushed.payload)}`);
  live.note('push (delivery)', `one encrypted push to ${pushed.path}, decrypted: ${JSON.stringify(pushed.payload)}`);

  // -- download --
  await page.getByRole('button', { name: 'Session actions' }).click();
  const [download] = await Promise.all([page.waitForEvent('download', { timeout: SLOW }), page.getByText('Export as HTML').click()]);
  const file = await download.path();
  const html = await Bun.file(file).text();
  assert(html.length > 0 && html.includes('<html'), `the export (${html.length} bytes) is not HTML`);
  // omp embeds the conversation as base64 JSON in <script id="session-data">.
  const embedded = /<script id="session-data" type="application\/json">([A-Za-z0-9+/=]+)<\/script>/.exec(html)?.[1];
  assert(embedded !== undefined, 'the export has no embedded session data');
  assert(Buffer.from(embedded, 'base64').toString('utf8').includes('PROMPT-MARK hello from the phone'), 'the export misses the conversation');
  await live.shot('11-export');
  live.note('download', `export saved as ${download.suggestedFilename()} (${html.length} bytes of HTML with the conversation)`);
  if (await page.locator('.sheet').isVisible()) await page.keyboard.press('Escape');
  await page.locator('.sheet').waitFor({ state: 'detached' });

  // -- terminal: a line typed in Input mode reaches omp as keystrokes (Enter submits it) --
  seen = (await live.requests(0)).count;
  await page.getByRole('button', { name: 'Terminal' }).click();
  const term = page.locator('.term');
  await term.locator('.xterm').waitFor();
  await page.getByRole('button', { name: /Observe/ }).click();
  await page.locator('button.option', { hasText: 'Type into the session' }).click();
  const input = page.getByLabel('Line to send to the terminal');
  await input.fill('TERM-TYPED from the phone');
  await input.press('Enter');
  const typed = await live.modelRequest(seen, 'the typed line', (request) => request.tools && request.last_user.includes('TERM-TYPED'), STEP);
  await term.getByText(/LIVE-REPLY to: TERM-TYPED from the phone/).first().waitFor({ timeout: STEP });
  await live.shot('12-terminal-typed');
  live.note('terminal', `Input mode; the typed line reached omp (model request #${typed.index}) and its reply rendered in the phone’s terminal`);

  // -- console, page errors, CSP --
  const finalPushes = await live.ctl<Pushed[]>('/pushes');
  assert(finalPushes.length === 1, `pushes over the whole run: ${finalPushes.length}`);
  assert(live.csp.length === 0, `CSP violations: ${live.csp.join(' | ')}`);
  assert(live.errors.length === 0, `console/page errors: ${live.errors.join(' | ')}`);
  live.note('csp/console', `0 CSP violations, 0 console errors; ${live.sockets.length} WebSocket(s) opened`);
}
