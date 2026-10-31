// Session manager (`inventory`): every session file, stubs included, as cards with size,
// messages, tokens, cost, model, subagents, created and last activity. Filters (text, older
// than, larger than, empty stubs, folder missing, not running) and sorting; multi-select with
// totals and bulk delete/copy/move/stop; a detail sheet with the full breakdown, parent and
// forks, and the per-session actions. Running (RAM, kill) is one tap away.

import { type ReactNode, useMemo, useState } from 'react';
import type { api } from '../../remote/index.ts';
import { useNav } from '../nav.ts';
import { age, dollars, perform, shortModel, succeeded, tokens } from '../ui/format.ts';
import { useCan, useClient, useFeed, useNow } from '../ui/hooks.ts';
import { ConfirmSheet, Empty, NeedsS4, Sheet, StaleBanner, useS4 } from '../ui/kit.tsx';
import {
  INVENTORY_SORTS,
  type InventoryFilter,
  type InventorySort,
  NO_FILTER,
  activeFilters,
  bytes,
  entryActivity,
  entryCost,
  entryMessages,
  entrySize,
  entryTitle,
  entryTokens,
  filterInventory,
  forksOf,
  plural,
  selectionTotals,
} from './fleet/logic.ts';
import { ProjectPicker, SessionActionsSheet } from './fleet/SessionActions.tsx';
import '../../styles/fleet.css';

const LIMIT = 500;
const OLDER: readonly number[] = [0, 7, 30, 90, 365];
const LARGER: readonly number[] = [0, 1_000_000, 10_000_000, 100_000_000];

type Bulk = 'delete' | 'copy' | 'move' | 'stop';

export function ManagerScreen(): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const s4 = useS4();
  const now = useNow();
  const inventory = useFeed(s4 ? { feed: 'inventory', limit: LIMIT } : null, 'inventory');
  const liveFeed = useFeed({ feed: 'live' }, 'live');
  const [filter, setFilter] = useState<InventoryFilter>(NO_FILTER);
  const [sort, setSort] = useState<InventorySort>('activity');
  const [ascending, setAscending] = useState(false);
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [detail, setDetail] = useState<string | null>(null);
  const [actionsFor, setActionsFor] = useState<api.InventoryView | null>(null);
  const [bulk, setBulk] = useState<Bulk | null>(null);
  const [busy, setBusy] = useState(false);
  const projectsFeed = useFeed(bulk === 'copy' || bulk === 'move' ? { feed: 'projects' } : null, 'projects');

  const entries = inventory?.entries ?? [];
  const byId = useMemo(() => new Map(entries.map((entry) => [entry.id, entry])), [entries]);
  const shown = useMemo(() => filterInventory(entries, filter, sort, ascending, now), [entries, filter, sort, ascending, now]);
  const chosen = entries.filter((entry) => selected.has(entry.id));
  const totals = selectionTotals(chosen);
  const liveBySession = useMemo(() => {
    const map = new Map<string, api.LiveSession>();
    for (const live of liveFeed?.sessions ?? []) if (live.session !== null && live.status.state !== 'exited') map.set(live.session, live);
    return map;
  }, [liveFeed]);
  const runningChosen = chosen.flatMap((entry) => {
    const live = liveBySession.get(entry.id);
    return live === undefined ? [] : [live];
  });
  const canManage = can({ op: 'delete_sessions', sessions: [] });
  const detailEntry = detail === null ? null : (byId.get(detail) ?? null);

  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  };
  const allShown = shown.length > 0 && shown.every((entry) => selected.has(entry.id));

  const runBulk = async (work: () => Promise<boolean>) => {
    setBusy(true);
    try {
      if (await work()) setSelected(new Set());
    } finally {
      setBusy(false);
      setBulk(null);
    }
  };
  const deleteChosen = () =>
    runBulk(async () =>
      succeeded(
        await perform(
          client,
          { op: 'delete_sessions', sessions: chosen.map((entry) => entry.id) },
          `Delete ${plural(chosen.length, 'session')}`,
          `Deleted ${plural(chosen.length, 'session')}, ${bytes(totals.bytes)} freed.`,
        ),
      ),
    );
  const copyOrMove = (op: 'copy_session' | 'move_session', project: api.Project) =>
    runBulk(async () => {
      let ok = 0;
      for (const entry of chosen) {
        const view = await perform(client, { op, session: entry.id, project: project.id }, `${op === 'copy_session' ? 'Copy' : 'Move'} “${entryTitle(entry)}”`);
        if (succeeded(view)) ok++;
      }
      if (ok > 0) client.notify(`${op === 'copy_session' ? 'Copied' : 'Moved'} ${plural(ok, 'session')} to ${project.display}.`);
      return ok === chosen.length;
    });
  const stopChosen = () =>
    runBulk(async () => {
      let ok = 0;
      for (const live of runningChosen) {
        if (succeeded(await perform(client, { op: 'stop', live: live.live }, `Stop “${live.title ?? live.label}”`))) ok++;
      }
      return ok === runningChosen.length;
    });

  const setOlder = () => setFilter({ ...filter, olderThanDays: OLDER[(OLDER.indexOf(filter.olderThanDays) + 1) % OLDER.length] ?? 0 });
  const setLarger = () => setFilter({ ...filter, largerThan: LARGER[(LARGER.indexOf(filter.largerThan) + 1) % LARGER.length] ?? 0 });

  return (
    <div className="screen pushed manager">
      <header className="top">
        <div className="eyebrow">Session manager</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className="grow">All sessions</h2>
          <button type="button" className="chip accent" onClick={() => nav.push({ kind: 'running' })}>
            ◉ Running
          </button>
        </div>
      </header>
      <div className="body">
        <StaleBanner />
        <NeedsS4 what="The session manager">
          <label className="search">
            <span aria-hidden="true">⌕</span>
            <input
              type="search"
              placeholder="Search title, prompt, folder, model"
              value={filter.text}
              onChange={(event) => setFilter({ ...filter, text: event.target.value })}
              aria-label="Search sessions"
            />
          </label>
          <div className="filter-row" role="group" aria-label="Filters">
            <button type="button" className="chip" aria-pressed={filter.olderThanDays > 0} onClick={setOlder}>
              {filter.olderThanDays > 0 ? `Older than ${filter.olderThanDays}d` : 'Older than…'}
            </button>
            <button type="button" className="chip" aria-pressed={filter.largerThan > 0} onClick={setLarger}>
              {filter.largerThan > 0 ? `Larger than ${bytes(filter.largerThan)}` : 'Larger than…'}
            </button>
            <button type="button" className="chip" aria-pressed={filter.stubsOnly} onClick={() => setFilter({ ...filter, stubsOnly: !filter.stubsOnly })}>
              Empty stubs
            </button>
            <button type="button" className="chip" aria-pressed={filter.missingOnly} onClick={() => setFilter({ ...filter, missingOnly: !filter.missingOnly })}>
              Folder missing
            </button>
            <button type="button" className="chip" aria-pressed={filter.notRunningOnly} onClick={() => setFilter({ ...filter, notRunningOnly: !filter.notRunningOnly })}>
              Not running
            </button>
            {activeFilters(filter) > 0 && (
              <button type="button" className="chip danger" onClick={() => setFilter(NO_FILTER)}>
                ✕ Clear {activeFilters(filter)}
              </button>
            )}
          </div>
          <div className="sort-row">
            <span className="k">Sort</span>
            <select className="input slim" value={sort} onChange={(event) => setSort(event.target.value as InventorySort)} aria-label="Sort by">
              {INVENTORY_SORTS.map(([id, label]) => (
                <option key={id} value={id}>
                  {label}
                </option>
              ))}
            </select>
            <button type="button" className="chip" onClick={() => setAscending(!ascending)} aria-label={ascending ? 'Ascending' : 'Descending'}>
              {ascending ? '↑ asc' : '↓ desc'}
            </button>
            <span className="grow" />
            <button
              type="button"
              className="chip"
              disabled={shown.length === 0}
              onClick={() => {
                const next = new Set(selected);
                for (const entry of shown) {
                  if (allShown) next.delete(entry.id);
                  else next.add(entry.id);
                }
                setSelected(next);
              }}
            >
              {allShown ? 'Deselect shown' : 'Select shown'}
            </button>
          </div>
          {inventory !== undefined && (
            <p className="muted small list-note">
              {shown.length} shown of {entries.length}
              {inventory.total > entries.length ? ` (newest ${entries.length} of ${inventory.total} loaded)` : ''}
            </p>
          )}
          {inventory === undefined ? (
            <Empty title="Loading the inventory…">The desktop scans every session file.</Empty>
          ) : shown.length === 0 ? (
            <Empty title={entries.length === 0 ? 'No sessions' : 'Nothing matches'}>{entries.length === 0 ? undefined : 'Loosen the filters.'}</Empty>
          ) : (
            shown.map((entry) => (
              <InventoryCard
                key={entry.id}
                entry={entry}
                now={now}
                selected={selected.has(entry.id)}
                onToggle={() => toggle(entry.id)}
                onOpen={() => setDetail(entry.id)}
              />
            ))
          )}
          {inventory?.truncated === true && <p className="muted small">Only the newest sessions were sent.</p>}
        </NeedsS4>
      </div>

      {chosen.length > 0 && (
        <div className="fab bulk-bar totals-bar" role="toolbar" aria-label="Selected sessions">
          <div className="totals">
            <b>{chosen.length} selected</b>
            <span>
              {bytes(totals.bytes)} · {tokens(totals.tokens)} tok · {dollars(totals.cost)}
            </span>
          </div>
          <div className="bulk-buttons">
            <button type="button" className="button" disabled={!canManage} onClick={() => setBulk('copy')}>
              Copy
            </button>
            <button type="button" className="button" disabled={!canManage} onClick={() => setBulk('move')}>
              Move
            </button>
            <button type="button" className="button" disabled={runningChosen.length === 0} onClick={() => setBulk('stop')}>
              Stop
            </button>
            <button type="button" className="button danger" disabled={!canManage} onClick={() => setBulk('delete')}>
              Delete
            </button>
          </div>
        </div>
      )}

      {detailEntry !== null && actionsFor === null && (
        <DetailSheet
          entry={detailEntry}
          now={now}
          parent={detailEntry.parent === null ? null : (byId.get(detailEntry.parent) ?? null)}
          forks={forksOf(entries, detailEntry.id)}
          onGo={setDetail}
          onActions={() => setActionsFor(detailEntry)}
          onClose={() => setDetail(null)}
        />
      )}
      {actionsFor !== null && (
        <SessionActionsSheet
          session={actionsFor.id}
          title={entryTitle(actionsFor)}
          display={actionsFor.display}
          entry={actionsFor}
          onClose={() => {
            setActionsFor(null);
            setDetail(null);
          }}
        />
      )}
      {bulk === 'delete' && (
        <ConfirmSheet
          title={`Delete ${plural(chosen.length, 'session')}?`}
          message="Their history is permanently removed from the desktop. This cannot be undone."
          facts={[
            ['Sessions', `${chosen.length}${totals.stubs > 0 ? ` (${totals.stubs} empty stubs)` : ''}`],
            ['Disk freed', bytes(totals.bytes)],
            ['Messages', totals.messages],
            ...chosen.slice(0, 4).map((entry, index) => [`#${index + 1}`, `${entryTitle(entry)} · ${bytes(entrySize(entry))}`] as const),
            ...(chosen.length > 4 ? [['More', `${chosen.length - 4} others`] as const] : []),
            ...(totals.running > 0 ? [['Running', `${totals.running} (stopped first)`] as const] : []),
          ]}
          confirm={`Delete ${chosen.length}`}
          busy={busy}
          onConfirm={() => void deleteChosen()}
          onCancel={() => setBulk(null)}
        />
      )}
      {bulk === 'stop' && (
        <ConfirmSheet
          title={`Stop ${plural(runningChosen.length, 'running session')}?`}
          message="omp is asked to exit in each selected session; work in progress ends. Unselected sessions keep running."
          facts={runningChosen.slice(0, 5).map((live, index) => [`#${index + 1}`, live.title ?? live.label] as const)}
          confirm="Stop"
          busy={busy}
          onConfirm={() => void stopChosen()}
          onCancel={() => setBulk(null)}
        />
      )}
      {(bulk === 'copy' || bulk === 'move') && (
        <ProjectPicker
          title={`${bulk === 'copy' ? 'Copy' : 'Move'} ${plural(chosen.length, 'session')} to…`}
          projects={projectsFeed?.projects ?? []}
          loading={projectsFeed === undefined}
          busy={busy}
          onPick={(project) => void copyOrMove(bulk === 'copy' ? 'copy_session' : 'move_session', project)}
          onClose={() => setBulk(null)}
        />
      )}
    </div>
  );
}

function Badges({ entry }: { entry: api.InventoryView }): ReactNode {
  return (
    <span className="badges">
      {entry.running && <span className="chip ok">● running</span>}
      {entry.stub && <span className="chip warn">empty stub</span>}
      {!entry.cwd_exists && <span className="chip danger">folder missing</span>}
      {entry.parent !== null && <span className="chip">⑂ fork</span>}
      {entry.subagents > 0 && <span className="chip accent">◇ {entry.subagents}</span>}
    </span>
  );
}

function InventoryCard({
  entry,
  now,
  selected,
  onToggle,
  onOpen,
}: {
  entry: api.InventoryView;
  now: number;
  selected: boolean;
  onToggle: () => void;
  onOpen: () => void;
}): ReactNode {
  const cost = entryCost(entry);
  return (
    <article className={`fleet-card inv-card${selected ? ' selected' : ''}${entry.stub ? ' stub' : ''}`}>
      <div className="inv-head">
        <button type="button" className="check-btn" role="checkbox" aria-checked={selected} aria-label={`Select ${entryTitle(entry)}`} onClick={onToggle}>
          <span className="check">{selected ? '✓' : ''}</span>
        </button>
        <button type="button" className="inv-title" onClick={onOpen}>
          <b>{entryTitle(entry)}</b>
          <small className="mono">{entry.display}</small>
        </button>
        <span className="age">{age(entryActivity(entry), now)}</span>
      </div>
      <Badges entry={entry} />
      <button type="button" className="metrics" onClick={onOpen} aria-label="Details">
        <span>
          <i>Size</i>
          <b>{bytes(entrySize(entry))}</b>
        </span>
        <span>
          <i>Msgs</i>
          <b>{entryMessages(entry)}</b>
        </span>
        <span>
          <i>Tokens</i>
          <b>{tokens(entryTokens(entry))}</b>
        </span>
        <span>
          <i>Cost</i>
          <b>{cost === null ? '—' : dollars(cost)}</b>
        </span>
        <span className="wide">
          <i>Model</i>
          <b>{entry.stats?.model == null ? '—' : shortModel(entry.stats.model)}</b>
        </span>
        <span className="wide">
          <i>Created</i>
          <b>{entry.created_unix_ms === null ? '—' : `${age(entry.created_unix_ms, now)} ago`}</b>
        </span>
      </button>
    </article>
  );
}

function DetailSheet({
  entry,
  now,
  parent,
  forks,
  onGo,
  onActions,
  onClose,
}: {
  entry: api.InventoryView;
  now: number;
  parent: api.InventoryView | null;
  forks: readonly api.InventoryView[];
  onGo: (id: string) => void;
  onActions: () => void;
  onClose: () => void;
}): ReactNode {
  const nav = useNav();
  const stats = entry.stats;
  const when = (ms: number | null) => (ms === null ? '—' : `${new Date(ms).toLocaleString()} · ${age(ms, now)} ago`);
  const rows: (readonly [string, ReactNode])[] = [
    ['Folder', <span key="f" className="mono">{entry.display}{entry.cwd_exists ? '' : ' (missing)'}</span>],
    ['Session file', bytes(entry.jsonl_bytes)],
    ['Artifacts', `${plural(entry.artifact_files, 'file')} · ${bytes(entry.artifact_bytes)}`],
    ['Your messages', stats?.user_messages ?? 0],
    ['Assistant turns', stats?.assistant_turns ?? 0],
    ['Input tokens', tokens(stats?.input_tokens ?? 0)],
    ['Output tokens', tokens(stats?.output_tokens ?? 0)],
    ['Cache read / write', `${tokens(stats?.cache_read_tokens ?? 0)} / ${tokens(stats?.cache_write_tokens ?? 0)}`],
    ['Cost', stats?.cost == null ? '—' : dollars(stats.cost)],
    ['Subagent cost', stats?.subagent_cost == null ? '—' : dollars(stats.subagent_cost)],
    ['Model', stats?.model ?? '—'],
    ['Compactions', stats?.compactions ?? 0],
    ['Subagents', entry.subagents],
    ['Created', when(entry.created_unix_ms)],
    ['Last activity', when(entryActivity(entry))],
    ['Modified', when(entry.modified_unix_ms)],
  ];
  return (
    <Sheet title={entryTitle(entry)} icon="▤" onClose={onClose}>
      <Badges entry={entry} />
      {entry.first_prompt !== null && entry.title !== null && <p className="node-text">{entry.first_prompt}</p>}
      <dl className="facts wide breakdown">
        {rows.map(([key, value]) => (
          <div key={key}>
            <dt>{key}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
      {(entry.parent !== null || forks.length > 0) && (
        <div className="lineage">
          <span className="eyebrow">Lineage</span>
          {entry.parent !== null && (
            <button type="button" className="option" disabled={parent === null} onClick={() => parent !== null && onGo(parent.id)}>
              <span className="grow">
                ↖ Forked from {parent === null ? 'a session not loaded here' : entryTitle(parent)}
                <small className="mono">{entry.parent}</small>
              </span>
              {parent !== null && <span className="muted">›</span>}
            </button>
          )}
          {forks.map((fork) => (
            <button key={fork.id} type="button" className="option" onClick={() => onGo(fork.id)}>
              <span className="grow">
                ↘ Fork: {entryTitle(fork)}
                <small>{age(entryActivity(fork), now)} ago</small>
              </span>
              <span className="muted">›</span>
            </button>
          ))}
        </div>
      )}
      <div className="buttons pair">
        <button
          type="button"
          className="button"
          onClick={() => {
            onClose();
            nav.push({ kind: 'conversation', session: entry.id, liveKey: null, title: entryTitle(entry) });
          }}
        >
          Open
        </button>
        <button type="button" className="button primary" onClick={onActions}>
          Actions…
        </button>
      </div>
    </Sheet>
  );
}
