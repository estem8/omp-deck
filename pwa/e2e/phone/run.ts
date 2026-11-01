// `bun run phone`: the phone app's real-device gate on an Android phone connected over adb (USB
// debugging on, unlocked, Developer options → Stay awake; `ANDROID_SERIAL` picks one of several).
// Builds dist/, starts the live stack on a fixed relay port, `adb reverse`s it so the phone's
// Chrome loads the app from http://127.0.0.1:<port>, runs ./spec.ts, then stops the stack and
// removes the port mappings, the origin's site data and the picked file from the phone.
// Exit code 1 on any failure; skipped (exit 0) when omp is not installed.

import { build, requireOmp, type Running, startFirstOfTwo, startSecondDesktop } from '../live/stack.ts';
import * as phone from './adb.ts';
import { runPhone } from './spec.ts';

/** One fixed origin on the phone across runs (Chrome keeps site data and permissions per origin). */
const PORT = process.env['PHONE_RELAY_PORT'] ?? '47319';

requireOmp('phone');
let device: string;
try {
  device = phone.preflight();
} catch (error) {
  console.error(`phone: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}
console.log(`phone: ${device}`);
build('phone');

let failed = false;
let stack: Running | null = null;
let second: Running | null = null;
let devtools: number | null = null;
try {
  // Two real desktops on one relay: the phone pairs with both (the second joins the first's relay in its own room).
  stack = await startFirstOfTwo({ OMPDECK_LIVE_RELAY_PORT: PORT });
  second = await startSecondDesktop(stack.info);
  phone.adb('reverse', `tcp:${PORT}`, `tcp:${PORT}`);
  phone.startChrome();
  devtools = phone.forwardDevtools();
  for (const end = Date.now() + 30_000; ; await Bun.sleep(300)) {
    const ok = await fetch(`http://127.0.0.1:${devtools}/json/version`).then(
      (r) => r.ok,
      () => false,
    );
    if (ok) break;
    if (Date.now() > end) throw new Error("Chrome's DevTools socket did not answer (is USB debugging on?)");
  }
  console.log(`phone: stack up — app ${stack.info.origin} (adb reverse), control ${stack.info.control}; second desktop control ${second.info.control}`);
  const started = performance.now();
  const run = await runPhone(stack.info, second.info, devtools);
  console.log(`phone: all steps passed in ${Math.round((performance.now() - started) / 1000)} s on ${device}`);
  console.log(`phone: screenshots\n${run.shots.map((path) => `  ${path}`).join('\n')}`);
} catch (error) {
  failed = true;
  console.error(`phone: FAILED — ${error instanceof Error ? (error.stack ?? error.message) : String(error)}`);
  if (stack !== null) console.error(`---- stack log (tail) ----\n${stack.log()}\n---- end ----`);
  if (second !== null) console.error(`---- second desktop log (tail) ----\n${second.log()}\n---- end ----`);
} finally {
  if (devtools !== null) Bun.spawnSync(['adb', 'forward', '--remove', `tcp:${devtools}`]);
  Bun.spawnSync(['adb', 'reverse', '--remove', `tcp:${PORT}`]);
  if (second !== null && !(await second.stop())) failed = true;
  if (stack !== null && !(await stack.stop())) failed = true;
}
process.exit(failed ? 1 : 0);
