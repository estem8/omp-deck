// Stands in for the version 1 app left open in another tab or window: a page of this origin that
// holds the version 1 database connection for its whole life, with no `versionchange` handler (the
// 0.22 keystore had none), so the new version's upgrade is blocked by it.
//
// A reload (the service worker's `navigate`) runs this again, and this time it lets go: that is what
// the new code does in the real app. `sessionStorage` carries the count across the reload, and the
// page shows it for the test to read.

import { type Seed, openLegacy, stored } from './legacy.ts';

const loads = Number(sessionStorage.getItem('holder-loads') ?? '0') + 1;
sessionStorage.setItem('holder-loads', String(loads));
document.body.dataset['loads'] = String(loads);

if (loads === 1) {
  const seed = (await (await fetch('/__smoke/seed.json')).json()) as Seed;
  // The profile's database is the new app's (it ran here first, to install the worker): start over as the old app left it.
  await new Promise<void>((resolve, reject) => {
    const drop = indexedDB.deleteDatabase('ompdeck');
    drop.onsuccess = () => resolve();
    drop.onerror = () => reject(drop.error);
  });
  const held = await openLegacy(stored(seed.pairing, Date.now()));
  Reflect.set(window, '__held', held);
}
document.body.dataset['holding'] = loads === 1 ? 'yes' : 'no';
