// The per-session actions sheet (Sessions rows, the session manager's detail sheet): open, open
// or close on the desktop (with split), session tree, resume/restart/stop/kill (this session
// only), rename, copy/move to another project, delete. Destructive steps confirm with what goes.

import { type ReactNode, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { useNav } from '../../nav.ts';
import { perform, shortModel, statusLabel, succeeded } from '../../ui/format.ts';
import { useCan, useClient, useFeed, useLiveFor } from '../../ui/hooks.ts';
import { ConfirmSheet, Sheet, useS4 } from '../../ui/kit.tsx';
import { doneResult } from './common.ts';
import { bytes, entryMessages, entrySize, plural } from './logic.ts';
import '../../../styles/fleet.css';

type Mode = 'menu' | 'desktop' | 'rename' | 'copy' | 'move' | 'delete' | 'kill' | 'stop' | 'restart';

const SPLITS: readonly (readonly [api.Split, string])[] = [
  ['none', 'Own tab'],
  ['right', 'Split right'],
  ['down', 'Split down'],
];

export function SessionActionsSheet({
  session,
  title,
  display,
  entry = null,
  onClose,
}: {
  session: string;
  title: string;
  /** The project folder, for confirmations. */
  display: string;
  /** Inventory figures, when known (sizes in the delete confirmation). */
  entry?: api.InventoryView | null;
  onClose: () => void;
}): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const s4 = useS4();
  const live = useLiveFor(session);
  const [mode, setMode] = useState<Mode>('menu');
  const [busy, setBusy] = useState(false);
  const [split, setSplit] = useState<api.Split>('none');
  const [name, setName] = useState(title);
  const projectsFeed = useFeed(mode === 'copy' || mode === 'move' ? { feed: 'projects' } : null, 'projects');

  const running = live !== null && live.status.state !== 'exited';
  const act = async (op: api.Op, label: string, success?: string): Promise<boolean> => {
    setBusy(true);
    const view = await perform(client, op, label, success);
    setBusy(false);
    const ok = succeeded(view);
    if (ok) onClose();
    else setMode('menu');
    return ok;
  };
  const open = () => {
    onClose();
    nav.push({ kind: 'conversation', session, liveKey: live?.live.key ?? null, title });
  };

  if (mode === 'delete') {
    return (
      <ConfirmSheet
        title="Delete this session?"
        message="Its history is permanently removed from the desktop. This cannot be undone."
        facts={[
          ['Session', title],
          ['Folder', display],
          ...(entry === null
            ? []
            : ([
                ['Size', bytes(entrySize(entry))],
                ['Messages', entryMessages(entry)],
                ...(entry.artifact_files > 0 ? [['Artifacts', `${plural(entry.artifact_files, 'file')} · ${bytes(entry.artifact_bytes)}`] as const] : []),
              ] as const)),
          ...(running ? [['Running', 'stopped first, then deleted'] as const] : []),
        ]}
        confirm="Delete"
        busy={busy}
        onConfirm={() => void act({ op: 'delete_sessions', sessions: [session] }, `Delete “${title}”`, 'Session deleted.')}
        onCancel={() => setMode('menu')}
      />
    );
  }
  if ((mode === 'kill' || mode === 'stop' || mode === 'restart') && live !== null) {
    const copy = {
      kill: ['Kill', 'The process is terminated at once; anything unsaved is lost. Other sessions are not touched.'],
      stop: ['Stop', 'omp is asked to exit; work in progress ends. Other sessions are not touched.'],
      restart: ['Restart', 'omp exits and starts again on the same session.'],
    }[mode];
    return (
      <ConfirmSheet
        title={`${copy[0]} “${title}”?`}
        message={copy[1]}
        facts={[
          ['Session', title],
          ['Folder', live.display],
          ['Status', statusLabel(live)],
          ['Phones watching', live.views],
        ]}
        confirm={copy[0]}
        busy={busy}
        onConfirm={() => void act({ op: mode, live: live.live }, `${copy[0]} “${title}”`)}
        onCancel={() => setMode('menu')}
      />
    );
  }
  if (mode === 'copy' || mode === 'move') {
    return (
      <ProjectPicker
        title={`${mode === 'copy' ? 'Copy' : 'Move'} “${title}” to…`}
        projects={projectsFeed?.projects ?? []}
        loading={projectsFeed === undefined}
        busy={busy}
        onPick={(project) =>
          void act(
            { op: mode === 'copy' ? 'copy_session' : 'move_session', session, project: project.id },
            `${mode === 'copy' ? 'Copy' : 'Move'} “${title}”`,
            `${mode === 'copy' ? 'Copied' : 'Moved'} to ${project.display}.`,
          )
        }
        onClose={() => setMode('menu')}
      />
    );
  }

  const renameOp: api.Op | null = live === null ? null : { op: 'action', live: live.live, action: { type: 'rename', name: name.trim() } };
  const deskOp: api.Op = { op: 'open_on_desktop', session, split };
  return (
    <Sheet title={title} icon="⋯" onClose={onClose}>
      <div className="badges sheet-badges">
        {live === null ? (
          <span className="chip">not running</span>
        ) : (
          <>
            <span className={`chip ${live.status.state === 'attention' ? 'warn' : running ? 'ok' : ''}`}>{statusLabel(live)}</span>
            <span className="chip">{shortModel(live.model)}</span>
            {live.desktop_controlled && <span className="chip">desktop control</span>}
          </>
        )}
        <span className="chip mono">{display}</span>
      </div>
      {mode === 'desktop' ? (
        <div className="sub-panel">
          <span className="eyebrow">Open on desktop as</span>
          <div className="segmented">
            {SPLITS.map(([id, label]) => (
              <button key={id} type="button" aria-pressed={split === id} onClick={() => setSplit(id)}>
                {label}
              </button>
            ))}
          </div>
          <div className="buttons pair">
            <button type="button" className="button" onClick={() => setMode('menu')}>
              Back
            </button>
            <button type="button" className="button primary" disabled={busy || !can(deskOp)} onClick={() => void act(deskOp, `Open “${title}” on desktop`, 'Opened on the desktop.')}>
              Open on desktop
            </button>
          </div>
        </div>
      ) : mode === 'rename' && renameOp !== null ? (
        <div className="sub-panel">
          <label className="field">
            <span>New name</span>
            <input className="input" value={name} maxLength={200} onChange={(event) => setName(event.target.value)} />
          </label>
          <div className="buttons pair">
            <button type="button" className="button" onClick={() => setMode('menu')}>
              Back
            </button>
            <button
              type="button"
              className="button primary"
              disabled={busy || name.trim().length === 0 || name.trim() === title || !can(renameOp)}
              onClick={() => void act(renameOp, `Rename “${title}”`, 'Renamed.')}
            >
              Rename
            </button>
          </div>
        </div>
      ) : (
        <div className="menu">
          <button type="button" className="button" onClick={open}>
            ▸ Open <span className="hint">{running ? 'conversation' : 'transcript'}</span>
          </button>
          {s4 && (
            <button type="button" className="button" onClick={() => setMode('desktop')}>
              ⧉ Open on desktop <span className="hint">tab or split</span>
            </button>
          )}
          {s4 && running && (
            <button
              type="button"
              className="button"
              disabled={busy || !can({ op: 'close_on_desktop', session })}
              onClick={() => void act({ op: 'close_on_desktop', session }, `Close “${title}” on desktop`, 'Closed on the desktop.')}
            >
              ⊟ Close on desktop <span className="hint">the session keeps running</span>
            </button>
          )}
          {s4 && (
            <button
              type="button"
              className="button"
              onClick={() => {
                onClose();
                nav.push({ kind: 'tree', session, title });
              }}
            >
              ⑂ Session tree <span className="hint">fork · go here</span>
            </button>
          )}
          {live === null || live.status.state === 'exited' ? (
            <button
              type="button"
              className="button"
              disabled={busy || !can({ op: 'resume', session })}
              onClick={() =>
                void (async () => {
                  setBusy(true);
                  const view = await perform(client, { op: 'resume', session }, `Resume “${title}”`);
                  setBusy(false);
                  const result = doneResult(view);
                  if (result?.type === 'session') {
                    onClose();
                    nav.push({ kind: 'conversation', session, liveKey: result.live.key, title });
                  }
                })()
              }
            >
              ↻ Resume <span className="hint">start omp on it</span>
            </button>
          ) : (
            <>
              <button type="button" className="button" disabled={!can({ op: 'restart', live: live.live })} onClick={() => setMode('restart')}>
                ↻ Restart
              </button>
              <button type="button" className="button" disabled={!can({ op: 'stop', live: live.live })} onClick={() => setMode('stop')}>
                ■ Stop <span className="hint">this session only</span>
              </button>
            </>
          )}
          {live !== null && (
            <button type="button" className="button danger-soft" disabled={!can({ op: 'kill', live: live.live })} onClick={() => setMode('kill')}>
              ✕ Kill <span className="hint">this session only</span>
            </button>
          )}
          <button
            type="button"
            className="button"
            disabled={renameOp === null || !live?.live_control || !can(renameOp)}
            onClick={() => setMode('rename')}
          >
            ✎ Rename <span className="hint">{live?.live_control === true ? 'omp /rename' : 'needs it running here'}</span>
          </button>
          <button type="button" className="button" disabled={!can({ op: 'copy_session', session, project: '' })} onClick={() => setMode('copy')}>
            ⎘ Copy to project…
          </button>
          <button type="button" className="button" disabled={!can({ op: 'move_session', session, project: '' })} onClick={() => setMode('move')}>
            ⇢ Move to project…
          </button>
          <button
            type="button"
            className="button danger"
            disabled={!can({ op: 'delete_sessions', sessions: [session] })}
            onClick={() => setMode('delete')}
          >
            ⌫ Delete…
          </button>
        </div>
      )}
    </Sheet>
  );
}

export function ProjectPicker({
  title,
  projects,
  loading = false,
  busy = false,
  onPick,
  onClose,
}: {
  title: string;
  projects: readonly api.Project[];
  loading?: boolean;
  busy?: boolean;
  onPick: (project: api.Project) => void;
  onClose: () => void;
}): ReactNode {
  const [filter, setFilter] = useState('');
  const needle = filter.trim().toLowerCase();
  return (
    <Sheet title={title} icon="▣" onClose={onClose}>
      <input
        className="input"
        placeholder="Filter folders"
        value={filter}
        onChange={(event) => setFilter(event.target.value)}
        style={{ marginBottom: 10 }}
      />
      <div className="option-list">
        {loading && <p className="muted small">Loading projects…</p>}
        {projects
          .filter((project) => needle.length === 0 || project.display.toLowerCase().includes(needle))
          .map((project) => (
            <button key={project.id} type="button" className="option" disabled={busy} onClick={() => onPick(project)}>
              <span className="grow">
                {project.display}
                <small>
                  {plural(project.session_count, 'session')}
                  {project.worktree ? ' · worktree' : ''}
                </small>
              </span>
              <span className="muted">›</span>
            </button>
          ))}
      </div>
    </Sheet>
  );
}
