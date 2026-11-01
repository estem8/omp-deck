// The smoke harness server: one Bun HTTP + WebSocket server on 127.0.0.1 that is
// - the static host of a fresh PWA build (built into a temp dir, never into dist/), whose
//   index.html is rewritten to boot through `boot.ts` (pairing seeds, theme, gateway variant);
// - the relay (`/v1/device`: the device hello, answered `HELLO_OK`, routed to a desktop by its room);
// - the gateways behind it, two desktops ("Workstation" and "Laptop", each with its own key, room and
//   tokens): the real Noise responder (`respond`), the real record codec, and the fixture-backed
//   application side (`gateway.ts`).
// Nothing here is in the production bundle: build.test.ts asserts it.

import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { ServerWebSocket } from 'bun';
import { build } from '../../build.ts';
import {
  CLOSE,
  DEVICE_PATH,
  decodeRecords,
  encodePairingInvite,
  encodeRecords,
  HELLO_OK,
  Keypair,
  MAX_APP_MESSAGE,
  type Pending,
  respond,
  type Session,
  toBase64Url,
} from '../../src/remote/index.ts';
import { type SmokeConnection, SmokeGateway } from './gateway.ts';

const PWA = join(import.meta.dir, '..', '..');

/** Fixed bytes from a label (deterministic keys and tokens). */
function fixed(label: string, length: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(length);
  const digest = new Bun.CryptoHasher('sha256').update(label).digest();
  for (let i = 0; i < length; i++) out[i] = digest[i % digest.length]! ^ (i >> 5);
  return out;
}

/** One fake desktop: its own gateway key, room, tokens and application state. */
export interface SmokeDesktop {
  /** The gateway's label (the desktop's name). */
  label: string;
  /** The pairing id the phone derives: the lowercase hex of the gateway's static key. */
  id: string;
  /** A `v1.…` pairing invite this desktop auto-approves. */
  invite: string;
  gateway: SmokeGateway;
}

export interface SmokeServer {
  /** `http://127.0.0.1:<port>` */
  origin: string;
  /** The relay URL in invites and the seeded pairings (`ws://127.0.0.1:<port>`). */
  relay: string;
  /** A `v1.…` pairing invite the first desktop (Workstation) auto-approves. */
  invite: string;
  gateway: SmokeGateway;
  /** The first desktop ("Workstation": the seeded pairing, `invite`, `gateway`). */
  first: SmokeDesktop;
  /** A second desktop ("Laptop") on the same relay, with its own key, room, tokens and gateway. */
  second: SmokeDesktop;
  /** `origin/#route=…&theme=…` */
  url(hash: string): string;
  stop(): Promise<void>;
}

export interface SmokeOptions {
  port?: number;
  /** Build minified (like the release). Default false: readable stack traces. */
  minify?: boolean;
  log?: (line: string) => void;
}

/** The desktop behind a room: the secrets the relay and its gateway use. */
interface Desktop {
  label: string;
  gatewayKey: Keypair;
  deviceKey: Keypair;
  room: Uint8Array<ArrayBuffer>;
  pairingToken: Uint8Array<ArrayBuffer>;
  deviceToken: Uint8Array<ArrayBuffer>;
  psk: Uint8Array<ArrayBuffer>;
  seededDevice: string;
  /** Device public key (hex) → registry id. */
  devices: Map<string, string>;
  gateways: { s4: SmokeGateway; v1: SmokeGateway };
  invite: string;
}

interface SocketData {
  stage: 'hello' | 'handshake' | 'session';
  desktop: Desktop | null;
  pairing: boolean;
  pending: Pending | null;
  session: Session | null;
  conn: SmokeConnection | null;
  /** Cookie `smoke_gw=v1`: this connection's gateway announces no `s4`. */
  v1: boolean;
}

export async function startSmoke(options: SmokeOptions = {}): Promise<SmokeServer> {
  const log = options.log ?? (() => undefined);
  const outdir = mkdtempSync(join(tmpdir(), 'ompdeck-smoke-'));
  await build({ outdir, minify: options.minify ?? false });
  const boot = await Bun.build({ entrypoints: [join(import.meta.dir, 'boot.ts')], target: 'browser', minify: false });
  if (!boot.success || boot.outputs[0] === undefined) throw new Error(`boot.ts failed to build: ${boot.logs.join('\n')}`);
  const bootJs = await boot.outputs[0].text();

  const html = readFileSync(join(outdir, 'index.html'), 'utf8');
  const script = /<script type="module"[^>]*src="([^"]+)"[^>]*><\/script>/.exec(html);
  if (script?.[1] === undefined) throw new Error('index.html has no module script');
  const main = script[1].startsWith('/') ? script[1] : `/${script[1].replace(/^\.\//, '')}`;
  const indexHtml = html.replace(script[0], '<script type="module" src="/__smoke/boot.js"></script>');

  const vapid = (await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign'])) as CryptoKeyPair;
  const pushKey = toBase64Url(new Uint8Array(await crypto.subtle.exportKey('raw', vapid.publicKey)));
  const png = new Uint8Array(readFileSync(join(PWA, 'public', 'icon-512.png')));
  /** `tag` keeps the first desktop's fixed bytes as they always were; the second gets its own. */
  const desktop = (tag: string, label: string, seededDevice: string, version: string | null): Desktop => {
    const at = (name: string) => `ompdeck-smoke-${name}${tag}`;
    const deviceKey = Keypair.fromSecret(fixed(at('device'), 32));
    return {
      label,
      gatewayKey: Keypair.fromSecret(fixed(at('gateway'), 32)),
      deviceKey,
      room: fixed(at('room'), 16),
      pairingToken: fixed(at('pairing-token'), 32),
      deviceToken: fixed(at('device-token'), 32),
      psk: fixed(at('psk'), 32),
      seededDevice,
      devices: new Map([[Buffer.from(deviceKey.public).toString('hex'), seededDevice]]),
      gateways: { s4: new SmokeGateway({ s4: true, png, pushKey, version }), v1: new SmokeGateway({ s4: false, png, pushKey: null }) },
      invite: '',
    };
  };
  const desktops = [desktop('', 'Workstation', 'd-0123456789abcdef', null), desktop('-laptop', 'Laptop', 'd-fedcba9876543210', '0.11.0')] as const;
  const [workstation, laptop] = desktops;

  const server = Bun.serve<SocketData, never>({
    hostname: '127.0.0.1',
    port: options.port ?? 0,
    fetch(request, srv): Response | undefined {
      const url = new URL(request.url);
      if (url.pathname === DEVICE_PATH) {
        const v1 = /(?:^|;\s*)smoke_gw=v1(?:;|$)/.test(request.headers.get('cookie') ?? '');
        const data: SocketData = { stage: 'hello', desktop: null, pairing: false, pending: null, session: null, conn: null, v1 };
        return srv.upgrade(request, { data }) ? undefined : new Response('upgrade failed', { status: 400 });
      }
      if (url.pathname === '/__smoke/boot.js') return new Response(bootJs, { headers: { 'content-type': 'text/javascript' } });
      if (url.pathname === '/__smoke/seed.json') {
        const seed = (d: Desktop) => ({
          secret: toBase64Url(d.deviceKey.secretBytes()),
          gatewayPublic: toBase64Url(d.gatewayKey.public),
          url: relay,
          room: toBase64Url(d.room),
          token: toBase64Url(d.deviceToken),
          device: d.seededDevice,
          gateway: d.label,
          name: 'Pixel 9',
        });
        return Response.json({ main, pairing: seed(workstation), second: seed(laptop) });
      }
      if (url.pathname === '/' || url.pathname === '/index.html') {
        return new Response(indexHtml, { headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' } });
      }
      const path = join(outdir, decodeURIComponent(url.pathname));
      if (!path.startsWith(outdir)) return new Response('forbidden', { status: 403 });
      const file = Bun.file(path);
      return file.size > 0 || url.pathname.endsWith('.json') ? new Response(file, { headers: { 'cache-control': 'no-store' } }) : new Response('not found', { status: 404 });
    },
    websocket: {
      maxPayloadLength: 70_000,
      message(ws: ServerWebSocket<SocketData>, message) {
        if (typeof message === 'string') {
          ws.close(CLOSE.PROTOCOL, 'text message');
          return;
        }
        const bytes = new Uint8Array(message);
        const data = ws.data;
        try {
          if (data.stage === 'hello') {
            const found = bytes.length === 50 && bytes[0] === 0x44 && bytes[1] === 1 ? desktops.find((d) => equal(bytes.subarray(2, 18), d.room)) : undefined;
            if (found === undefined) {
              ws.close(CLOSE.UNAUTHORIZED, 'bad hello');
              return;
            }
            data.desktop = found;
            data.pairing = equal(bytes.subarray(18, 50), found.pairingToken);
            data.stage = 'handshake';
            ws.send(HELLO_OK);
            return;
          }
          if (data.stage === 'handshake' && data.desktop !== null) {
            const d = data.desktop;
            const pending = respond(data.pairing ? { kind: 'pairing', psk: d.psk } : { kind: 'reconnect' }, d.gatewayKey, bytes);
            const key = Buffer.from(pending.devicePublic).toString('hex');
            if (data.pairing) {
              // The desktop "approves" after a moment, so the approval stage is visible.
              const device = `d-${key.slice(0, 16)}`;
              d.devices.set(key, device);
              log(`pairing ${pending.hello.name} → ${device} at ${d.label}`);
              setTimeout(() => {
                const { msg2 } = pending.accept({ device, gateway: d.label, token: d.deviceToken });
                ws.send(msg2);
              }, 900);
              data.stage = 'session';
              return;
            }
            const device = d.devices.get(key) ?? `d-${key.slice(0, 16)}`;
            const { session, msg2 } = pending.accept({ device, gateway: d.label });
            session.config.maxMessage = MAX_APP_MESSAGE;
            data.session = session;
            data.stage = 'session';
            ws.send(msg2);
            const gateway = data.v1 ? d.gateways.v1 : d.gateways.s4;
            log(`device ${device} connected to ${d.label} (${data.v1 ? 'v1' : 's4'} gateway, features ${pending.hello.features.join(',') || 'none'})`);
            data.conn = gateway.connect(
              (records) => {
                if (ws.readyState !== 1 || data.session === null) return;
                for (const frame of data.session.seal(encodeRecords(records))) ws.send(frame);
              },
              device,
              pending.hello.name,
            );
            return;
          }
          if (data.session === null || data.conn === null) return;
          const plain = data.session.open(bytes);
          if (plain === null) return;
          data.conn.receive(decodeRecords(plain, 'device'));
        } catch (error) {
          log(`closing a device: ${error instanceof Error ? error.message : String(error)}`);
          ws.close(CLOSE.PROTOCOL, 'protocol error');
        }
      },
      close(ws) {
        ws.data.conn?.close();
      },
    },
  });

  const port: number = server.port ?? 0;
  const origin = `http://127.0.0.1:${port}`;
  const relay: string = `ws://127.0.0.1:${port}`;
  for (const d of desktops) d.invite = encodePairingInvite({ url: relay, room: d.room, token: d.pairingToken, gatewayPublic: d.gatewayKey.public, psk: d.psk });
  const view = (d: Desktop): SmokeDesktop => ({ label: d.label, id: Buffer.from(d.gatewayKey.public).toString('hex'), invite: d.invite, gateway: d.gateways.s4 });
  return {
    origin,
    relay,
    invite: workstation.invite,
    gateway: workstation.gateways.s4,
    first: view(workstation),
    second: view(laptop),
    url: (hash) => `${origin}/${hash.length === 0 || hash.startsWith('#') ? hash : `#${hash}`}`,
    async stop() {
      await server.stop(true);
      rmSync(outdir, { recursive: true, force: true });
    },
  };
}

function equal(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}
