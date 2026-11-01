// The live stack's lifecycle, shared by `bun run live` and `bun run phone`: builds dist/ and the
// launcher (crates/deck-service/tests/pwa_live_stack.rs), starts it under a private session bus,
// waits for its URL file and stops it through its stop file.

import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Stack } from './spec.ts';

const PWA = join(import.meta.dir, '../..');
const REPO = join(PWA, '..');
const CARGO_TEST = ['cargo', 'test', '-p', 'deck-service', '--test', 'pwa_live_stack'];
/** The launcher's pairing window and omp's first start, once the test binary is built. */
const STACK_UP = 120_000;

export interface Running {
  info: Stack;
  /** Stops the stack; `false` when it had to be killed or exited non-zero. */
  stop(): Promise<boolean>;
  /** The tail of the stack's output. */
  log(): string;
}

/** Exits with 0 when omp is missing (the stack needs a real omp on PATH). */
export function requireOmp(gate: string): void {
  if (Bun.which('omp') === null) {
    console.log(`${gate}: skipped — omp is not installed (the stack needs a real omp on PATH)`);
    process.exit(0);
  }
}

/** Runs `cmd` in the repo or the app dir; exits 1 when it fails. */
export function step(gate: string, cmd: string[], where: 'pwa' | 'repo'): void {
  console.log(`${gate}: ${cmd.join(' ')}`);
  const result = Bun.spawnSync(cmd, { cwd: where === 'pwa' ? PWA : REPO, stdout: 'inherit', stderr: 'inherit' });
  if (result.exitCode !== 0) {
    console.error(`${gate}: \`${cmd.join(' ')}\` failed (${result.exitCode})`);
    process.exit(1);
  }
}

/** Builds the app and the launcher. */
export function build(gate: string): void {
  step(gate, ['bun', 'run', 'build'], 'pwa');
  step(gate, [...CARGO_TEST, '--no-run'], 'repo');
}

/** Starts the stack (`env` adds launcher variables) and waits until it is up. */
export function startStack(env: Record<string, string> = {}): Promise<Running> {
  return launch(env);
}

/**
 * A first stack for a run with two desktops: the same stack, plus a second relay room whose key is
 * published as `info.second_room_key` (`startSecondDesktop` hands it to the second gateway).
 */
export function startFirstOfTwo(env: Record<string, string> = {}): Promise<Running> {
  return launch({ ...env, OMPDECK_LIVE_SECOND_ROOM: '1' });
}

/**
 * The second desktop of a two-desktop run: another gateway in its own sandbox, namespace and room, with
 * its own real omp on its own scripted model, linked to `first`'s relay (it serves no relay of its own).
 * Its `info` has its own pairing link and control endpoint; `origin` and `relay` are the first's.
 */
export async function startSecondDesktop(first: Stack, env: Record<string, string> = {}): Promise<Running> {
  if (first.second_room_key === null || first.second_room_key === undefined) throw new Error('the first stack has no second room (start it with startFirstOfTwo)');
  return launch({ ...env, OMPDECK_LIVE_JOIN_RELAY: first.relay, OMPDECK_LIVE_JOIN_KEY: first.second_room_key });
}

async function launch(env: Record<string, string>): Promise<Running> {
  const dir = mkdtempSync(join(tmpdir(), 'pwa-live-'));
  const urlFile = join(dir, 'stack.json');
  const stopFile = join(dir, 'stop');
  const logFile = join(dir, 'stack.log');
  // A private session bus when available: the gateway's desktop notifications never reach the
  // user's desktop.
  const dbus = Bun.which('dbus-run-session');
  const stack = Bun.spawn([...(dbus === null ? [] : [dbus, '--']), ...CARGO_TEST, '--', '--ignored', '--exact', '--nocapture', 'pwa_live_stack'], {
    cwd: REPO,
    env: { ...process.env, ...env, OMPDECK_LIVE_URL_FILE: urlFile, OMPDECK_LIVE_STOP_FILE: stopFile },
    stdout: Bun.file(logFile),
    stderr: Bun.file(`${logFile}.err`),
  });
  let exited = false;
  void stack.exited.then(() => {
    exited = true;
  });
  const log = (): string => {
    const read = (path: string) => (existsSync(path) ? readFileSync(path, 'utf8') : '');
    return `${read(logFile)}${read(`${logFile}.err`)}`.split('\n').slice(-120).join('\n');
  };
  const stop = async (): Promise<boolean> => {
    writeFileSync(stopFile, '');
    const stopped = await Promise.race([stack.exited.then(() => true), Bun.sleep(90_000).then(() => false)]);
    let ok = true;
    if (!stopped) {
      stack.kill('SIGKILL');
      ok = false;
      console.error('the stack did not stop in time');
    } else if (stack.exitCode !== 0) {
      ok = false;
      console.error(`the stack exited ${stack.exitCode}`);
    }
    if (!ok) console.error(`---- stack log (tail) ----\n${log()}\n---- end ----`);
    rmSync(dir, { recursive: true, force: true });
    return ok;
  };
  const end = Date.now() + STACK_UP;
  while (!existsSync(urlFile)) {
    if (exited || Date.now() > end) {
      const why = exited ? `the stack exited (${stack.exitCode})` : 'the stack did not come up in time';
      await stop();
      throw new Error(why);
    }
    await Bun.sleep(200);
  }
  return { info: JSON.parse(readFileSync(urlFile, 'utf8')) as Stack, stop, log };
}
