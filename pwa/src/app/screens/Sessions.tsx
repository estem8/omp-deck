// Sessions: projects with their newest sessions (empty stubs hidden, as the desktop sidebar),
// linked worktrees nested under their repository with a branch chip, search, running
// indicators, new session (per project, or in any desktop folder through the browser), a
// per-row actions sheet, and a select mode for bulk delete/copy/move/stop.

import { type ReactNode, useEffect, useMemo, useState } from 'react';
import type { api } from '../../remote/index.ts';
import { feedKey } from '../client.ts';
import { useNav } from '../nav.ts';
import { age, perform, shortModel, statusLabel, succeeded } from '../ui/format.ts';
import { useCan, useClient, useClientState, useFeed, useNow } from '../ui/hooks.ts';
import { ConfirmSheet, ConnectionPill, Empty, InstallHint, Sheet, StaleBanner, UpdateBanner, useS4 } from '../ui/kit.tsx';
import { doneResult } from './fleet/common.ts';
import { type WorktreeLink, nestWorktrees, plural } from './fleet/logic.ts';
import { ProjectPicker, SessionActionsSheet } from './fleet/SessionActions.tsx';
import '../../styles/fleet.css';

/** Expanded project groups each hold a `sessions` feed; the oldest expansion closes past this. */
const MAX_EXPANDED = 6;
const FIRST_EXPANDED = 3;
const PAGE = 20;
/** Worktree projects whose repository is looked up at a time (one `worktrees` feed each). */
const LINK_LOOKUPS = 3;

interface Selected {
  id: string;
  title: string;
  project: string;
}

interface RowActions {
  session: string;
  title: string;
  display: string;
}

/** Worktree → repository links learnt so far (project ids are stable per desktop). */
const linkCache = new Map<string, WorktreeLink>();

/**
 * The main checkout and branch of each linked-worktree project, looked up through short-lived
 * `worktrees` subscriptions (a few at a time, each released once answered) and cached.
 */
function useWorktreeLinks(projects: readonly api.Project[]): ReadonlyMap<string, WorktreeLink> {
  const client = useClient();
  const s4 = useS4();
  const feeds = useClientState((state) => state.feeds);
  const [links, setLinks] = useState<ReadonlyMap<string, WorktreeLink>>(() => new Map(linkCache));
  const pending = projects
    .filter((project) => project.worktree && !links.has(project.id))
    .slice(0, LINK_LOOKUPS)
    .map((project) => project.id);
  const key = pending.join('\n');

  useEffect(() => {
    if (!s4 || pending.length === 0) return;
    const releases = pending.map((project) => client.follow({ feed: 'worktrees', project }));
    return () => {
      for (const release of releases) release?.();
    };
    // `key` identifies `pending`.
  }, [client, s4, key]);

  useEffect(() => {
    let next: Map<string, WorktreeLink> | null = null;
    for (const project of pending) {
      const value = feeds.get(feedKey({ feed: 'worktrees', project }));
      if (value?.feed !== 'worktrees') continue;
      const link = { primary: value.worktree?.primary ?? null, branch: value.worktree?.branch ?? null };
      linkCache.set(project, link);
      next ??= new Map(links);
      next.set(project, link);
    }
    if (next !== null) setLinks(next);
  }, [feeds, key]);
  return links;
}

export function SessionsScreen({ standalone }: { standalone: boolean }): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const s4 = useS4();
  const projectsFeed = useFeed({ feed: 'projects' }, 'projects');
  const liveFeed = useFeed({ feed: 'live' }, 'live');
  const [query, setQuery] = useState('');
  /** Most recent expansion last. null = the default (the newest projects). */
  const [expanded, setExpanded] = useState<string[] | null>(null);
  const [limits, setLimits] = useState<ReadonlyMap<string, number>>(new Map());
  const [selecting, setSelecting] = useState(false);
  const [selected, setSelected] = useState<ReadonlyMap<string, Selected>>(new Map());
  const [newOpen, setNewOpen] = useState(false);
  const [rowActions, setRowActions] = useState<RowActions | null>(null);
  const [sheet, setSheet] = useState<'delete' | 'copy' | 'move' | 'stop' | null>(null);
  const [busy, setBusy] = useState(false);

  const projects = useMemo(
    () => [...(projectsFeed?.projects ?? [])].sort((a, b) => b.modified_unix_ms - a.modified_unix_ms),
    [projectsFeed],
  );
  const links = useWorktreeLinks(projects);
  const groups = useMemo(() => nestWorktrees(projects, links), [projects, links]);
  const open = expanded ?? groups.slice(0, FIRST_EXPANDED).map((group) => group.project.id);
  const liveBySession = useMemo(() => {
    const map = new Map<string, api.LiveSession>();
    for (const live of liveFeed?.sessions ?? []) if (live.session !== null) map.set(live.session, live);
    return map;
  }, [liveFeed]);

  const toggle = (id: string) => {
    if (open.includes(id)) {
      setExpanded(open.filter((other) => other !== id));
      return;
    }
    const next = [...open, id];
    setExpanded(next.length > MAX_EXPANDED ? next.slice(next.length - MAX_EXPANDED) : next);
  };

  const newSession = async (project: api.Project) => {
    setNewOpen(false);
    const result = doneResult(await perform(client, { op: 'new_session', project: project.id }, `New session in ${project.display}`));
    if (result?.type === 'session') {
      nav.push({ kind: 'conversation', session: null, liveKey: result.live.key, title: project.display });
    }
  };

  const toggleSelected = (entry: Selected) => {
    const next = new Map(selected);
    if (next.has(entry.id)) next.delete(entry.id);
    else next.set(entry.id, entry);
    setSelected(next);
  };
  const endSelect = () => {
    setSelecting(false);
    setSelected(new Map());
    setSheet(null);
  };
  const chosen = [...selected.values()];
  const runningChosen = chosen.flatMap((entry) => {
    const live = liveBySession.get(entry.id);
    return live === undefined || live.status.state === 'exited' ? [] : [live];
  });

  const bulk = async (run: () => Promise<boolean>) => {
    setBusy(true);
    try {
      if (await run()) endSelect();
      else setSheet(null);
    } finally {
      setBusy(false);
    }
  };
  const deleteChosen = () =>
    bulk(async () =>
      succeeded(
        await perform(
          client,
          { op: 'delete_sessions', sessions: chosen.map((entry) => entry.id) },
          `Delete ${plural(chosen.length, 'session')}`,
          `Deleted ${plural(chosen.length, 'session')}.`,
        ),
      ),
    );
  const copyOrMove = (op: 'copy_session' | 'move_session', project: api.Project) =>
    bulk(async () => {
      let ok = 0;
      for (const entry of chosen) {
        const verb = op === 'copy_session' ? 'Copy' : 'Move';
        const view = await perform(client, { op, session: entry.id, project: project.id }, `${verb} “${entry.title}”`);
        if (succeeded(view)) ok++;
      }
      if (ok > 0) client.notify(`${op === 'copy_session' ? 'Copied' : 'Moved'} ${plural(ok, 'session')} to ${project.display}.`);
      return ok === chosen.length;
    });
  const stopChosen = () =>
    bulk(async () => {
      let ok = 0;
      for (const live of runningChosen) {
        if (succeeded(await perform(client, { op: 'stop', live: live.live }, `Stop “${live.title ?? live.label}”`))) ok++;
      }
      return ok === runningChosen.length;
    });

  const needle = query.trim().toLowerCase();
  // Searching also loads the newest projects' sessions, within the same feed budget.
  const searchOpen = new Set(
    needle.length === 0
      ? []
      : projects
          .filter((project) => !open.includes(project.id))
          .slice(0, Math.max(0, MAX_EXPANDED - open.length))
          .map((project) => project.id),
  );
  const sampleProject = projects[0]?.id ?? '';
  const canManage = can({ op: 'delete_sessions', sessions: [] });
  const canNew = can({ op: 'new_session', project: sampleProject });

  const groupProps = {
    needle,
    live: liveBySession,
    selecting,
    selected,
    canNew,
    s4,
    onSelect: toggleSelected,
    onOpen: (session: string, title: string) => nav.push({ kind: 'conversation', session, liveKey: null, title }),
    onActions: setRowActions,
  };

  return (
    <div className="screen">
      <header className="top">
        <div className="line spread">
          <h2>OmpDeck</h2>
          <div className="line">
            {s4 && (
              <button type="button" className="icon-btn" onClick={() => nav.push({ kind: 'running' })} aria-label="Running sessions" title="Running">
                ◉
              </button>
            )}
            {s4 && (
              <button type="button" className="icon-btn" onClick={() => nav.push({ kind: 'manager' })} aria-label="Session manager" title="Session manager">
                ▤
              </button>
            )}
            <button
              type="button"
              className="icon-btn"
              aria-pressed={selecting}
              onClick={() => (selecting ? endSelect() : setSelecting(true))}
            >
              {selecting ? 'Done' : 'Select'}
            </button>
          </div>
        </div>
        <ConnectionPill />
      </header>
      <div className="body">
        <UpdateBanner />
        <StaleBanner />
        <InstallHint standalone={standalone} />
        <label className="search">
          <span aria-hidden="true">⌕</span>
          <input
            type="search"
            placeholder="Search sessions"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            aria-label="Search sessions"
          />
          <span className="key">/</span>
        </label>
        {projectsFeed === undefined ? (
          <Empty title="Loading projects…">The desktop sends its session list once connected.</Empty>
        ) : projects.length === 0 ? (
          <Empty title="No sessions yet">Start one with New session.</Empty>
        ) : (
          groups.map(({ project, children }) => (
            <section key={project.id} className="project">
              <ProjectGroup
                {...groupProps}
                project={project}
                branch={null}
                expanded={open.includes(project.id) || searchOpen.has(project.id)}
                limit={limits.get(project.id) ?? PAGE}
                onToggle={() => toggle(project.id)}
                onMore={() => setLimits(new Map(limits).set(project.id, (limits.get(project.id) ?? PAGE) + 40))}
                onNew={() => void newSession(project)}
              />
              {children.map((child) => (
                <div key={child.project.id} className="wt-nest">
                  <ProjectGroup
                    {...groupProps}
                    project={child.project}
                    branch={child.branch}
                    expanded={open.includes(child.project.id) || searchOpen.has(child.project.id)}
                    limit={limits.get(child.project.id) ?? PAGE}
                    onToggle={() => toggle(child.project.id)}
                    onMore={() => setLimits(new Map(limits).set(child.project.id, (limits.get(child.project.id) ?? PAGE) + 40))}
                    onNew={() => void newSession(child.project)}
                  />
                </div>
              ))}
            </section>
          ))
        )}
        {projectsFeed?.truncated === true && <p className="muted small">Only the newest projects are listed.</p>}
      </div>
      {selecting ? (
        <div className="fab bulk-bar" role="toolbar" aria-label="Selected sessions">
          <span>{selected.size} selected</span>
          <button type="button" className="button" disabled={chosen.length === 0 || !canManage} onClick={() => setSheet('copy')}>
            Copy
          </button>
          <button type="button" className="button" disabled={chosen.length === 0 || !canManage} onClick={() => setSheet('move')}>
            Move
          </button>
          <button type="button" className="button" disabled={runningChosen.length === 0} onClick={() => setSheet('stop')}>
            Stop
          </button>
          <button type="button" className="button danger" disabled={chosen.length === 0 || !canManage} onClick={() => setSheet('delete')}>
            Delete
          </button>
        </div>
      ) : (
        <button type="button" className="fab" disabled={!canNew} onClick={() => setNewOpen(true)}>
          + New session
        </button>
      )}
      {newOpen && (
        <NewSessionSheet
          projects={projects}
          s4={s4}
          onPick={(project) => void newSession(project)}
          onBrowse={() => {
            setNewOpen(false);
            nav.push({ kind: 'browse' });
          }}
          onClose={() => setNewOpen(false)}
        />
      )}
      {rowActions !== null && (
        <SessionActionsSheet
          session={rowActions.session}
          title={rowActions.title}
          display={rowActions.display}
          onClose={() => setRowActions(null)}
        />
      )}
      {sheet === 'delete' && (
        <ConfirmSheet
          title={`Delete ${plural(chosen.length, 'session')}?`}
          message="This permanently removes their local history on the desktop. This action cannot be undone."
          facts={[
            ['Sessions', chosen.length],
            ...chosen.slice(0, 4).map((entry, index) => [`#${index + 1}`, entry.title] as const),
            ...(chosen.length > 4 ? [['More', `${chosen.length - 4} others`] as const] : []),
            ...(runningChosen.length > 0 ? [['Running', `${runningChosen.length} (stopped first)`] as const] : []),
          ]}
          confirm="Delete"
          busy={busy}
          onConfirm={() => void deleteChosen()}
          onCancel={() => setSheet(null)}
        />
      )}
      {sheet === 'stop' && (
        <ConfirmSheet
          title={`Stop ${plural(runningChosen.length, 'running session')}?`}
          message="omp is asked to exit in each selected session; unsaved work in progress ends. Other sessions keep running."
          facts={runningChosen.slice(0, 5).map((live, index) => [`#${index + 1}`, live.title ?? live.label] as const)}
          confirm="Stop"
          busy={busy}
          onConfirm={() => void stopChosen()}
          onCancel={() => setSheet(null)}
        />
      )}
      {(sheet === 'copy' || sheet === 'move') && (
        <ProjectPicker
          title={`${sheet === 'copy' ? 'Copy' : 'Move'} ${plural(chosen.length, 'session')} to…`}
          projects={projects}
          busy={busy}
          onPick={(project) => void copyOrMove(sheet === 'copy' ? 'copy_session' : 'move_session', project)}
          onClose={() => setSheet(null)}
        />
      )}
    </div>
  );
}

function ProjectGroup({
  project,
  branch,
  expanded,
  limit,
  needle,
  live,
  selecting,
  selected,
  canNew,
  s4,
  onToggle,
  onMore,
  onNew,
  onSelect,
  onOpen,
  onActions,
}: {
  project: api.Project;
  /** Set for a linked worktree nested under its repository. */
  branch: string | null;
  expanded: boolean;
  limit: number;
  needle: string;
  live: ReadonlyMap<string, api.LiveSession>;
  selecting: boolean;
  selected: ReadonlyMap<string, Selected>;
  canNew: boolean;
  s4: boolean;
  onToggle: () => void;
  onMore: () => void;
  onNew: () => void;
  onSelect: (entry: Selected) => void;
  onOpen: (session: string, title: string) => void;
  onActions: (row: RowActions) => void;
}): ReactNode {
  const nav = useNav();
  const now = useNow();
  const feed = useFeed(expanded ? { feed: 'sessions', project: project.id, limit } : null, 'sessions');
  const projectMatches = needle.length === 0 || project.display.toLowerCase().includes(needle) || (branch ?? '').toLowerCase().includes(needle);
  const all = feed?.sessions ?? [];
  const stubs = all.filter((session) => session.stub === true).length;
  const sessions = all.filter(
    (session) =>
      session.stub !== true &&
      (projectMatches ||
        (session.title ?? '').toLowerCase().includes(needle) ||
        (session.first_prompt ?? '').toLowerCase().includes(needle)),
  );
  if (needle.length > 0 && !projectMatches && sessions.length === 0) return null;
  const nested = branch !== null || project.worktree;
  return (
    <>
      <div className={`project-title${nested ? ' wt-title' : ''}`}>
        <button type="button" className="name" onClick={onToggle} aria-expanded={expanded}>
          {expanded ? '⌄' : '›'} {branch !== null ? <span className="chip accent branch-chip">⎇ {branch}</span> : project.display}
          {branch === null && project.worktree && <span className="chip accent small"> worktree</span>}
        </button>
        {s4 && (
          <button
            type="button"
            className="row-btn"
            onClick={() => nav.push({ kind: 'worktrees', project: project.id, display: project.display })}
            aria-label={`Worktrees of ${project.display}`}
            title="Worktrees"
          >
            ⎇
          </button>
        )}
        <button type="button" className="row-btn" disabled={!canNew} onClick={onNew} aria-label={`New session in ${project.display}`} title="New session">
          +
        </button>
        <span className="count">{project.session_count}</span>
      </div>
      {expanded &&
        (feed === undefined ? (
          <p className="muted small">Loading…</p>
        ) : (
          <>
            {sessions.map((session) => {
              const running = live.get(session.id) ?? null;
              const title = session.title ?? session.first_prompt ?? 'Untitled session';
              const isSelected = selected.has(session.id);
              return (
                <div
                  key={session.id}
                  className={`session row-with-actions${running?.status.state === 'attention' ? ' attention' : ''}${running?.busy === true ? ' active' : ''}${isSelected ? ' selected' : ''}`}
                >
                  <button
                    type="button"
                    className="row-main"
                    aria-pressed={selecting ? isSelected : undefined}
                    onClick={() => (selecting ? onSelect({ id: session.id, title, project: project.id }) : onOpen(session.id, title))}
                  >
                    {selecting ? <span className="check">{isSelected ? '✓' : ''}</span> : <Indicator live={running} />}
                    <span className="copy">
                      <b>{title}</b>
                      <small>
                        {running === null ? (session.first_prompt ?? 'Not running') : `${shortModel(running.model)} · ${statusLabel(running)}`}
                      </small>
                    </span>
                    <span className="age">{age(session.modified_unix_ms, now)}</span>
                  </button>
                  {!selecting && (
                    <button
                      type="button"
                      className="row-btn more-btn"
                      aria-label={`Actions for ${title}`}
                      onClick={() => onActions({ session: session.id, title, display: project.display })}
                    >
                      ⋯
                    </button>
                  )}
                </div>
              );
            })}
            {sessions.length === 0 && needle.length === 0 && (
              <p className="muted small">{stubs > 0 ? `Only ${plural(stubs, 'empty stub')} (hidden).` : 'No sessions.'}</p>
            )}
            {feed.total > all.length && needle.length === 0 && (
              <button type="button" className="more-row" onClick={onMore}>
                Show more ({feed.total - all.length} older)
              </button>
            )}
          </>
        ))}
    </>
  );
}

function Indicator({ live }: { live: api.LiveSession | null }): ReactNode {
  if (live === null) return <span className="dot idle" aria-label="Not running" />;
  if (live.status.state === 'attention') return <span className="warn" aria-label="Needs input">⚠</span>;
  if (live.busy || live.status.state === 'busy') return <span className="busy" aria-label="Working" />;
  if (live.status.state === 'exited') return <span className="dot off" aria-label="Exited" />;
  return <span className="dot" aria-label="Running" />;
}

function NewSessionSheet({
  projects,
  s4,
  onPick,
  onBrowse,
  onClose,
}: {
  projects: readonly api.Project[];
  s4: boolean;
  onPick: (project: api.Project) => void;
  onBrowse: () => void;
  onClose: () => void;
}): ReactNode {
  const [filter, setFilter] = useState('');
  const needle = filter.trim().toLowerCase();
  return (
    <Sheet title="New session in…" icon="+" onClose={onClose}>
      {s4 && (
        <button type="button" className="option browse-option" onClick={onBrowse}>
          <span className="grow">
            ▣ Browse desktop folders…
            <small>Any folder on the desktop, not just existing projects</small>
          </span>
          <span className="muted">›</span>
        </button>
      )}
      <input className="input" placeholder="Filter projects" value={filter} onChange={(event) => setFilter(event.target.value)} style={{ margin: '10px 0' }} />
      <div className="option-list">
        {projects
          .filter((project) => needle.length === 0 || project.display.toLowerCase().includes(needle))
          .map((project) => (
            <button key={project.id} type="button" className="option" onClick={() => onPick(project)}>
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
