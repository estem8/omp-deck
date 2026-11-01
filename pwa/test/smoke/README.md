# Smoke harness

A deterministic fake desktop for the phone app. One Bun server on `127.0.0.1` is all three of:

- **Static host.** It serves a fresh build of the app in a temp dir, never `dist/`. The served `index.html` boots through
  `boot.ts`, which seeds the pairing into IndexedDB, applies the theme and picks the gateway variant, then loads the real
  bundle.
- **Relay.** It answers the device hello on `/v1/device` and routes the connection to a desktop by the hello's room.
- **Gateways.** There are two desktops on this one relay, each with its own key, room, tokens and state: **Workstation**
  (`smoke.first`, also `smoke.invite`/`smoke.gateway`; the seeded pairing) and **Laptop** (`smoke.second`, version
  `0.11.0`). Each runs the real Noise responder (`src/remote` `respond`) and the real record codec. The application
  side, `gateway.ts`, answers every v1 and slice-4 feed and op from `fixtures.ts`. `gateway.connections` counts a
  desktop's open device connections, which is how the e2e run proves that only the active desktop is connected.

Mutations are journaled like the real gateway. They answer `accepted` at once. Long ops (backups, updates, restores,
worktree finish) then send `progress` events, and every op finishes with its outcome event. A prompt, compose or review
appends the user item and, about 1.5 s later, an assistant reply to the live transcript. `answer_ui` clears the request.
Uploads answer about 700 ms per chunk, so progress is visible.

None of this ships: `test/build.test.ts` asserts that `dist/` contains no smoke code.

## Commands (run in `pwa/`)

| Command | What it does |
| --- | --- |
| `bun test/smoke/serve.ts [--port 4780] [--minify]` | Starts the harness and prints the URL of every route plus a pairing link. Restart it to rebuild. |
| `bun test/smoke/shoot.ts --out /tmp/pwa-smoke/<agent> [--themes aurora,blueprint] [--wait 1500] [--full] TARGET…` | Builds, starts the harness, and writes `<label>-<theme>.png` at 390×844 per target and theme. |
| `bun run e2e` | Runs the Playwright end-to-end spec (`e2e/`) against the harness. |

### Shoot targets

A target has the form `[label::]route[@step|step…]`, where `route` is a name from the table below or a raw hash.

The steps are:

- `click=<css>`
- `tap=<visible text>`
- `fill=<css>=<text>`
- `files=<css>=<path>`
- `press=<key>`
- `wait=<ms>`
- `scroll=<css>`
- `top`

Example:

```sh
bun test/smoke/shoot.ts --out /tmp/pwa-smoke/PwaConvo 'ctx::conversation@tap=37%|wait=400' ui-ask
```

Console errors from the page are printed next to each file.

## URL hash

| Parameter | Meaning |
| --- | --- |
| `route=` | A tab (`sessions`, `agents`, `inbox`, `settings`) or a screen (`conversation`, `subagent`, `terminal`, `changes`, `manager`, `tree`, `browse`, `worktrees`, `running`). With `settings`, `section=` opens a settings section. `route=pair` (smoke only) starts unpaired (no pairing at all). |
| `session=`, `agent=`, `live=`, `project=`, `display=`, `title=`, `section=` | The screen's parameters. |
| `s=<session id>` | The notification deep link: opens that conversation. |
| `d=<pairing id>` | The desktop a link is for (the hex of its gateway key; `smoke.second.id`): the app switches to it first. A desktop that is not paired drops the link. |
| `pair=v1.…` | A pairing invite. The harness approves it after about 1 s. |
| `theme=` | `aurora` (default), `blueprint`, `signal`, `margin`, `tidal`, `field`. Smoke only. |
| `desktops=2` | Also seeds the Laptop's pairing; the Workstation stays active. Smoke only. |
| `legacy=1` | Seeds the Workstation as version 1 stored it (database version 1, `pairing/current`) on a fresh profile, so the app's own upgrade migrates it. Smoke only. |
| `gw=v1` | An old desktop: `Welcome.features` lacks `s4`, and s4 ops answer `unsupported`. Smoke only. |

`route` and `s` are real app deep links (`src/app/nav.ts` `routeFromHash`). `theme`, `gw` and `route=pair` are read by
`boot.ts` only.

## Routes (`routes.ts`)

| Name | Hash |
| --- | --- |
| pair | `route=pair` |
| sessions / agents / inbox / settings | `route=<tab>` |
| conversation | `route=conversation&session=s-convo` (busy: rich transcript, images, tool details, desktop control) |
| conversation-idle | `route=conversation&session=s-idle` (idle: the composer sends prompts) |
| ui-select, ui-multi, ui-confirm, ui-input, ui-editor, ui-ask, ui-approve | `route=conversation&session=s-ui-<kind>` (one pending UI request each) |
| exited | `route=conversation&session=s-exited` |
| subagent | `route=subagent&session=s-convo&agent=PwaHarness` |
| terminal | `route=terminal&live=tmux:8:omp-deck:omp-3c4d` (input is echoed) |
| changes | `route=changes&project=p-ompdeck` (the `diff` op returns a multi-hunk diff) |
| manager | `route=manager` |
| tree | `route=tree&session=s-convo` (two branches, paginated) |
| browse | `route=browse` (folder tokens are `dir:<path>`) |
| worktrees / worktrees-linked | `route=worktrees&project=p-ompdeck` / `p-ompdeck-wt` (parked branches) |
| running | `route=running` (RAM per session, one orphan mux session) |
| settings-notifications, -device, -usage, -omp, -updates, -backup, -sync | `route=settings&section=<section>` |
| deep-link | `s=s-convo` |
| old-desktop | `route=settings&section=usage&gw=v1` |

## Fixtures

Everything is in `fixtures.ts`, keyed to `now` at server start.

- **Projects.** `p-ompdeck`, `p-website`, `p-infra`, and `p-ompdeck-wt` (a linked worktree).
- **Live sessions.** Ten of them (`LIVE`): one busy, one idle, seven needing attention with one UI request each, and one
  exited.
- **Per-session data.**
  - Sessions per project, including a stub.
  - Inbox entries of every kind.
  - Cards with git counts and subagent counts.
  - Subagents of `s-convo`: running, done, failed and parked.
  - The context breakdown.
- **Project data.**
  - Changes covering every change kind, plus a conflict and an untracked directory.
  - Inventory with stats.
  - Worktrees with parked branches.
- **Desktop-wide data.**
  - Running sessions with memory.
  - The folder tree.
  - Usage from three providers, and 371 days of activity.
- **Settings data.**
  - omp settings covering every kind, including set and unset secrets.
  - Updates: omp has a newer version, and the app is a deb with apply refused.
  - Backup: the active destination (Google Drive in the fixture), two hosts with three backups each, and a settings restore plan.
    The phone also supports FTP, FTPS and SFTP destination labels and desktop setup.
  - Sync: three rows and backups for two projects on two hosts, plus a project restore plan with conflicts and a
    problem. Restore review offers **Choose sessions**; left-out ids are sent as `excluded`, independently of conflict
    choices. Restore is disabled when the plan has no writable files or included, writable sessions.
  - Device state, including push.

To change what a screen shows, edit `fixtures.ts`. `test/smoke.test.ts` validates every fixture answer against the wire
decoders.

`bun run e2e` also switches backup destination kind and label with timestamps unchanged on both
backup screens, verifies selections reset, and releases a delayed old-destination reply to check
that it cannot replace the new listing.
