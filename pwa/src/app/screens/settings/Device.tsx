// This device: its name (renamable, validated as at pairing), its permissions (read-only, changed
// on the desktop), the gateway it talks to, and unpairing (the desktop revokes it, then this
// phone's keys are deleted). Everything here is about the active desktop; this phone's pairings
// with other desktops are untouched, and one of them becomes active afterwards.

import { type ReactNode, useEffect, useState } from 'react';
import { useClient, useClientState, useFeed, useOnline } from '../../ui/hooks.ts';
import { Card, ConfirmSheet, KeyValues, NeedsS4 } from '../../ui/kit.tsx';
import { SectionScreen, ask } from './common.tsx';
import { MAX_NAME_CHARS, MODE_LABEL, deviceNameProblem } from './logic.ts';

export function DeviceSection(): ReactNode {
  const client = useClient();
  const online = useOnline();
  const pairing = useClientState((state) => state.pairing);
  const welcome = useClientState((state) => state.welcome);
  const device = useFeed({ feed: 'device' }, 'device');
  const [unpair, setUnpair] = useState(false);
  const [forget, setForget] = useState(false);
  const [busy, setBusy] = useState(false);
  const permissions = device?.permissions ?? welcome?.permissions ?? null;
  const desktop = pairing?.gateway ?? '—';

  const doUnpair = async () => {
    setBusy(true);
    await client.dropPush();
    const reply = await ask(client, { op: 'unpair' }, 'Unpair');
    setBusy(false);
    setUnpair(false);
    if (reply === null) return;
    await client.forget().catch((error: unknown) => client.notify(`Could not delete this phone’s keys: ${String(error)}`, 'error'));
  };

  return (
    <SectionScreen eyebrow="Settings" title="This device">
      <NeedsS4 what="Device settings">
        <NameCard current={device?.name ?? null} />
      </NeedsS4>
      <Card title="Permissions" aside={<span className="chip">read-only</span>}>
        <div className="perm-grid">
          <Permission name="View" on detail="Sessions, transcripts, usage, backups" />
          <Permission name="Control" on={permissions?.control ?? false} detail="Prompts, terminal input, approvals, backups" />
          <Permission name="Manage" on={permissions?.manage ?? false} detail="Settings, updates, restores, deletes" />
        </div>
        <p className="muted small note">Permissions are changed on the desktop (Settings → Mobile access), never from a phone.</p>
      </Card>
      <Card title="Gateway">
        <KeyValues
          rows={[
            ['Desktop', desktop],
            ['Version', <span className="chip accent">{device?.gateway_version ?? welcome?.gateway_version ?? '—'}</span>],
            ['Mode', device === undefined ? '—' : <span className="chip">{MODE_LABEL[device.mode]}</span>],
            welcome !== null ? ['Protocol', <span className="chip">v{welcome.protocol}</span>] : null,
            welcome !== null ? ['Features', welcome.features.length === 0 ? '—' : welcome.features.map((f) => <span key={f} className="chip ok">{f}</span>)] : null,
            ['Device id', <span className="mono">{welcome?.device ?? pairing?.device ?? '—'}</span>],
            ['Fingerprint', <span className="mono">{pairing?.fingerprint ?? '—'}</span>],
            ['Paired', pairing === null ? '—' : new Date(pairing.pairedUnixMs).toLocaleString()],
          ]}
        />
      </Card>
      <NeedsS4 what="Unpairing from the phone">
        <button type="button" className="button danger block" disabled={!online || busy} onClick={() => setUnpair(true)}>
          Unpair this phone
        </button>
      </NeedsS4>
      <button type="button" className="button ghost block forget" onClick={() => setForget(true)}>
        Forget on this phone only
      </button>
      {unpair && (
        <ConfirmSheet
          title="Unpair this phone?"
          message="The desktop revokes this phone, deletes its push subscription and attachments, and this phone deletes its keys for this desktop only. Connecting to it again needs a new pairing link. Other desktops stay paired."
          facts={[
            ['Desktop', desktop],
            ['This phone', device?.name ?? pairing?.device ?? '—'],
            ['Push subscription', device?.push.subscribed === true ? 'removed' : 'none'],
          ]}
          confirm="Unpair"
          busy={busy}
          onConfirm={() => void doUnpair()}
          onCancel={() => setUnpair(false)}
        />
      )}
      {forget && (
        <ConfirmSheet
          title="Forget this desktop?"
          message="Only this phone’s keys for this desktop are deleted; the desktop still lists the device until you revoke it there. Use this when the desktop is unreachable. Other desktops stay paired."
          facts={[
            ['Desktop', desktop],
            ['Device', pairing?.device ?? '—'],
          ]}
          confirm="Forget"
          onConfirm={() => {
            setForget(false);
            client.forget().catch((error: unknown) => client.notify(`Could not forget: ${String(error)}`, 'error'));
          }}
          onCancel={() => setForget(false)}
        />
      )}
    </SectionScreen>
  );
}

function Permission({ name, on, detail }: { name: string; on: boolean; detail: string }): ReactNode {
  return (
    <div className={`perm${on ? ' on' : ''}`}>
      <span className={`chip ${on ? 'ok' : 'danger'}`}>{on ? '✓' : '✕'} {name}</span>
      <small>{detail}</small>
    </div>
  );
}

function NameCard({ current }: { current: string | null }): ReactNode {
  const client = useClient();
  const online = useOnline();
  const [name, setName] = useState(current ?? '');
  const [busy, setBusy] = useState(false);
  // Follow the desktop's name until the user starts typing.
  const [dirty, setDirty] = useState(false);
  useEffect(() => {
    if (!dirty && current !== null) setName(current);
  }, [current, dirty]);
  const problem = deviceNameProblem(name);
  const changed = current !== null && name.trim() !== current;

  const save = async () => {
    setBusy(true);
    const reply = await ask(client, { op: 'device_rename', name: name.trim() }, 'Rename');
    setBusy(false);
    if (reply !== null) {
      setDirty(false);
      client.notify('Renamed. The desktop shows the new name.');
    }
  };

  return (
    <Card title="Name">
      <label className="field">
        <span>Shown on the desktop</span>
        <input
          className={`input${dirty && problem !== null ? ' invalid' : ''}`}
          value={name}
          maxLength={MAX_NAME_CHARS * 2}
          disabled={!online || busy}
          onChange={(event) => {
            setDirty(true);
            setName(event.target.value);
          }}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && problem === null && changed) void save();
          }}
        />
      </label>
      {dirty && problem !== null && <p className="field-error">{problem}</p>}
      <div className="buttons end">
        {dirty && (
          <button
            type="button"
            className="button ghost"
            onClick={() => {
              setDirty(false);
              setName(current ?? '');
            }}
          >
            Undo
          </button>
        )}
        <button type="button" className="button primary" disabled={!online || busy || problem !== null || !changed} onClick={() => void save()}>
          {busy ? 'Saving…' : 'Rename'}
        </button>
      </div>
    </Card>
  );
}
