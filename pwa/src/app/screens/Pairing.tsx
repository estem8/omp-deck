// Pairing: name this phone, compare fingerprints with the desktop's approval dialog, and wait
// for approval. Reached by opening the desktop's pairing link (`#pair=`), pasting it, or "Pair
// another desktop" in the desktop switcher. A new desktop is added next to the paired ones; an
// invite for a desktop that is already paired replaces only that pairing.

import { type ReactNode, useMemo, useState } from 'react';
import { inspectInvite } from '../desktops.ts';
import { inviteFromText } from '../nav.ts';
import { useClientState, useDesktops, useDesktopsState } from '../ui/hooks.ts';
import { Card, InstallHint, KeyValues } from '../ui/kit.tsx';

function defaultName(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return 'iPhone';
  if (/iPad/.test(ua)) return 'iPad';
  if (/Android/.test(ua)) return 'Android phone';
  return 'Phone';
}

export function PairingScreen({
  invite,
  onInvite,
  standalone,
}: {
  /** The invite from the opened link, if any. */
  invite: string | null;
  onInvite: (invite: string | null) => void;
  standalone: boolean;
}): ReactNode {
  const attempt = useDesktopsState((state) => state.attempt);
  // A pairing in flight runs on its own client: the progress view reads that one (the context).
  if (attempt !== null) return <PairingProgress invite={attempt.invite} onInvite={onInvite} />;
  return <PairingForm invite={invite} onInvite={onInvite} standalone={standalone} />;
}

function PairingProgress({ invite, onInvite }: { invite: string; onInvite: (invite: string | null) => void }): ReactNode {
  const desktops = useDesktops();
  const flow = useClientState((state) => state.pairingFlow);
  if (flow === null) return null;
  const waiting = flow.stage !== 'failed';
  return (
    <div className="screen pushed">
      <header className="top">
        <div className="eyebrow">Pairing</div>
        <h2>{flow.stage === 'approval' ? 'Approve on your desktop' : flow.stage === 'failed' ? 'Pairing failed' : 'Connecting…'}</h2>
      </header>
      <div className="body">
        <div className="pair-hero">
          <div className="mark">{waiting ? <span className="busy" style={{ width: 28, height: 28, borderWidth: 3, borderColor: 'currentColor', borderRightColor: 'transparent' }} /> : '!'}</div>
          <p className="muted" style={{ margin: 0 }}>
            {flow.stage === 'approval'
              ? 'OmpDeck on your desktop asks whether to allow this phone. Check that it shows this fingerprint:'
              : flow.stage === 'failed'
                ? (flow.error ?? 'The desktop did not accept this phone.')
                : 'Reaching your desktop through the relay…'}
          </p>
        </div>
        <div className="fingerprint" aria-label="This phone’s fingerprint">
          {flow.fingerprint}
        </div>
        <Card title="Details">
          <KeyValues
            rows={[
              ['Phone name', flow.name],
              ['Desktop key', <span className="mono">{flow.gatewayFingerprint}</span>],
              ['Relay', <span className="mono">{flow.relay}</span>],
            ]}
          />
        </Card>
        {flow.stage === 'failed' && (
          <div className="buttons pair" style={{ display: 'grid', gridTemplateColumns: '1fr 1fr' }}>
            <button
              type="button"
              className="button"
              onClick={() => {
                desktops.dismissPairing();
                onInvite(invite);
              }}
            >
              Back
            </button>
            <button type="button" className="button primary" onClick={() => void desktops.pair(invite, flow.name)}>
              Try again
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

function PairingForm({ invite, onInvite, standalone }: { invite: string | null; onInvite: (invite: string | null) => void; standalone: boolean }): ReactNode {
  const desktops = useDesktops();
  const paired = useDesktopsState((state) => state.desktops);
  const [name, setName] = useState(defaultName);
  const [pasted, setPasted] = useState('');
  const active = invite ?? inviteFromText(pasted);
  const info = useMemo(() => (active === null ? null : inspectInvite(active)), [active]);
  // The invite names a desktop that is already paired: pairing replaces that pairing only.
  const known = info?.ok === true ? paired.find((desktop) => desktop.id === info.id) : undefined;
  const others = known === undefined ? paired : paired.filter((desktop) => desktop.id !== known.id);
  const invalid = info !== null && !info.ok;
  const title = paired.length === 0 ? 'Pair with your desktop' : known !== undefined ? `Pair again with ${known.gateway}` : 'Pair another desktop';
  const action = paired.length === 0 ? 'Pair this phone' : known !== undefined ? 'Replace pairing' : 'Pair this desktop';
  return (
    <div className="screen pushed">
      <header className="top">
        <div className="eyebrow">OmpDeck</div>
        <h2>{title}</h2>
      </header>
      <div className="body">
        <div className="pair-hero">
          <div className="mark">◆</div>
          <p className="muted" style={{ margin: 0 }}>
            {active !== null
              ? 'Name this phone, then approve it on your desktop.'
              : 'On your desktop, open OmpDeck → Settings → Mobile access → Pair a phone, then open the link on this phone or paste it here.'}
          </p>
        </div>
        <InstallHint standalone={standalone} />
        {known !== undefined && (
          <div className="banner warn">
            <span>⚠</span>
            <span className="grow">
              This phone is already paired with {known.gateway} as {known.name}. Pairing again replaces this phone’s pairing with {known.gateway}.
              {others.length > 0 && ` ${others.map((desktop) => desktop.gateway).join(', ')} stay${others.length === 1 ? 's' : ''} paired.`}
            </span>
          </div>
        )}
        {known === undefined && !invalid && paired.length > 0 && (
          <div className="banner" role="note">
            <span>◆</span>
            <span className="grow">
              {paired.map((desktop) => desktop.gateway).join(', ')} {paired.length === 1 ? 'stays' : 'stay'} paired. This adds another desktop; switch between them from the desktop name at the top of the app.
            </span>
          </div>
        )}
        {info?.ok === false && (
          <div className="banner danger" role="alert">
            <span>⚠</span>
            <span className="grow">This pairing link is not valid: {info.error}</span>
          </div>
        )}
        {/* Stays while typing: a half-typed link is not an invite yet, and a field that vanishes at the first valid prefix loses focus. */}
        {invite === null && (
          <label className="field">
            <span>Pairing link</span>
            <input
              className="input"
              value={pasted}
              onChange={(event) => setPasted(event.target.value)}
              placeholder="https://…/#pair=v1.…"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
            />
          </label>
        )}
        <label className="field">
          <span>Phone name</span>
          <input className="input" value={name} maxLength={64} onChange={(event) => setName(event.target.value)} />
        </label>
        <div className="buttons pair" style={{ display: 'grid', gridTemplateColumns: paired.length > 0 || invite !== null ? '1fr 1fr' : '1fr' }}>
          {(paired.length > 0 || invite !== null) && (
            <button
              type="button"
              className="button"
              onClick={() => {
                setPasted('');
                onInvite(null);
                desktops.cancelAdd();
              }}
            >
              Cancel
            </button>
          )}
          <button
            type="button"
            className="button primary block"
            disabled={active === null || invalid || name.trim().length === 0}
            onClick={() => {
              if (active === null) return;
              // The pairing in flight (the desktops' `attempt`) carries the invite from here: Back after a failure restores it.
              onInvite(null);
              void desktops.pair(active, name.trim());
            }}
          >
            {action}
          </button>
        </div>
      </div>
    </div>
  );
}
