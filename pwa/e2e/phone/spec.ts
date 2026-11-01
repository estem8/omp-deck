// The phone gate's steps on a real Android phone (`bun run phone`): the live stack (the real relay
// serving the built app, a real gateway, real omp on a scripted model) reached through
// `adb reverse`, so the phone's Chrome loads the app from `http://127.0.0.1:<port>` — loopback is
// a secure context: service worker and Web Push work as on HTTPS. Everything the user does is a
// physical tap or key event on the phone (adb); Chrome's DevTools (CDP over adb) only observes:
// the page's state, console and page errors, WebSockets and CSP violations. Push goes through the
// phone's real push service (FCM) into the system's notification list.
//
// Two real desktops share the relay (each its own gateway, sandbox, room, omp and scripted model);
// the phone pairs with both, subscribes to push on each (a registration per desktop at /d/<id>/),
// and a turn that finishes on the inactive desktop reaches it as a real push whose tap switches
// the app to that desktop.

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { type Browser, chromium, type Page } from 'playwright-core';
import { assert, LiveRun, type Stack } from '../live/spec.ts';
import * as phone from './adb.ts';
import { titleWithLabel } from '../../src/push.ts';

const OUT = process.env['PHONE_OUT'] ?? '/tmp/pwa-smoke/phone';
const STEP = 30_000;
/** omp's first start in a fresh sandbox (and a turn on the scripted model). */
const SLOW = 120_000;
/** The picked attachment's name in the phone's Download folder. */
const PICKED = 'ompdeck-gate-gradient.png';

interface DeviceRow {
  fingerprint: string;
  control: boolean;
  manage: boolean;
  online: boolean;
  push: boolean;
}

export class PhoneRun {
  readonly live: LiveRun;
  /** The second desktop's control endpoint (its gateway is a second real stack on the same relay). */
  readonly liveB: LiveRun;
  readonly stack: Stack;
  readonly second: Stack;
  readonly report: string[] = [];
  readonly shots: string[] = [];
  readonly #devtools: number;
  /** The phone's fingerprint at desktop A and at desktop B (each pairing has its own static key). */
  fingerprintA = '';
  fingerprintB = '';
  /** The two desktops' names in the switcher (A paired first). */
  titleA = '';
  titleB = '';
  /** Desktop A's name while it was the only desktop. */
  plainA = '';
  /** How long each two-desktop step took, ms. */
  readonly timings: [string, number][] = [];
  #lapStart = performance.now();
  /** Starts timing a step. */
  readonly startLap = (): void => {
    this.#lapStart = performance.now();
  };
  readonly lap = (name: string): void => {
    this.timings.push([name, Math.round(performance.now() - this.#lapStart)]);
    this.#lapStart = performance.now();
  };
  #browser: Browser | null = null;

  constructor(stack: Stack, second: Stack, devtools: number) {
    this.live = new LiveRun(stack);
    this.liveB = new LiveRun(second);
    this.stack = stack;
    this.second = second;
    this.#devtools = devtools;
  }

  note(step: string, observed: string): void {
    const line = `✓ ${step}: ${observed}`;
    this.report.push(line);
    console.log(line);
  }

  /** The phone's whole screen (system UI included). */
  shot(name: string): void {
    const path = join(OUT, `${name}.png`);
    phone.screenshot(path);
    this.shots.push(path);
  }

  /** Attaches DevTools and makes the app's tab current. */
  async attach(): Promise<void> {
    // Seen through the plain HTTP list first: attaching while a fresh tab still starts can hang.
    for (const end = Date.now() + STEP; ; await Bun.sleep(300)) {
      const targets = (await (await fetch(`http://127.0.0.1:${this.#devtools}/json/list`)).json()) as { type: string; url: string }[];
      if (targets.some((t) => t.type === 'page' && t.url.startsWith(this.stack.origin))) break;
      assert(Date.now() < end, `no tab on ${this.stack.origin}`);
    }
    await Bun.sleep(1000);
    const browser = await this.#connect();
    this.#browser = browser;
    const page = browser.contexts()[0]?.pages().find((p) => p.url().startsWith(this.stack.origin));
    assert(page !== undefined, `DevTools shows no tab on ${this.stack.origin}`);
    page.setDefaultTimeout(STEP);
    await this.live.attach(page);
  }

  async #connect(): Promise<Browser> {
    for (let attempt = 1; ; attempt++) {
      try {
        return await chromium.connectOverCDP(`http://127.0.0.1:${this.#devtools}`, { timeout: 10_000 });
      } catch (error) {
        if (attempt === 3) throw error;
        await Bun.sleep(1000);
      }
    }
  }

  /**
   * Detaches DevTools. While it is attached Chrome keeps the page visible, so a backgrounded
   * app would still tell the desktop it is on screen (and get no push).
   */
  async detach(): Promise<void> {
    await this.#browser?.close();
    this.#browser = null;
  }

  /** Clears the origin's site data (pairing, service worker and its push subscription) and closes its tabs. */
  async forget(): Promise<void> {
    const browser = this.#browser ?? (await this.#connect());
    try {
      const context = browser.contexts()[0];
      assert(context !== undefined, 'Chrome has no browser context');
      // Android Chrome refuses Storage.clearDataForOrigin on the browser session: any tab's will do.
      const any = context.pages()[0] ?? (await context.newPage());
      const cdp = await context.newCDPSession(any);
      await cdp.send('Storage.clearDataForOrigin', { origin: this.stack.origin, storageTypes: 'all' });
      await cdp.detach();
      for (const page of context.pages()) {
        if (page.url().startsWith(this.stack.origin)) await page.close();
      }
    } finally {
      await browser.close();
      this.#browser = null;
    }
  }

  /** Taps the conversation's message field, types `text` with the device's keys and taps Send. */
  async send(text: string): Promise<void> {
    const page = this.live.page;
    await page.locator('.composer button.send').waitFor({ timeout: SLOW });
    phone.tap(await phone.waitNode('the message field', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.EditText' && n.id === ''));
    for (const end = Date.now() + 5000; !phone.keyboardShown(); await Bun.sleep(200)) {
      assert(Date.now() < end, 'the keyboard did not open on the message field');
    }
    phone.typeText(text);
    const field = page.getByLabel('Message');
    for (const end = Date.now() + 5000; (await field.inputValue()) !== text; await Bun.sleep(100)) {
      assert(Date.now() < end, `the field shows “${await field.inputValue()}”, typed “${text}”`);
    }
    // The composer stays above the keyboard.
    const fit = await page.evaluate(() => {
      const box = document.querySelector('.composer')?.getBoundingClientRect();
      const view = window.visualViewport;
      return box === undefined || view === null ? null : { bottom: box.bottom, visible: view.offsetTop + view.height };
    });
    assert(fit !== null && fit.bottom <= fit.visible + 1, `the keyboard covers the composer: ${JSON.stringify(fit)}`);
    await phone.tapNode('Send', (n) => n.desc === 'Send' && n.enabled);
    for (const end = Date.now() + SLOW; (await field.inputValue()) !== ''; await Bun.sleep(200)) {
      const toasts = await page.locator('.toast').allInnerTexts();
      assert(!toasts.some((t) => /did not|failed|refused|not allowed|cannot/i.test(t)) && Date.now() < end, `sending “${text}” did not go through; toasts: ${JSON.stringify(toasts)}`);
    }
    if (phone.keyboardShown()) phone.key('KEYCODE_BACK');
  }

  async idle(): Promise<void> {
    await this.live.page.locator('.composer.mode-send').waitFor({ timeout: SLOW });
  }
}

export async function runPhone(stack: Stack, second: Stack, devtools: number): Promise<PhoneRun> {
  mkdirSync(OUT, { recursive: true });
  const run = new PhoneRun(stack, second, devtools);
  // Stale state of an earlier run on this origin (pairing with another gateway, subscription).
  await run.forget();
  try {
    await steps(run);
  } catch (error) {
    try {
      run.shot('failure');
    } catch {
      // the screenshot is best effort
    }
    throw error;
  } finally {
    phone.adb('shell', 'cmd', 'statusbar', 'collapse');
    phone.adb('shell', 'rm', '-f', `/sdcard/Download/${PICKED}`);
    await run.detach();
    // Chrome's DevTools socket refuses connections while Chrome is in the background.
    phone.startChrome();
    await Bun.sleep(1500);
    await run.forget().catch((error: unknown) => console.error(`phone: cleanup: ${String(error)}`));
  }
  return run;
}

async function steps(run: PhoneRun): Promise<void> {
  const { stack, live } = run;

  // -- pair: the link opens in Chrome the way a scanned QR code does --
  phone.openUrl(stack.link);
  await run.attach();
  const page = live.page;
  await page.getByRole('button', { name: 'Pair this phone' }).waitFor();
  assert(!page.url().includes('#pair='), `#pair= still in location.href: ${page.url()}`);
  run.shot('01-pair-invite');
  await phone.tapNode('Pair this phone', phone.named('Pair this phone'));
  await page.getByText('Approve on your desktop').waitFor();
  const fingerprint = (await page.locator('.fingerprint').innerText()).trim();
  let desktop: string | null = null;
  for (const end = Date.now() + STEP; desktop === null; ) {
    desktop = (await live.ctl<{ fingerprint: string | null }>('/pairing')).fingerprint;
    if (desktop === null) {
      assert(Date.now() < end, 'the desktop never showed an approval request');
      await Bun.sleep(100);
    }
  }
  run.shot('02-pair-fingerprint');
  assert(fingerprint === desktop, `fingerprints differ: phone ${fingerprint}, desktop ${desktop}`);
  run.fingerprintA = fingerprint;
  await live.ctl('/approve', { method: 'POST', body: fingerprint });
  await page.getByText(/Connected/).first().waitFor();
  const devices = (await live.ctl<{ devices: (DeviceRow & { platform: string; name: string })[] }>('/mobile')).devices;
  const device = devices.find((d) => d.fingerprint === fingerprint);
  assert(device !== undefined && device.control && device.manage, `the desktop's device: ${JSON.stringify(devices)}`);
  run.note('pair', `link opened by an Android intent; fingerprint ${fingerprint} matched the desktop; approved as “${device.name}” (${device.platform})`);

  // -- the pairing survives a reload (IndexedDB); the device socket runs under the relay's CSP --
  await page.reload();
  await page.getByText(/Connected/).first().waitFor();
  for (const end = Date.now() + STEP; ; await Bun.sleep(200)) {
    if (live.sockets.some((s) => s.url.startsWith(`${stack.relay}/v1/device`) && s.received > 0 && !s.closed)) break;
    assert(Date.now() < end, `no open device WebSocket with traffic: ${JSON.stringify(live.sockets)}`);
  }
  run.shot('03-sessions');
  run.note('reload', 'still paired after a reload; device WebSocket open with traffic');

  // The second desktop's pairing window is short-lived: pair it now, while it is open.
  await pairSecondDesktop(run);

  // -- push: the user turns it on; Chrome asks; the phone's real push service subscribes --
  await phone.tapNode('Settings', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Settings/.test(n.text));
  await phone.tapNodeAboveBar('Notifications', (n) => n.pkg === phone.CHROME && /^Notifications/.test(n.text || n.desc) && n.id === '');
  await page.getByRole('switch', { name: 'Push notifications' }).waitFor();
  await phone.tapNode('the Push notifications switch', (n) => n.pkg === phone.CHROME && n.desc === 'Push notifications');
  const prompt = await phone.waitNode('', phone.named('Allow'), 8000).catch(() => null);
  if (prompt !== null) {
    run.shot('04-permission-prompt');
    phone.tap(prompt);
  }
  await page.getByText('subscribed', { exact: true }).waitFor();
  // The subscription lives on the desktop's own registration (scope /d/<id>/), not on the root one.
  const endpoint = await page.evaluate(async () => {
    for (const registration of await navigator.serviceWorker.getRegistrations()) {
      const subscription = await registration.pushManager.getSubscription();
      if (subscription !== null) return subscription.endpoint;
    }
    return '';
  });
  const pushHost = endpoint === '' ? '' : new URL(endpoint).host;
  assert(endpoint.startsWith('https://') && !/^(127\.|localhost)/.test(pushHost), `not a real push service: ${endpoint}`);
  await live.devicePush(fingerprint, true);
  run.shot('05-push-on');
  run.note('push (subscribe)', `${prompt === null ? 'permission already granted on this origin' : 'Chrome asked; Allow tapped'}; subscribed at ${pushHost}; the desktop shows push on`);

  // -- a new session; a prompt typed with the phone's keyboard --
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await phone.tapNode('Sessions', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Sessions/.test(n.text));
  await phone.tapNode('New session', phone.named(`New session in ${stack.project}`));
  await run.idle();
  await run.send('PROMPT-MARK typed on the phone');
  const reply = await live.transcript(/LIVE-REPLY to: PROMPT-MARK typed on the phone/);
  await run.idle();
  run.shot('06-prompt-reply');
  run.note('prompt', `typed with key events into the field (keyboard open, composer above it); transcript shows “${reply}”`);

  // -- ask: answered with taps --
  let seen = (await live.requests(0)).count;
  await run.send('ASK-MARK choose one');
  await page.locator('.ui-card', { hasText: 'Pick a color?' }).waitFor({ timeout: SLOW });
  run.shot('07-ask-card');
  await phone.tapNode('the blue option', (n) => n.pkg === phone.CHROME && n.text === 'blue');
  await phone.tapNode('Submit answers', (n) => n.pkg === phone.CHROME && /Submit answers/.test(n.text || n.desc));
  const asked = await live.modelRequest(seen, 'the ask result', (r) => r.last_user.includes('ASK-MARK') && r.messages.some((m) => m.role === 'tool'));
  const toolText = asked.messages.filter((m) => m.role === 'tool').map((m) => m.text).join(' ');
  assert(toolText.includes('blue') && !toolText.includes('red'), `the ask result: ${toolText}`);
  await live.transcript(/ASK-REPLY/);
  await run.idle();
  run.note('ask', `“blue” tapped; the model got “${toolText.slice(0, 60)}”`);

  // -- attachment: picked in Android's own file picker --
  const png = new Uint8Array(await (await fetch(`${stack.control}/png`)).arrayBuffer());
  const local = join(OUT, PICKED);
  await Bun.write(local, png);
  phone.adb('push', local, `/sdcard/Download/${PICKED}`);
  phone.adb('shell', 'am', 'broadcast', '-a', 'android.intent.action.MEDIA_SCANNER_SCAN_FILE', '-d', `file:///sdcard/Download/${PICKED}`);
  await phone.tapNode('Attach files', phone.named('Attach files'));
  // Chrome asks for the camera (its chooser offers a capture option) unless it may already.
  const camera = await phone.waitNode('', phone.named('Only this time'), 3000).catch(() => null);
  if (camera !== null) phone.tap(camera);
  const source = await phone.waitNode('the file chooser', (n) => ['Files', 'Browse', 'Photos & videos', 'Documents'].includes(n.text));
  phone.tap(source);
  await phone.tapNode(PICKED, phone.named(PICKED));
  await page.locator('.attach-chip.done').waitFor();
  run.shot('08-attachment-ready');
  seen = (await live.requests(0)).count;
  await run.send('IMG-MARK what is in this picture');
  const withImage = await live.modelRequest(seen, 'the image', (r) => r.tools && r.last_user.includes('IMG-MARK'));
  const expected = `data:image/png;base64,${Buffer.from(png).toString('base64')}`;
  assert(withImage.images.length === 1 && withImage.images[0] === expected, `the model's image differs (${withImage.images.length} image(s))`);
  await live.transcript(/IMG-REPLY saw 1 image/);
  await run.idle();
  run.note('attachment', `picked through “${source.text}” → ${PICKED}; ${png.length} bytes reached the model byte-identical`);

  // -- push delivery: a turn finishes while the app is in the background --
  assert(phone.notifications(stack.origin).length === 0, `notifications while the app was on screen: ${JSON.stringify(phone.notifications(stack.origin))}`);
  await run.send('PUSH-LONG-MARK take your time');
  await page.locator('.composer.mode-steer').waitFor();
  // Elsewhere in the app, so opening the notification has to navigate.
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await page.getByRole('button', { name: `New session in ${stack.project}` }).waitFor();
  await run.detach();
  // A turn that ends while the app is on screen correctly pushes nothing.
  const controllers = await live.ctl<{ activity: string; label: string }[]>('/live');
  assert(controllers.some((c) => c.activity === 'Busy'), `the turn already ended before the app left the screen: ${JSON.stringify(controllers.map((c) => [c.label, c.activity]))}`);
  phone.key('KEYCODE_HOME');
  const leftAt = Date.now();
  let shown: phone.Shown[] = [];
  for (const end = Date.now() + 60_000; shown.length === 0; await Bun.sleep(500)) {
    shown = phone.notifications(stack.origin);
    assert(shown.length > 0 || Date.now() < end, 'no notification for the finished turn within 60 s');
  }
  const arrived = ((Date.now() - leftAt) / 1000).toFixed(1);
  await Bun.sleep(4000);
  shown = phone.notifications(stack.origin);
  assert(shown.length === 1, `notifications for one finished turn: ${JSON.stringify(shown)}`);
  const notification = shown[0];
  assert(notification !== undefined && notification.title === titleWithLabel('OmpDeck', run.titleA) && notification.text === 'A session finished', `the notification (with two desktops paired its title names the desktop, A): ${JSON.stringify(notification)}`);
  phone.adb('shell', 'cmd', 'statusbar', 'expand-notifications');
  const host = new URL(stack.origin).host;
  const row = await phone.waitNode('the notification in the shade', (n) => n.text === host);
  run.shot('09-notification');
  const body = phone.nodes().find((n) => n.text === notification.text && n.bounds[1] >= row.bounds[3] && n.bounds[1] - row.bounds[3] < 320);
  assert(body !== undefined, 'the notification text is not in the shade');
  phone.tap(body);
  run.note('push (delivery)', `backgrounded with Home mid-turn; “${notification.title}” / “${notification.text}” in the system's list ${arrived} s later (FCM)`);

  // -- opening the notification lands on the session --
  await run.attach();
  await live.page.getByText('PUSH-REPLY finished after a long while').first().waitFor();
  await run.idle();
  assert(phone.notifications(stack.origin).length === 0, 'the notification stayed after it was opened');
  run.shot('10-opened-from-notification');
  run.note('push (open)', 'tapping it brought Chrome back on the session (from the session list); the notification closed');

  // -- the relay restarts: the phone reconnects by itself --
  const socketsBefore = live.sockets.length;
  await live.ctl('/relay-restart', { method: 'POST' });
  const restartedAt = Date.now();
  for (const end = Date.now() + 60_000; ; await Bun.sleep(250)) {
    const fresh = live.sockets.slice(socketsBefore).find((s) => s.url.startsWith(`${stack.relay}/v1/device`) && s.received > 0 && !s.closed);
    if (fresh !== undefined) break;
    assert(Date.now() < end, `no new device WebSocket after the relay restart: ${JSON.stringify(live.sockets)}`);
  }
  const back = ((Date.now() - restartedAt) / 1000).toFixed(1);
  await run.send('RECONNECT-MARK after the restart');
  await live.transcript(/LIVE-REPLY to: RECONNECT-MARK after the restart/);
  run.shot('11-after-relay-restart');
  run.note('reconnect', `relay restarted; the phone opened a new device socket after ${back} s and the next prompt went through`);

  await pushAcrossDesktops(run);

  // -- console, page errors, CSP --
  assert(live.csp.length === 0, `CSP violations: ${live.csp.join(' | ')}`);
  assert(live.errors.length === 0, `console/page errors: ${live.errors.join(' | ')}`);
  run.note('csp/console', `0 CSP violations, 0 console errors; ${live.sockets.length} WebSocket(s) seen`);
}

// ---- two desktops --------------------------------------------------------------------------------

interface Registration {
  scope: string;
  endpoint: string | null;
  notifications: { tag: string; title: string; body: string; desktop: string | null; navigate: string | null }[];
}

/** The page's service-worker registrations: scope, push subscription and shown notifications. */
async function registrations(page: Page): Promise<Registration[]> {
  return page.evaluate(async () => {
    const out: Registration[] = [];
    for (const registration of await navigator.serviceWorker.getRegistrations()) {
      const subscription = await registration.pushManager.getSubscription();
      const shown = await registration.getNotifications();
      out.push({
        scope: new URL(registration.scope).pathname,
        endpoint: subscription?.endpoint ?? null,
        notifications: shown.map((n) => {
          const data = n.data as { desktop?: string; navigate?: string } | null;
          return { tag: n.tag, title: n.title, body: n.body, desktop: data?.desktop ?? null, navigate: data?.navigate ?? null };
        }),
      });
    }
    return out.sort((a, b) => a.scope.localeCompare(b.scope));
  });
}

/** The paired desktops' ids, oldest pairing first (the app's own IndexedDB). */
async function pairedIds(page: Page): Promise<string[]> {
  return page.evaluate(
    () =>
      new Promise<string[]>((resolve, reject) => {
        const open = indexedDB.open('ompdeck');
        open.onerror = () => reject(open.error);
        open.onsuccess = () => {
          const store = open.result.transaction('pairings').objectStore('pairings');
          const keys = store.getAllKeys();
          const records = store.getAll();
          records.onsuccess = () => {
            const ids = (keys.result as string[]).map((id, i) => ({ id, at: (records.result[i] as { pairedUnixMs: number }).pairedUnixMs }));
            open.result.close();
            resolve(ids.sort((a, b) => a.at - b.at).map((entry) => entry.id));
          };
        };
      }),
  );
}

/** The active desktop's name in the chip. */
async function activeDesktop(page: Page): Promise<string> {
  return (await page.locator('.desktop-chip .desk-name').first().innerText()).trim();
}

/** Waits until the chip names `title`. */
async function waitActive(page: Page, title: string): Promise<void> {
  for (const end = Date.now() + STEP; (await activeDesktop(page).catch(() => '')) !== title; await Bun.sleep(200)) {
    assert(Date.now() < end, `the chip never showed “${title}” (it shows “${await activeDesktop(page).catch(() => '?')}”)`);
  }
}

/** Taps the desktop chip (on a screen that has one) and waits for the switcher. */
async function openSwitcher(run: PhoneRun): Promise<void> {
  await phone.tapNode('the desktop chip', (n) => n.pkg === phone.CHROME && /Switch desktop$/.test(n.desc));
  await run.live.page.getByRole('dialog').waitFor();
}

/** Switches to the desktop named `title` through the switcher: two taps. */
async function switchTo(run: PhoneRun, title: string): Promise<void> {
  await openSwitcher(run);
  await phone.tapNode(`${title} in the switcher`, (n) => n.pkg === phone.CHROME && [n.text, n.desc].some((v) => v.includes(title) && v.includes('This phone:')));
  await waitActive(run.live.page, title);
}

/** The gateway-side truth of which desktop the phone is talking to: that desktop's device is online, the other's is not. */
async function onlyOnline(run: PhoneRun, which: 'A' | 'B'): Promise<void> {
  const row = async (live: LiveRun, fingerprint: string): Promise<boolean> =>
    (await live.ctl<{ devices: DeviceRow[] }>('/mobile')).devices.some((d) => d.fingerprint === fingerprint && d.online);
  const want = which === 'A';
  let seen = '';
  for (const end = Date.now() + STEP; Date.now() < end; await Bun.sleep(300)) {
    const a = await row(run.live, run.fingerprintA);
    const b = await row(run.liveB, run.fingerprintB);
    seen = `A online ${a}, B online ${b}`;
    if (a === want && b === !want) return;
  }
  throw new Error(`expected only desktop ${which} online: ${seen}`);
}

/** What the app and both gateways show now (for a failure message). */
async function diagnose(run: PhoneRun, idA: string, idB: string, navigate: string | null): Promise<string> {
  const page = run.live.page;
  const active = await page
    .evaluate(
      () =>
        new Promise<string | null>((resolve) => {
          const open = indexedDB.open('ompdeck');
          open.onsuccess = () => {
            const get = open.result.transaction('meta').objectStore('meta').get('active');
            get.onsuccess = () => {
              open.result.close();
              resolve((get.result as string | undefined) ?? null);
            };
          };
        }),
    )
    .catch(() => '?');
  const online = async (live: LiveRun, fingerprint: string): Promise<string> =>
    (await live.ctl<{ devices: DeviceRow[] }>('/mobile').then((m) => m.devices.find((d) => d.fingerprint === fingerprint)?.online).catch(() => '?')) + '';
  const text = (await page.locator('body').innerText().catch(() => '')).replace(/\s+/g, ' ').slice(0, 240);
  const controllers = ((await run.liveB.ctl<Record<string, unknown>[]>('/live').catch(() => [])) as Record<string, unknown>[]).map((c) => ({ activity: c.activity, session_path: c.session_path }));
  return `stored active ${active === idA ? 'A' : active === idB ? 'B' : active}; online A ${await online(run.live, run.fingerprintA)} B ${await online(run.liveB, run.fingerprintB)}; B controllers ${JSON.stringify(controllers)}; url ${page.url()}; notification navigate ${navigate}; screen “${text}”`;
}

/** Settings → Notifications on the active desktop; turns the switch on when it is off. */
async function enablePush(run: PhoneRun): Promise<string> {
  const page = run.live.page;
  await phone.tapNode('Settings', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Settings/.test(n.text));
  await phone.tapNodeAboveBar('Notifications', (n) => n.pkg === phone.CHROME && /^Notifications/.test(n.text || n.desc) && n.id === '');
  await page.getByRole('switch', { name: 'Push notifications' }).waitFor();
  assert((await page.getByText('not subscribed', { exact: true }).count()) > 0, 'push is already on for this desktop');
  await phone.tapNode('the Push notifications switch', (n) => n.pkg === phone.CHROME && n.desc === 'Push notifications');
  const prompt = await phone.waitNode('', phone.named('Allow'), 4000).catch(() => null);
  if (prompt !== null) phone.tap(prompt);
  await page.getByText('subscribed', { exact: true }).waitFor();
  return prompt === null ? 'permission already granted' : 'Chrome asked; Allow tapped';
}

/** Step 1: pair desktop B through the app (the phone is on desktop A's session list); the switcher lists and switches both. Ends on A. */
async function pairSecondDesktop(run: PhoneRun): Promise<void> {
  const { live, liveB, stack, second } = run;
  const page = live.page;
  const lap = run.lap;
  run.startLap();

  // -- 1. pair desktop B through the app: chip -> "Pair another desktop" -> the link typed with the keyboard --
  await page.getByRole('button', { name: `New session in ${stack.project}` }).waitFor();
  // Both gateways are called “OmpDeck on <this host>”: once two are paired the switcher tells them apart by their key.
  run.plainA = await activeDesktop(page);
  await openSwitcher(run);
  assert((await page.locator('.desktop-option').count()) === 1, 'the switcher lists more than the one desktop before pairing another');
  run.shot('12-switcher-one-desktop');
  await phone.tapNode('Pair another desktop', phone.named('Pair another desktop'));
  await page.getByRole('heading', { name: 'Pair another desktop' }).waitFor();
  const invite = second.link.split('#pair=')[1];
  assert(invite !== undefined && invite.startsWith('v1.'), `the second desktop's link: ${second.link}`);
  const linkField = phone.nodes().filter((n) => n.pkg === phone.CHROME && n.cls === 'android.widget.EditText' && n.id === '').sort((a, b) => a.bounds[1] - b.bounds[1])[0];
  assert(linkField !== undefined, 'the pairing screen shows no link field');
  phone.tap(linkField);
  for (const end = Date.now() + 5000; !phone.keyboardShown(); await Bun.sleep(200)) assert(Date.now() < end, 'the keyboard did not open on the link field');
  phone.typeText(invite);
  const typed = await page.getByLabel('Pairing link').inputValue();
  assert(typed === invite, `the link field shows ${typed.length} characters, typed ${invite.length}`);
  if (phone.keyboardShown()) phone.key('KEYCODE_BACK');
  await page.getByText(/stays paired/).waitFor();
  run.shot('13-pair-another-desktop');
  await phone.tapNode('Pair this desktop', phone.named('Pair this desktop'));
  await page.getByText('Approve on your desktop').waitFor();
  run.fingerprintB = (await page.locator('.fingerprint').innerText()).trim();
  let shown: string | null = null;
  for (const end = Date.now() + STEP; shown === null; ) {
    shown = (await liveB.ctl<{ fingerprint: string | null }>('/pairing')).fingerprint;
    if (shown === null) {
      assert(Date.now() < end, 'desktop B never showed an approval request');
      await Bun.sleep(100);
    }
  }
  assert(shown === run.fingerprintB, `fingerprints differ: phone ${run.fingerprintB}, desktop B ${shown}`);
  await liveB.ctl('/approve', { method: 'POST', body: run.fingerprintB });
  await page.getByText(/Connected/).first().waitFor();
  const titleB = await activeDesktop(page);
  await onlyOnline(run, 'B');
  run.shot('14-paired-b');

  // -- the switcher lists both and switches --
  await openSwitcher(run);
  const titles = await page.locator('.desktop-option b').allInnerTexts();
  const [titleA] = titles;
  assert(titles.length === 2 && titleA !== undefined && titles[1] === titleB && titleA !== titleB, `the switcher lists ${JSON.stringify(titles)}, the chip shows “${titleB}”`);
  lap('pair B');
  run.note('two desktops (pair B)', `“${titleB}” paired through the switcher next to “${titleA}” (link typed with the keyboard, fingerprints matched); only B is connected`);
  assert((await page.locator('.desktop-option[aria-pressed="true"]').innerText()).includes(titleB), 'the active desktop is not marked');
  run.shot('15-switcher-two-desktops');
  await phone.tapNode(`${titleA} in the switcher`, (n) => n.pkg === phone.CHROME && [n.text, n.desc].some((v) => v.includes(titleA) && v.includes('This phone:')));
  await waitActive(page, titleA);
  await onlyOnline(run, 'A');
  await page.getByRole('button', { name: `New session in ${stack.project}` }).waitFor();
  await switchTo(run, titleB);
  await onlyOnline(run, 'B');
  await switchTo(run, titleA);
  await onlyOnline(run, 'A');
  lap('switch A, B, A');
  run.titleA = titleA;
  run.titleB = titleB;
  run.note('two desktops (switch)', `the switcher lists both with B marked; A, B, A by tap: the chip follows and the gateways show only the active one online`);
}

/** Steps 2-5: push on both desktops, a turn finishing on the inactive one, the tap, forgetting B. Starts on A's conversation. */
async function pushAcrossDesktops(run: PhoneRun): Promise<void> {
  const { live, liveB, stack } = run;
  const page = live.page;
  const lap = run.lap;
  run.startLap();
  const { titleA, titleB } = run;
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await page.getByRole('button', { name: `New session in ${stack.project}` }).waitFor();
  await switchTo(run, titleB);
  await onlyOnline(run, 'B');

  // -- 2. notifications on both: a registration each, real FCM subscriptions --
  const how = await enablePush(run);
  const [idA, idB] = await pairedIds(page);
  assert(idA !== undefined && idB !== undefined, 'the phone has fewer than two pairings');
  const regs = await registrations(page);
  const subscribed = regs.filter((r) => r.endpoint !== null);
  assert(
    subscribed.length === 2 && subscribed.every((r) => r.endpoint?.startsWith('https://fcm.googleapis.com/') === true) && JSON.stringify(subscribed.map((r) => r.scope).sort()) === JSON.stringify([`/d/${idA}/`, `/d/${idB}/`].sort()),
    `push registrations: ${JSON.stringify(regs)}`,
  );
  assert(regs.find((r) => r.scope === '/')?.endpoint == null, 'the root registration holds a subscription');
  await liveB.devicePush(run.fingerprintB, true);
  await live.devicePush(run.fingerprintA, true);
  run.shot('16-push-both');
  lap('push both');
  run.note('two desktops (push)', `B: ${how}; /d/${idA.slice(0, 8)}… and /d/${idB.slice(0, 8)}… each hold an FCM subscription (${new URL(subscribed[0]?.endpoint ?? 'x:').host}); the root registration none; each gateway shows its own device with push on`);

  // -- 3. a turn finishes on B while A is active and on screen --
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await phone.tapNode('Sessions', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Sessions/.test(n.text));
  await phone.tapNode('New session', phone.named(`New session in ${stack.project}`));
  await run.idle();
  assert(phone.notifications(stack.origin).length === 0, `notifications before the turn: ${JSON.stringify(phone.notifications(stack.origin))}`);
  // The push's turn is the session's first: the gateway must learn the fresh session's file from live control
  // (omp holds no handle on it after the title rewrite), or the tap below could only open “Starting the session…”.
  await run.send('PUSH-SWITCH-MARK finish on the other desktop');
  const sentAt = Date.now();
  await page.locator('.composer.mode-steer').waitFor();
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await page.getByRole('button', { name: `New session in ${stack.project}` }).waitFor();
  await switchTo(run, titleA);
  await onlyOnline(run, 'A');
  const busy = await liveB.ctl<{ activity: string; label: string }[]>('/live');
  assert(busy.some((c) => c.activity === 'Busy'), `B's turn ended before the phone switched to A: ${JSON.stringify(busy.map((c) => [c.label, c.activity]))}`);
  const switchedIn = ((Date.now() - sentAt) / 1000).toFixed(1);
  run.shot('17-on-a-while-b-works');
  let pushed: phone.Shown[] = [];
  for (const end = Date.now() + 90_000; pushed.length === 0; await Bun.sleep(500)) {
    pushed = phone.notifications(stack.origin);
    assert(pushed.length > 0 || Date.now() < end, 'no notification for the turn that finished on desktop B within 90 s');
  }
  const arrived = ((Date.now() - sentAt) / 1000).toFixed(1);
  await Bun.sleep(3000);
  pushed = phone.notifications(stack.origin);
  const titleOfB = titleWithLabel('OmpDeck', titleB);
  assert(pushed.length === 1 && pushed[0]?.title === titleOfB && pushed[0].text === 'A session finished', `the notification(s) (the title must name desktop B “${titleB}”): ${JSON.stringify(pushed)}`);
  // Whose it is: shown by desktop B's registration, never A's.
  const after = await registrations(page);
  const fromB = after.find((r) => r.scope === `/d/${idB}/`);
  const fromA = after.find((r) => r.scope === `/d/${idA}/`);
  assert(fromB?.notifications.length === 1 && fromB.notifications[0]?.title === titleOfB && fromB.notifications[0].desktop === idB && fromB.notifications[0].tag.startsWith(`${idB}:`), `desktop B's notifications: ${JSON.stringify(fromB)}`);
  assert(fromA?.notifications.length === 0, `desktop A's notifications: ${JSON.stringify(fromA)}`);
  assert((await activeDesktop(page)) === titleA, 'the app left desktop A before the notification was tapped');
  lap('turn finishes on B');
  run.note('two desktops (push for B)', `turn started on B, phone switched to A after ${switchedIn} s while B was busy; one push “${pushed[0]?.title}” / “${pushed[0]?.text}” arrived ${arrived} s after the prompt, shown by B's registration (tag ${fromB.notifications[0]?.tag.slice(0, 13)}…), none by A's`);

  // -- 4. tapping it switches to B and opens that session --
  phone.adb('shell', 'cmd', 'statusbar', 'expand-notifications');
  const host = new URL(stack.origin).host;
  const row = await phone.waitNode('the notification in the shade', (n) => n.text === host);
  run.shot('18-notification-for-b');
  // The shade itself shows which desktop it is from (and not the other one).
  const shade = phone.nodes().filter((n) => n.pkg === 'com.android.systemui').map((n) => n.text);
  assert(shade.some((t) => t === titleOfB), `the shade does not show “${titleOfB}”: ${JSON.stringify(shade.filter((t) => t.startsWith('OmpDeck')))}`);
  assert(!shade.some((t) => t === titleWithLabel('OmpDeck', titleA)), 'the shade shows a notification for desktop A');
  const body = phone.nodes().find((n) => n.text === pushed[0]?.text && n.bounds[1] >= row.bounds[3] && n.bounds[1] - row.bounds[3] < 320);
  assert(body !== undefined, 'the notification text is not in the shade');
  phone.tap(body);
  try {
    await page.getByText('SWITCH-REPLY finished on the other desktop').first().waitFor({ timeout: 30_000 });
  } catch (error) {
    throw new Error(`${error instanceof Error ? error.message.split('\n')[0] : String(error)}; ${await diagnose(run, idA, idB, fromB.notifications[0]?.navigate ?? null)}`);
  }
  await onlyOnline(run, 'B');
  await run.idle();
  assert(phone.notifications(stack.origin).length === 0, 'the notification stayed after it was opened');
  run.shot('19-opened-on-b');
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await waitActive(page, titleB);
  lap('tap switches to B');
  run.note('two desktops (tap)', `tapping it switched the app from A to B (chip “${titleB}”; gateways: only B online) and opened B's session with its reply`);

  // -- 5. forgetting B leaves A working --
  await phone.tapNode('Settings', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Settings/.test(n.text));
  await phone.tapNodeAboveBar('This device', (n) => n.pkg === phone.CHROME && /^This device/.test(n.text || n.desc) && n.id === '');
  await phone.tapNode('Forget on this phone only', phone.named('Forget on this phone only'));
  await phone.tapNode('Forget', (n) => n.pkg === phone.CHROME && (n.text === 'Forget' || n.desc === 'Forget') && n.cls === 'android.widget.Button');
  // Alone again, A needs no key suffix.
  await waitActive(page, run.plainA);
  await page.getByText(/Connected/).first().waitFor();
  const left = await registrations(page);
  assert(!left.some((r) => r.scope === `/d/${idB}/`), `B's registration survived forgetting it: ${JSON.stringify(left)}`);
  assert(left.find((r) => r.scope === `/d/${idA}/`)?.endpoint?.startsWith('https://fcm.googleapis.com/') === true, `A's subscription after forgetting B: ${JSON.stringify(left)}`);
  assert(JSON.stringify(await pairedIds(page)) === JSON.stringify([idA]), 'the phone still holds a pairing for B');
  run.shot('20-forgot-b');
  // A works: a new session, a prompt and its reply.
  await phone.tapNode('Sessions', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Sessions/.test(n.text));
  await phone.tapNode('New session', phone.named(`New session in ${stack.project}`));
  await run.idle();
  await run.send('FORGET-MARK desktop A still works');
  await live.transcript(/LIVE-REPLY to: FORGET-MARK desktop A still works/);
  await onlyOnline(run, 'A');
  run.shot('21-a-works');
  lap('forget B');
  run.note('two desktops (forget)', `B forgotten on the phone: its pairing and /d/${idB.slice(0, 8)}… registration are gone, the app is on “${titleA}”, whose FCM subscription stays and which answered a new prompt`);
  console.log(`phone: two-desktop timings (ms): ${run.timings.map(([name, ms]) => `${name} ${ms}`).join(', ')}`);
}
