// End-to-end run of the phone app against the smoke harness (test/smoke): real build, real Noise
// pairing through the fake relay, real wire codec. `bun run e2e` (exit code 1 on any failure).
//
// Kept outside `bun test` (no `.test.ts` name): it needs Chromium and takes ~30 s.

import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { BrowserContext, Page } from 'playwright-core';
import { launch, persistentPhone, phone } from '../test/smoke/browser.ts';
import { startSmoke } from '../test/smoke/server.ts';
import type { api } from '../src/remote/index.ts';
import * as fixtures from '../test/smoke/fixtures.ts';

const OUT = process.env['E2E_OUT'] ?? '/tmp/pwa-smoke/e2e';
const TIMEOUT = 15_000;
mkdirSync(OUT, { recursive: true });

const smoke = await startSmoke();
const browser = await launch();
const failures: string[] = [];

/**
 * Runs one scenario in a fresh phone context; it fails on any page or console error, and a
 * failure is recorded with a screenshot.
 */
async function scenario(
  name: string,
  hash: string,
  body: (page: Page, context: BrowserContext) => Promise<void>,
  blockWorkers = false,
  /** A real (non-incognito) profile: the Push API exists there, as on a phone. */
  realProfile = false,
): Promise<void> {
  const profile = realProfile ? mkdtempSync(join(tmpdir(), 'pwa-e2e-profile-')) : null;
  const context = profile === null ? await phone(browser, blockWorkers) : await persistentPhone(profile);
  context.setDefaultTimeout(TIMEOUT);
  const page = await context.newPage();
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(message.text());
  });
  const started = performance.now();
  try {
    await page.goto(smoke.url(hash));
    await body(page, context);
    if (errors.length > 0) throw new Error(`console errors: ${errors.join(' | ')}`);
    console.log(`✓ ${name} (${Math.round(performance.now() - started)} ms)`);
  } catch (error) {
    const shot = join(OUT, `${name.replace(/\W+/g, '-')}.png`);
    await page.screenshot({ path: shot }).catch(() => undefined);
    failures.push(name);
    console.error(`✗ ${name}: ${error instanceof Error ? error.message : String(error)}\n  screenshot: ${shot}`);
  } finally {
    await context.close();
    if (profile !== null) rmSync(profile, { recursive: true, force: true });
  }
}

async function openSession(page: Page, title: string): Promise<void> {
  await page.getByText(title, { exact: true }).first().click();
  await page.locator('.composer').waitFor();
}

try {
  await scenario('pair with an invite link', `pair=${smoke.invite}`, async (page) => {
    await page.getByRole('button', { name: 'Pair this phone' }).click();
    await page.getByText('Approve on your desktop').waitFor();
    // The harness approves after ~1 s; the app lands on the session list, online.
    await page.getByText('Refactor tmux launcher').first().waitFor();
    await page.getByText(/Connected/).first().waitFor();
  });

  // -- several desktops: one pairing each, one live connection (the active desktop's) --
  const chip = (page: Page, name: string) => page.locator('.desktop-chip', { hasText: name }).first();
  const gatewayConnections = async (page: Page, expected: { first: number; second: number }): Promise<void> => {
    const deadline = Date.now() + TIMEOUT;
    while (smoke.first.gateway.connections !== expected.first || smoke.second.gateway.connections !== expected.second) {
      if (Date.now() > deadline) {
        throw new Error(`device connections: ${smoke.first.label} ${smoke.first.gateway.connections}, ${smoke.second.label} ${smoke.second.gateway.connections}; wanted ${JSON.stringify(expected)}`);
      }
      await page.waitForTimeout(100);
    }
  };
  const openSwitcher = async (page: Page): Promise<void> => {
    await page.locator('.desktop-chip').first().click();
    await page.getByRole('dialog').getByText('Pair another desktop').waitFor();
  };

  await scenario('pair a second desktop, switch between both', 'route=sessions', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
    await openSwitcher(page);
    await page.getByRole('dialog').getByRole('button', { name: 'Pair another desktop' }).click();
    await page.getByRole('heading', { name: 'Pair with your desktop' }).waitFor({ state: 'detached' }).catch(() => undefined);
    await page.getByRole('heading', { name: 'Pair another desktop' }).waitFor();
    await page.getByLabel('Pairing link').fill(smoke.second.invite);
    await page.getByText(/Workstation stays paired/).waitFor();
    await page.getByRole('button', { name: 'Pair this desktop' }).click();
    await page.getByText('Approve on your desktop').waitFor();
    await chip(page, 'Laptop').waitFor();
    await page.getByText('Refactor tmux launcher').first().waitFor();
    // The Workstation's connection closed when the Laptop's opened.
    await gatewayConnections(page, { first: 0, second: 1 });

    // Both are listed, the Laptop marked; a tap switches back, with a fresh session list from the Workstation.
    await openSwitcher(page);
    const dialog = page.getByRole('dialog');
    await dialog.locator('button.desktop-option[aria-pressed="true"]', { hasText: 'Laptop' }).waitFor();
    await dialog.locator('button.desktop-option[aria-pressed="false"]', { hasText: 'Workstation' }).click();
    await chip(page, 'Workstation').waitFor();
    await page.getByText('Refactor tmux launcher').first().waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });

    // Settings lists the desktops too.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
    await page.locator('button.desktop-row', { hasText: 'Laptop' }).click();
    await chip(page, 'Laptop').waitFor();
    await gatewayConnections(page, { first: 0, second: 1 });
  });

  await scenario('a screen is not carried over to the other desktop', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await openSession(page, 'Refactor tmux launcher');
    await page.getByLabel('Message').fill('draft for the workstation');
    // A conversation is a pushed screen without the desktop chip: go back, then switch from Settings.
    await page.getByRole('button', { name: 'Back' }).click();
    await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
    await page.locator('button.desktop-row', { hasText: 'Laptop' }).click();
    await chip(page, 'Laptop').waitFor();
    if ((await page.locator('.composer').count()) !== 0) throw new Error('the other desktop opened with the previous conversation');
    if ((await page.getByText('draft for the workstation').count()) !== 0) throw new Error('a draft followed the switch');
  });

  await scenario('re-pairing a paired desktop from an invite link replaces it, others stay', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await page.evaluate((invite) => {
      location.hash = `#pair=${invite}`;
    }, smoke.first.invite);
    await page.getByRole('heading', { name: 'Pair again with Workstation' }).waitFor();
    await page.getByText(/replaces this phone’s pairing with Workstation/).waitFor();
    // Nothing dropped yet: the live connection is still the Workstation's.
    await gatewayConnections(page, { first: 1, second: 0 });
    await page.getByRole('button', { name: 'Replace pairing' }).click();
    await page.getByText('Approve on your desktop').waitFor();
    await chip(page, 'Workstation').waitFor();
    await openSwitcher(page);
    const options = page.getByRole('dialog').locator('button.desktop-option');
    const names = await options.allInnerTexts();
    if (names.length !== 2 || names.filter((text) => text.includes('Workstation')).length !== 1 || names.filter((text) => text.includes('Laptop')).length !== 1) {
      throw new Error(`desktops after re-pairing: ${JSON.stringify(names)}`);
    }
  });

  await scenario('an invite link for a new desktop opened while the app runs keeps the others', 'route=sessions', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await page.evaluate((invite) => {
      location.hash = `#pair=${invite}`;
    }, smoke.second.invite);
    await page.getByRole('heading', { name: 'Pair another desktop' }).waitFor();
    await page.getByText(/Workstation stays paired/).waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
    // Cancel: back to the Workstation, still connected.
    await page.getByRole('button', { name: 'Cancel' }).click();
    await chip(page, 'Workstation').waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
  });

  await scenario('forget one desktop: the next one becomes active; forget the last: pairing screen', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    const forgetHere = async (): Promise<void> => {
      await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
      await page.locator('button.hub-row', { hasText: 'This device' }).click();
      await page.getByRole('button', { name: 'Forget on this phone only' }).click();
      await page.getByRole('button', { name: 'Forget', exact: true }).click();
    };
    await forgetHere();
    await chip(page, 'Laptop').waitFor();
    await gatewayConnections(page, { first: 0, second: 1 });
    await page.getByText('Refactor tmux launcher').first().waitFor().catch(() => undefined);
    await forgetHere();
    await page.getByRole('heading', { name: 'Pair with your desktop' }).waitFor();
    await gatewayConnections(page, { first: 0, second: 0 });
  });

  await scenario('a notification tap for the other desktop switches to it and opens its session', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    // What the service worker opens (or posts) for a push from the Laptop: its id first, then the route.
    await page.evaluate((id) => {
      location.hash = `#d=${id}&s=s-convo`;
    }, smoke.second.id);
    await page.locator('.composer').waitFor();
    await gatewayConnections(page, { first: 0, second: 1 });
    await page.getByRole('button', { name: 'Back' }).click();
    await chip(page, 'Laptop').waitFor();
    // A link naming a desktop this phone is not paired with is ignored: still on the Laptop, nothing opened.
    await page.evaluate(() => {
      location.hash = `#d=${'ee'.repeat(32)}&s=s-idle`;
    });
    await page.waitForTimeout(300);
    await chip(page, 'Laptop').waitFor();
    if ((await page.locator('.composer').count()) !== 0) throw new Error('a link for an unpaired desktop opened a conversation');
    await gatewayConnections(page, { first: 0, second: 1 });
  });

  await scenario('a version 1 pairing migrates without pairing again', 'route=sessions&legacy=1', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await page.getByText('Refactor tmux launcher').first().waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
  });

  // -- the upgrade while the old app is still open in another window --
  // Version 1 keeps its IndexedDB connection for the page's life and cannot be told to let go, so it
  // blocks the new version's upgrade of the database. The holder page (test/smoke/holder.ts) stands
  // in for it. The first page of each scenario installs the worker, as the app does on its first run.
  const holderUrl = new URL('/__smoke/holder', smoke.url('')).href;
  const controlled = (page: Page) => page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  const watch = (page: Page, errors: string[]) => {
    page.on('pageerror', (error) => errors.push(error.message));
    page.on('console', (message) => {
      if (message.type() === 'error') errors.push(message.text());
    });
  };

  await scenario('an old window holding the database is reloaded by the worker, and the new app boots paired', 'route=sessions', async (page, context) => {
    await controlled(page);
    // This window becomes the old app: it drops the new database and holds version 1 instead.
    await page.goto(holderUrl);
    await page.locator('body[data-holding="yes"]').waitFor({ state: 'attached' });
    if (!(await page.evaluate(() => navigator.serviceWorker.controller !== null))) throw new Error('the old window is not controlled by the worker');
    const errors: string[] = [];
    const fresh = await context.newPage();
    watch(fresh, errors);
    await fresh.goto(smoke.url('route=sessions&legacy=1'));
    await chip(fresh, 'Workstation').waitFor();
    await fresh.getByText('Refactor tmux launcher').first().waitFor();
    // The worker reloaded the old window: it ran again and let go.
    await page.waitForFunction(() => document.body?.dataset['loads'] === '2');
    if (errors.length > 0) throw new Error(`console errors: ${errors.join(' | ')}`);
  });

  await scenario('an old window the worker cannot reload: the app says it is finishing the update and boots once that window closes', 'route=sessions', async (page, context) => {
    await controlled(page);
    await page.goto('about:blank');
    // Not controlled by the worker (it is bypassed for this page), so it cannot be navigated.
    const old = await context.newPage();
    const session = await context.newCDPSession(old);
    await session.send('Network.enable');
    await session.send('Network.setBypassServiceWorker', { bypass: true });
    await old.goto(holderUrl);
    await old.locator('body[data-holding="yes"]').waitFor({ state: 'attached' });
    if (await old.evaluate(() => navigator.serviceWorker.controller !== null)) throw new Error('the old window is controlled by the worker');
    const errors: string[] = [];
    const fresh = await context.newPage();
    watch(fresh, errors);
    await fresh.goto(smoke.url('route=sessions&legacy=1'));
    await fresh.getByRole('heading', { name: 'Finishing the update…' }).waitFor();
    await fresh.getByText('close the other OmpDeck tabs').waitFor();
    // Give the worker its chance: the window stays as it was.
    await fresh.waitForTimeout(500);
    if ((await old.evaluate(() => document.body.dataset['loads'])) !== '1') throw new Error('the worker reloaded a window it does not control');
    if ((await fresh.locator('.desktop-chip').count()) !== 0) throw new Error('the app booted while the database was blocked');
    await old.close();
    await chip(fresh, 'Workstation').waitFor();
    await fresh.getByText('Refactor tmux launcher').first().waitFor();
    if (errors.length > 0) throw new Error(`console errors: ${errors.join(' | ')}`);
  });

  // -- Web Push per desktop: each desktop's subscription is on its own registration (/d/<id>/) --
  await scenario(
    'push: each desktop subscribes on its own service-worker registration; forgetting one removes only its own',
    'route=settings&desktops=2',
    async (page, context) => {
      await context.grantPermissions(['notifications'], { origin: smoke.origin });
      // The real push service is out of reach of headless Chromium: one fake subscription per registration's PushManager.
      await page.evaluate(() => {
        const subs = new WeakMap<object, PushSubscription>();
        const make = (n: number, key: ArrayBuffer): PushSubscription =>
          ({
            endpoint: `https://push.example/${n}`,
            expirationTime: null,
            options: { userVisibleOnly: true, applicationServerKey: key },
            getKey: () => new Uint8Array([1, 2, 3]).buffer,
            unsubscribe: async function (this: PushSubscription) {
              for (const manager of [...registry]) if (subs.get(manager) === this) subs.delete(manager);
              return true;
            },
            toJSON: () => ({ endpoint: `https://push.example/${n}`, expirationTime: null, keys: { p256dh: 'AQID', auth: 'AQID' } }),
          }) as unknown as PushSubscription;
        const registry = new Set<object>();
        let count = 0;
        PushManager.prototype.subscribe = async function (options?: PushSubscriptionOptionsInit): Promise<PushSubscription> {
          registry.add(this);
          const key = options?.applicationServerKey;
          const buffer = key instanceof ArrayBuffer ? key : key !== null && key !== undefined && typeof key !== 'string' ? (key as ArrayBufferView).buffer.slice(0) as ArrayBuffer : new ArrayBuffer(0);
          const made = make(++count, buffer);
          subs.set(this, made);
          return made;
        };
        PushManager.prototype.getSubscription = async function (): Promise<PushSubscription | null> {
          return subs.get(this) ?? null;
        };
      });
      const scopes = () =>
        page.evaluate(async () => {
          const found: { scope: string; subscribed: boolean }[] = [];
          for (const registration of await navigator.serviceWorker.getRegistrations()) {
            found.push({ scope: new URL(registration.scope).pathname, subscribed: (await registration.pushManager.getSubscription()) !== null });
          }
          return found.sort((a, b) => a.scope.localeCompare(b.scope));
        });
      const notifications = async (): Promise<void> => {
        await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
        await page.locator('button.hub-row', { hasText: 'Notifications' }).click();
        await page.getByRole('switch', { name: 'Push notifications' }).waitFor();
      };

      await chip(page, 'Workstation').waitFor();
      await page.evaluate(() => navigator.serviceWorker.ready);
      await notifications();
      await page.getByRole('switch', { name: 'Push notifications' }).click();
      await page.getByText('subscribed', { exact: true }).waitFor();
      let found = await scopes();
      const subscribed = found.filter((entry) => entry.subscribed).map((entry) => entry.scope);
      if (subscribed.length !== 1 || subscribed[0] !== `/d/${smoke.first.id}/`) throw new Error(`after enabling push for the Workstation: ${JSON.stringify(found)}`);

      // The Laptop is its own desktop: not subscribed until its own switch is turned on.
      await page.getByRole('button', { name: 'Back' }).click();
      await page.locator('button.desktop-row', { hasText: 'Laptop' }).click();
      await chip(page, 'Laptop').waitFor();
      await notifications();
      await page.getByText('not subscribed', { exact: true }).waitFor();
      await page.getByRole('switch', { name: 'Push notifications' }).click();
      await page.getByText('subscribed', { exact: true }).waitFor();
      found = await scopes();
      const both = found.filter((entry) => entry.subscribed).map((entry) => entry.scope).sort();
      if (JSON.stringify(both) !== JSON.stringify([`/d/${smoke.first.id}/`, `/d/${smoke.second.id}/`].sort())) throw new Error(`after enabling push for both: ${JSON.stringify(found)}`);

      // Forget the Laptop: its registration goes, the Workstation's stays subscribed.
      await page.getByRole('button', { name: 'Back' }).click();
      await page.locator('button.hub-row', { hasText: 'This device' }).click();
      await page.getByRole('button', { name: 'Forget on this phone only' }).click();
      await page.getByRole('button', { name: 'Forget', exact: true }).click();
      await chip(page, 'Workstation').waitFor();
      const deadline = Date.now() + TIMEOUT;
      for (;;) {
        found = await scopes();
        if (!found.some((entry) => entry.scope === `/d/${smoke.second.id}/`)) break;
        if (Date.now() > deadline) throw new Error(`the Laptop's registration survived forgetting it: ${JSON.stringify(found)}`);
        await page.waitForTimeout(100);
      }
      const left = found.filter((entry) => entry.subscribed).map((entry) => entry.scope);
      if (left.length !== 1 || left[0] !== `/d/${smoke.first.id}/`) throw new Error(`after forgetting the Laptop: ${JSON.stringify(found)}`);
    },
    false,
    true,
  );

  await scenario('send a prompt and see the reply', 'route=sessions', async (page) => {
    await openSession(page, 'Refactor tmux launcher');
    await page.getByLabel('Message').fill('Please split launch.rs');
    await page.locator('.composer button.send').click();
    await page.getByText('Please split launch.rs').first().waitFor();
    await page.getByText('Starting with the files you mentioned.').first().waitFor();
  });

  await scenario('answer a pending confirmation', 'route=conversation&session=s-ui-confirm&title=Clean%20old%20worktrees', async (page) => {
    const card = page.locator('.ui-card');
    await card.first().waitFor();
    await card.getByRole('button', { name: '✓ Yes' }).click();
    await card.first().waitFor({ state: 'detached' });
  });

  await scenario('download: transcript image and HTML export', 'route=conversation&session=s-convo&title=Ship%20mobile%20relay', async (page) => {
    // The transcript image is pulled chunk by chunk and shown as a thumbnail.
    await page.locator('button.img-thumb img').first().waitFor();
    await page.getByRole('button', { name: 'Back' }).click();
    await openSession(page, 'Refactor tmux launcher');
    await page.getByRole('button', { name: 'Session actions' }).click();
    const [file] = await Promise.all([page.waitForEvent('download'), page.getByText('Export as HTML').click()]);
    if (!file.suggestedFilename().endsWith('.html')) throw new Error(`export saved as ${file.suggestedFilename()}`);
    const path = await file.path();
    const html = await Bun.file(path).text();
    if (!html.includes('<html')) throw new Error('the export is not HTML');
  });

  await scenario('attach a file and send it', 'route=conversation&session=s-idle&title=Refactor%20tmux%20launcher', async (page) => {
    await page.locator('.composer input[type=file]:not([capture])').setInputFiles({
      name: 'notes.txt',
      mimeType: 'text/plain',
      buffer: Buffer.alloc(700_000, 'omp '),
    });
    await page.locator('.attach-chip.done').waitFor();
    await page.getByLabel('Message').fill('Read the notes');
    await page.locator('.composer button.send').click();
    await page.getByText('I can see notes.txt').first().waitFor();
  });

  for (const section of ['backup', 'sync'] as const) {
    await scenario(`${section}: destination switches reset selection and fence old replies`, `route=settings&section=${section}`, async (page) => {
      const gw = smoke.gateway;
      const reply = (marker: string): api.Resp => section === 'backup'
        ? { type: 'backup_list', hosts: [{ host: marker, backups: [{ id: marker, name: marker, size: 1, created_unix_ms: gw.t0 }] }], truncated: false }
        : { type: 'sync_backups', backups: [{ ...fixtures.syncBackups(gw.t0, null)[0]!, id: marker, name: marker, host: marker }], truncated: false };
      const reload = page.getByRole('button', { name: section === 'backup' ? 'Reload versions' : 'Reload backups', exact: true });
      await page.locator('.version .check').first().click();
      await page.locator('.select-bar').waitFor();
      const held = Promise.withResolvers<api.Resp>();
      const pending = Promise.withResolvers<void>();
      gw.backupListing = () => {
        pending.resolve();
        return held.promise;
      };
      await reload.click();
      const timer = setTimeout(() => pending.reject(new Error('old listing was not requested')), TIMEOUT);
      try {
        await pending.promise;
      } finally {
        clearTimeout(timer);
      }
      try {
        for (const [index, destination] of [
          { kind: 'sftp', label: 'sftp://first.example' },
          { kind: 'sftp', label: 'sftp://second.example' },
        ].entries()) {
          const marker = `destination-version-${index}`;
          gw.backupListing = () => reply(marker);
          gw.backupDestination = destination as api.BackupDestinationView;
          // Do not change timestamps: kind/label alone must cause a fresh listing.
          gw.changed('backup');
          await page.getByRole('button', { name: `Select ${marker}`, exact: true }).waitFor();
          if (await page.locator('.select-bar').count() !== 0) throw new Error('selection survived a destination switch');
          if (index === 0) {
            held.resolve(reply('stale-version'));
            await page.waitForTimeout(250);
            if (await page.getByRole('button', { name: 'Select stale-version', exact: true }).count() !== 0) throw new Error('old response replaced destination listing');
          }
          await page.getByRole('button', { name: `Select ${marker}`, exact: true }).click();
          await page.locator('.select-bar').waitFor();
        }
      } finally {
        gw.backupListing = null;
        gw.backupDestination = { kind: 'drive', label: 'Google Drive' };
        gw.changed('backup');
      }
    });
  }

  await scenario('terminal input reaches the desktop and echoes', 'route=terminal&live=tmux%3A8%3Aomp-deck%3Aomp-3c4d&title=Refactor%20tmux%20launcher', async (page) => {
    await page.getByRole('button', { name: /Observe/ }).click();
    await page.locator('button.option', { hasText: 'Type into the session' }).click();
    const input = page.locator('form.term-input input');
    await input.fill('git status');
    await input.press('Enter');
    const deadline = Date.now() + TIMEOUT;
    while (!smoke.gateway.terminalInput.includes('git status\r')) {
      if (Date.now() > deadline) throw new Error(`the desktop got ${JSON.stringify(smoke.gateway.terminalInput)}`);
      await page.waitForTimeout(100);
    }
    await page.locator('.term').getByText('omp is thinking').first().waitFor();
  });

  await scenario(
    'an older desktop without service workers shows the update hint',
    'route=settings&section=usage&gw=v1',
    async (page) => {
      await page.locator('.needs-s4').first().waitFor();
    },
    true,
  );
} finally {
  await browser.close();
  await smoke.stop();
}

if (failures.length > 0) {
  console.error(`${failures.length} e2e scenario(s) failed`);
  process.exit(1);
}
console.log('e2e: all scenarios passed');
