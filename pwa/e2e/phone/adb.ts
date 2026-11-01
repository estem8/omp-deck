// The Android side of the phone gate, through adb (`$ANDROID_SERIAL` picks the device when
// several are connected): physical taps on what the screen shows (uiautomator nodes, i.e. the
// accessibility tree Chrome exposes for the page), key events through the real input stack,
// the system's notification list and screenshots.

import { writeFileSync } from 'node:fs';

export const CHROME = 'com.android.chrome';

/** Runs adb; throws with its output when it fails. */
export function adb(...args: string[]): string {
  const result = Bun.spawnSync(['adb', ...args], { stdout: 'pipe', stderr: 'pipe' });
  const out = result.stdout.toString();
  if (result.exitCode !== 0) throw new Error(`adb ${args.join(' ')}: ${out}${result.stderr.toString()}`.trim());
  return out;
}

/** The device must be connected, awake, unlocked and kept awake while plugged in. */
export function preflight(): string {
  if (Bun.which('adb') === null) throw new Error('adb is not installed');
  const state = Bun.spawnSync(['adb', 'get-state'], { stdout: 'pipe', stderr: 'pipe' });
  if (state.exitCode !== 0) throw new Error(`no Android device: ${state.stderr.toString().trim()} (connect one with USB debugging on; ANDROID_SERIAL picks one of several)`);
  if (!adb('shell', 'pm', 'list', 'packages', CHROME).includes(`package:${CHROME}`)) throw new Error('Chrome is not installed on the phone');
  if (!/mWakefulness=Awake/.test(adb('shell', 'dumpsys', 'power'))) throw new Error('the phone screen is off: wake and unlock it');
  if (/isKeyguardShowing=true/.test(adb('shell', 'dumpsys', 'window'))) throw new Error('the phone is locked: unlock it');
  const stayOn = Number(adb('shell', 'settings', 'get', 'global', 'stay_on_while_plugged_in').trim());
  if (!(stayOn > 0)) throw new Error('the screen may turn off and lock during the run: turn on Developer options → Stay awake');
  const model = adb('shell', 'getprop', 'ro.product.model').trim();
  const release = adb('shell', 'getprop', 'ro.build.version.release').trim();
  const chrome = /versionName=(\S+)/.exec(adb('shell', 'dumpsys', 'package', CHROME))?.[1] ?? '?';
  return `${model}, Android ${release}, Chrome ${chrome}`;
}

export function screenshot(path: string): void {
  const result = Bun.spawnSync(['adb', 'exec-out', 'screencap', '-p'], { stdout: 'pipe' });
  if (result.exitCode !== 0) throw new Error('adb screencap failed');
  writeFileSync(path, result.stdout);
}

export interface Node {
  text: string;
  desc: string;
  cls: string;
  id: string;
  pkg: string;
  enabled: boolean;
  checked: boolean;
  /** Screen pixels: left, top, right, bottom. */
  bounds: [number, number, number, number];
}

function unescapeXml(text: string): string {
  return text.replace(/&(#x?[0-9a-fA-F]+|amp|lt|gt|quot|apos);/g, (_, entity: string) => {
    if (entity.startsWith('#x')) return String.fromCodePoint(Number.parseInt(entity.slice(2), 16));
    if (entity.startsWith('#')) return String.fromCodePoint(Number(entity.slice(1)));
    return { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'" }[entity] ?? '';
  });
}

/** What the screen shows now (uiautomator's dump). */
export function nodes(): Node[] {
  adb('shell', 'uiautomator', 'dump', '/sdcard/ompdeck-ui.xml');
  const xml = adb('exec-out', 'cat', '/sdcard/ompdeck-ui.xml');
  const out: Node[] = [];
  for (const [element] of xml.matchAll(/<node [^>]*>/g)) {
    const attr = (name: string) => unescapeXml(new RegExp(` ${name}="([^"]*)"`).exec(element)?.[1] ?? '');
    const b = /\[(\d+),(\d+)\]\[(\d+),(\d+)\]/.exec(attr('bounds'));
    if (b === null) continue;
    out.push({
      text: attr('text'),
      desc: attr('content-desc'),
      cls: attr('class'),
      id: attr('resource-id'),
      pkg: attr('package'),
      enabled: attr('enabled') === 'true',
      checked: attr('checked') === 'true',
      bounds: [Number(b[1]), Number(b[2]), Number(b[3]), Number(b[4])],
    });
  }
  return out;
}

/** A node whose text or description is `name` (a string matches exactly). */
export function named(name: string | RegExp): (node: Node) => boolean {
  return (node) => [node.text, node.desc].some((value) => (typeof name === 'string' ? value === name : name.test(value)));
}

/** Waits until a node matching `match` is on screen. */
export async function waitNode(what: string, match: (node: Node) => boolean, timeout = 15_000): Promise<Node> {
  const end = Date.now() + timeout;
  for (;;) {
    const found = nodes().find(match);
    if (found !== undefined) return found;
    if (Date.now() > end) throw new Error(`the phone never showed ${what}`);
    await Bun.sleep(300);
  }
}

export function tap(node: Node): void {
  const [l, t, r, b] = node.bounds;
  adb('shell', 'input', 'tap', String(Math.round((l + r) / 2)), String(Math.round((t + b) / 2)));
}

/** Waits for the node, then taps it. */
export async function tapNode(what: string, match: (node: Node) => boolean, timeout?: number): Promise<Node> {
  const node = await waitNode(what, match, timeout);
  tap(node);
  return node;
}

/**
 * Taps the node once it is clear of the floating bottom bar, scrolling the page up first when it
 * sits behind it (a tap there would hit the bar's own buttons).
 */
export async function tapNodeAboveBar(what: string, match: (node: Node) => boolean, timeout?: number): Promise<Node> {
  for (let attempt = 0; ; attempt++) {
    const all = nodes();
    const node = all.find(match) ?? (await waitNode(what, match, timeout));
    const screen = Math.max(...all.map((n) => n.bounds[3]), node.bounds[3]);
    const width = Math.max(...all.map((n) => n.bounds[2]), node.bounds[2]);
    const centre = (node.bounds[1] + node.bounds[3]) / 2;
    if (centre < screen * 0.82 || attempt === 4) {
      tap(node);
      return node;
    }
    const x = String(Math.round(width / 2));
    adb('shell', 'input', 'swipe', x, String(Math.round(screen * 0.7)), x, String(Math.round(screen * 0.35)), '300');
    await Bun.sleep(400);
  }
}

/** Types through the device's input stack (key events into the focused field). */
export function typeText(text: string): void {
  if (!/^[A-Za-z0-9 .,:_-]+$/.test(text)) throw new Error(`typeText takes letters, digits, spaces and .,:_- only: ${text}`);
  // In pieces: one long argument makes `input text` drop or reorder keys on some phones.
  for (let at = 0; at < text.length; at += 48) adb('shell', 'input', 'text', text.slice(at, at + 48).replaceAll(' ', '%s'));
}

export function key(code: string): void {
  adb('shell', 'input', 'keyevent', code);
}

export function keyboardShown(): boolean {
  return /mInputShown=true/.test(adb('shell', 'dumpsys', 'input_method'));
}

export interface Shown {
  title: string;
  text: string;
}

/** Chrome's notifications for the web origin `origin` in the system's list. */
export function notifications(origin: string): Shown[] {
  const dump = adb('shell', 'dumpsys', 'notification', '--noredact');
  const out: Shown[] = [];
  for (const block of dump.split('NotificationRecord(').slice(1)) {
    const head = block.slice(0, block.indexOf('\n'));
    if (!head.includes(`pkg=${CHROME}`) || !head.includes(`channel=web:${origin}`)) continue;
    out.push({
      title: /android\.title=String \((.*)\)/.exec(block)?.[1] ?? '',
      text: /android\.text=String \((.*)\)/.exec(block)?.[1] ?? '',
    });
  }
  return out;
}

/** Opens `url` in Chrome the way a scanned QR code or a tapped link does. */
export function openUrl(url: string): void {
  if (/['\s]/.test(url)) throw new Error(`unexpected characters in ${url}`);
  // adb shell hands one string to the device's sh: quote it (the pairing link has a #fragment).
  adb('shell', `am start -a android.intent.action.VIEW -d '${url}' ${CHROME}`);
}

export function startChrome(): void {
  adb('shell', 'am', 'start', '-n', `${CHROME}/com.google.android.apps.chrome.Main`);
}

/** Forwards Chrome's DevTools socket to a free local port; returns it. */
export function forwardDevtools(): number {
  return Number(adb('forward', 'tcp:0', 'localabstract:chrome_devtools_remote').trim());
}
