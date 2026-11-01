# Mobile companion slice 4: PWA full-control client and Web Push

Status: **contract** (2026-10-30). Orchestrator-owned; implementers change it only through the orchestrator.

Scope (`mobile-companion.md` step 4, nothing dropped): mobile navigation and every coverage-matrix action
(`mobile-companion.md` lines 20-28), terminal and approval flows, the desktop-folder browser, downloads and attachments,
clear destructive confirmations, service worker and Web Push. It also covers every item of `mobile-relay.md`'s
"Deferred to slice 4" list. The protocol stays v1 (frozen at R5). Everything here is additive under the freeze rules
(`mobile-relay.md` "Protocol v1 freeze").

Recon facts used here (2026-10-30):
- upstream rendering: `collab-web` (MIT, private on npm);
- Web Push: `web-push-native` 0.5.0 (verified round trip, RFC 8291 vector);
- toolchain: bun bundler, React 19.3, xterm 6.0;
- capability map: what exists in the gateway per surface.

## User decisions

- **Visual direction:** mockups in `~/omp-deck-pwa-mockups/` (`index.html#d1…#d6`, PNGs). All six ship as selectable
  phone themes, and the default is **2 Aurora glass** (user, 2026-10-30). The mockups' palettes and structure are
  the source of truth for each theme:
  1. Blueprint mobile
  2. Aurora glass
  3. Signal / 01
  4. Margin
  5. Tidal
  6. Field console
- **Hosting:** the relay host serves the PWA (R5): one origin, `https://<relay host>/`.
- **Permissions:** Control = code execution; View + Control is the approval default (R5).

## Orchestrator decisions

- **Feature gate.** The gateway announces `Welcome.features: ["s4"]`; a slice-4 PWA announces
  `DeviceHello.features: ["s4"]`.
  - A device sends a new feed, optional request field, enum value or record kind only if `s4` is in
    `Welcome.features`.
  - The gateway sends a new enum variant, `Event` type or `Item` type only to devices that announced `s4`.
  - New ops need no gate: an older gateway answers `unsupported`.
  - New fields on gateway→device objects need no gate either.
  - All of this follows the freeze rules.
- **Vector files.**
  - `api-v1.json` stays byte-identical as the R5 baseline. Every new op, feed, variant or field has vectors in a new
    generated file `crates/deck-remote/tests/vectors/api-s4.json`, pinned in `wire_freeze.rs`.
  - Struct fields added to existing types use `#[serde(default, skip_serializing_if = …)]`, so the R5 vectors still
    encode byte-identically. This is the one exception to the "`None` written as `null`" convention; it is allowed
    only for fields added after R5 and is documented in `api.rs`.
  - Both decoders (Rust and TS) must still decode every `api-v1.json` vector unchanged.
- **No path ever crosses to a device.** The folder browser, restore targets and new sessions in a folder use opaque
  **dir tokens**: `"f-"` + 16 hex digits from a per-device gateway map, at most 4096 entries, dropped when the device's
  session ends. Displays are `~`-abbreviated strings, which are never accepted back. Running sessions without a
  session file use opaque **running ids** (`"r-"` + 16 hex digits of SHA-256 over the mux name and the gateway epoch).
- **Attachments are files on the desktop.**
  - The phone uploads bytes in chunks. The gateway writes them, owner-only, to
    `<data dir>/omp-deck/attachments/<device id>/<sha256-16hex>-<sanitized name>`.
  - It then injects one bracketed paste of the quoted path, then the text, then submits. This mirrors desktop drag
    and drop (`deck-term` `encode_dropped_paths`). omp attaches an image path as an image and keeps the original path,
    so the files are durable, not temp files.
  - Limits: 20 MiB per file and 200 MiB per device. When over quota, the oldest files more than 30 days old are
    removed first, otherwise the upload is refused `too_large`. Revoking a device deletes its directory.
- **Steering.**
  - `compose` mode `steer` pastes the text and sends Enter while omp is busy (omp: Enter while streaming = steer).
  - Mode `follow_up` pastes and sends Ctrl+Q, omp's queued follow-up (`app-keybindings.ts:159-164`).
  - Both require `Busy`, and an empty text is refused: an empty Enter while busy aborts the turn
    (`docs/notes/omp-actions.md:139`).
  - This is a new `inject` job, not a ctl migration.
- **Downloads are pull-based JSON.**
  - `export`/`blob` answer a `download` handle. The device pulls `download { download, offset }`, receiving chunks of
    at most 512 KiB raw bytes as base64 in one reply, which stays below `MAX_REPLY` and inside the ack window.
  - Handles are bound to the device, expire 10 minutes after their last use, and at most 8 are open per device.
  - There is no new record kind.
- **Uploads use the same scheme in reverse:** `upload_begin` → `upload_chunk { offset, data }` (base64, ≤ 512 KiB raw)
  → the upload id is used in `compose`. Uploads are bound to the device, expire after 10 minutes idle, and at most 4 are
  open per device.
- **Desktop-only logic moves into shared crates**, and the desktop cuts over in the same slice (clean cutover, no
  second implementation):
  - the worktree finish state machine, the park/continue/switch-back flows and the tab classification, from
    `omp-deck/src/app/worktree.rs` into the gateway engine;
  - the review prompt builder (`omp-deck/src/review.rs` `build_prompt`, `MAX_PROMPT_BYTES`) into `deck-service`;
  - the project-restore stop list (`app/project_sync.rs` `session_kill_names`) into the gateway, which derives it
    from its holders;
  - the fork-route check (refuse a fork of a session running *without* control, `app/tree.rs:95-101`) into the
    `ForkAt` mutation;
  - the worktree destination and base recording (`worktree_dialog.rs`, `app/worktree.rs:343-350`) into the gateway.

  The desktop calls the gateway for each of these afterwards.
- **"Open on desktop"** is explicit and named so. It targets the desktop GUI through a new `ServerEvent` (open/focus a
  session's tab, optionally in a split; close a tab). With no GUI connected, the gateway starts the app with
  `--activate` as it already does for notifications. Phone navigation never moves desktop focus by itself.
- **Backups follow the active destination** (main 0.20.0: Google Drive, FTP, FTPS or SFTP). Drive sign-in cannot
  finish on a phone (loopback OAuth), so for Drive the phone offers **"Sign in on the desktop"** (`backup_connect`):
  the gateway opens the URL in the desktop browser (through the GUI if connected, otherwise `--activate`), and the
  phone watches the `backup` feed. With no desktop session it is refused with `rejected { reason: "needs desktop" }`.
  `backup_connect` is refused for server destinations: the phone tells the user to set the server up in the desktop's
  Settings → Backup.
- **OmpDeck self-update from the phone:** check and download are allowed. Apply is allowed only for in-place kinds
  (tarball/install.sh, AppImage, Windows dir). The pkexec kinds (deb/rpm) answer `rejected { reason: "at desktop" }`
  because the polkit prompt appears on the desktop. After apply, the gateway is replaced and the phone reconnects
  through the epoch change. Running omp sessions keep their old version until restarted (as on the desktop).
- **A device never edits** its permissions, the relay, the gateway mode or other devices. It may rename itself and
  unpair itself.
- **Restore handles** (Drive and project) become bound to the client that prepared them; this is an R5-style fix that
  also applies to local clients. Absolute paths in restore plans (`safety_dir`, targets) are projected to `~` displays.

## Wire additions (`crates/deck-remote/src/api.rs`, mirrored in `pwa/src/remote/api.ts`)

Types not defined here are v1 types. `Id`s are strings. The class determines the permission. "J" = journaled (needs
`seq`, answers `accepted`, then `Event::Outcome`).

### New ops

| Op | Fields | Class | J | Answer |
|---|---|---|---|---|
| `last_reply` | `session` | View | | `text { text, truncated }` (≤ 256 KiB) |
| `export` | `session` | View | | `download { download, name, mime, size }` (omp `--export` HTML into OmpDeck's exports dir) |
| `blob` | `session`, `agent?: String`, `blob: String` | View | | `download {…}` (an image referenced by an `ImageRef`) |
| `download` | `download`, `offset: u64` | View | | `chunk { data: base64, eof: bool }` |
| `tree` | `session`, `filter: TreeFilter`, `cursor?: String`, `limit: u32` (1..=2000) | View | | `tree { nodes, active_leaf?, next_cursor?, truncated }` |
| `browse` | `dir?: DirToken` (absent = roots) | View | | `dir { dir?, display, parent?, entries, truncated }` |
| `usage_activity` | `days: u32` (1..=371) | View | | `activity { days: Vec<DayCount> }` |
| `usage_refresh` | — | View | | `unit` |
| `omp_settings` | — | View | | `omp_settings { groups, truncated }` (secrets: `set: bool` only, never a value) |
| `backup_list` | — | View | | `backup_list { hosts, truncated }` (versions on the active destination) |
| `sync_backups` | `project?` | View | | `sync_backups { backups, truncated }` |
| `presence` | `visible: bool` | View | | `unit` (push is skipped while the device reports visible and connected) |
| `push_subscribe` | `endpoint`, `p256dh`, `auth` (base64url) | View | | `unit` |
| `push_unsubscribe` | — | View | | `unit` |
| `push_prefs` | `prefs: PushPrefs` | View | | `unit` |
| `device_rename` | `name` (display-safe, as at pairing) | View | | `unit` |
| `unpair` | — | View | | `unit`, then the gateway revokes this device (like a desktop revoke) |
| `upload_begin` | `name`, `mime`, `size: u64` | Control | | `upload { upload }` |
| `upload_chunk` | `upload`, `offset: u64`, `data: base64` | Control | | `unit` |
| `upload_cancel` | `upload` | Control | | `unit` |
| `compose` | `live`, `text`, `attachments: Vec<String>`, `mode: ComposeMode` | Control | J | `submitted {…}` |
| `review` | `live`, `comments: Vec<ReviewComment>`, `general?: String`, `send: bool` | Control | J | `submitted {…}` |
| `answer_ui` | `live`, `request: String`, `value: UiAnswer` | Control | J | `answered { applied }` (see "Structured UI requests") |
| `agent_kill` / `agent_revive` | `live`, `agent` | Control | J | `unit` |
| `agent_chat` | `live`, `agent`, `text` | Control | J | `unit` |
| `fork_at` | `session`, `entry` | Control | J | `file { session }` (refused while the session runs without control) |
| `navigate` | `live`, `entry` | Control | J | `unit` (idle only) |
| `new_session_in` | `dir: DirToken` | Control | J | `session { live }` |
| `running_kill` | `ids: Vec<String>` (running ids) | Control | J | `killed { results }` |
| `worktree_create` | `project`, `branch`, `base?: String`, `open: bool` | Control | J | `session { live }` when `open`, else `unit` |
| `worktree_move` | `live`, `branch` | Control | J | `submitted {…}` |
| `worktree_finish` | `project` (the worktree's project) | Control | J | `unit` (progress events; the flow runs on the gateway) |
| `worktree_continue` | `parked: String`, `where: ContinueWhere` | Control | J | `session { live }` |
| `update_check` | — | Control | | `unit` |
| `backup_run` | — | Control | J | `unit` |
| `sync_backup` | `project` | Control | J | `unit` |
| `open_on_desktop` | `session`, `split: Split` | Control | J | `unit` |
| `close_on_desktop` | `session` | Control | J | `unit` |
| `worktree_remove` | `project`, `park: bool` | Manage | J | `unit` |
| `omp_set` | `key`, `value: serde_json::Value` | Manage | J | `unit` |
| `omp_reset` | `key` | Manage | J | `unit` |
| `update_run` | `kind: UpdateKind` | Manage | J | `session { live }` (runs in a session the phone can attach to) |
| `app_update` | `step: AppUpdateStep` | Manage | J | `unit` |
| `backup_connect` | — | Manage | J | `unit` (Drive only) |
| `backup_delete` | `ids: Vec<String>` | Manage | J | `unit` |
| `backup_restore_prepare` | `id` | Manage | J | `restore_plan { handle, plan }` |
| `backup_restore_apply` / `backup_restore_discard` | `handle: u64` | Manage | J | `unit` |
| `sync_add` | `dir: DirToken` | Manage | J | `unit` |
| `sync_set` | `project`, `parts: SyncParts`, `excluded: Vec<String>` (session ids) | Manage | J | `unit` |
| `sync_remove` | `project` | Manage | J | `unit` |
| `sync_delete` | `ids: Vec<String>` | Manage | J | `unit` |
| `sync_restore_prepare` | `backup`, `target: RestoreTarget`, `parts: SyncParts` | Manage | J | `restore_plan { handle, plan }` |
| `sync_restore_apply` | `handle`, `conflicts: Vec<ConflictChoice>`, `excluded?: Vec<String>` (session ids left out, main 0.19.1) | Manage | J | `unit` |

### New feeds (gate `s4`; latest value only, bounded with `truncated`)

| Feed | Key | Value |
|---|---|---|
| `running` | — | `{ sessions: Vec<RunningView>, server_bytes?: u64, truncated }` |
| `inventory` | `project?: String`, `limit` (1..=500) | `{ project?, total, entries: Vec<InventoryView>, truncated }` (stubs included; sizes, messages, tokens, cost, model, subagents, created, last activity, running) |
| `context` | `live` | `{ live, context?: ContextView }` (only with live control) |
| `usage` | — | `{ usage?: UsageView, fetched_unix_ms?, error? }` (provider quota cards, tightest remaining) |
| `agents` | `session` | `{ session, agents: Vec<AgentView>, truncated }` (subagent list: id, label, status incl. parked, model, tokens, cost, parent) |
| `worktrees` | `project` | `{ project, worktree?: WorktreeView, parked: Vec<ParkedView> }` |
| `backup` | — | `{ destination: { kind: drive\|ftp\|ftps\|sftp, label }, available, connected, account?, last_backup_unix_ms?, interval, busy?, error? }` (never secrets; `account` = Google email, null for servers) |
| `sync` | — | `{ rows: Vec<SyncRowView>, interval, busy?, truncated }` |
| `updates` | — | `{ omp: OmpUpdateView, app: AppUpdateView }` |
| `device` | — | `{ name, permissions, push: PushStateView, gateway_version, mode }` |

### Additive fields (no gate)

- `Welcome.push_key: String`: the VAPID public key (base64url of the 65-byte uncompressed P-256 point).
- `Item::ToolCall.args_value?: serde_json::Value`: structured arguments. Each string field is cut to 16 KiB with
  `CUT_MARK`, and the whole value is at most 64 KiB, else absent.
- `Item::ToolResult.details?: serde_json::Value`: at most 32 KiB, else absent.
- `Item::ToolResult.images` and `Item::User.images`: `Vec<ImageRef { blob, mime, bytes }>`, fetched with `blob`.
- `SessionSummary.stub: bool`.
- `LiveSession.pending_ui: Vec<UiRequestView>` (see below).

### New types (sketch; exact Rust in `api.rs`)

- `ComposeMode` = `send` (idle) | `steer` (busy) | `follow_up` (busy) | `insert` (any state, no submit).
- `ReviewComment { file, side: DiffSide, start: u32, end: u32, text }` (repo-relative file; the gateway checks it
  against the `changes` listing like other changed-file ops).
- `TreeFilter` = `conversation` | `default` | `all` | `labeled`.
- `DirToken` = `String` (`f-…`).
- `DirEntry { name, dir: DirToken, project?: String, repo: bool }`.
- `Split` = `none` | `right` | `down`.
- `ContinueWhere` = `worktree` | `main` | `switch_back`.
- `UpdateKind` = `omp` | `plugins` | `stable` | `canary`.
- `AppUpdateStep` = `download` | `apply`.
- `SyncParts { omp: bool, sessions: bool, memory: bool }`.
- `RestoreTarget` = `{ project }` | `{ dir: DirToken }` (tag `type`).
- `ConflictChoice { session, choice: keep | overwrite | both }`.
- `PushPrefs { turn_finished, attention, ended, show_titles }` (defaults true, true, true, false).
- `DoneView` gains `text { text }`, `killed { results }` and `restore_plan { handle, plan: RestorePlanView }`; these are
  sent only to `s4` devices. `Resp` gains `text`, `download`, `chunk`, `tree`, `dir`, `activity`, `omp_settings`,
  `backup_list`, `sync_backups`, `upload`.

### As built (P1)

`crates/deck-remote/src/api.rs` is authoritative for every field set. Vectors: `tests/vectors/api-s4.json` (146
messages, 12 refused requests; `"features":["s4"]`), pinned in `wire_freeze.rs`. `api-v1.json` regenerates
byte-identically (test). Deltas from the sketch above:
- `usage` and `worktrees` values carry `truncated`, `usage` also carries `fetching`, `backup` carries `available`, and
  `DoneView::RestorePlan` carries `truncated`.
- `RestorePlanView` is tagged: `backup{…, safety_display}` | `project{target_display, …, sessions}`.
- Merge with main 0.20.0: the s4 Drive surface was renamed destination-neutral before release (no aliases). Backup and
  project listings remember the destination they came from; their ids resolve only while it is still active
  (`not_found("backup")` after a switch), and the phone's sync state resets on a switch.
- `UiRequestView` is one flat struct keyed by `kind: UiKind`; `initial`/`recommended` are option indices; a grant's
  `scope` is a free string.
- `AppUpdateView.apply_allowed` (false for pkexec kinds); `needs_manual` carries no install command (it would hold
  paths).
- `SyncInterval` = off | ten_minutes | thirty_minutes | hourly | six_hours | daily.
- Bounds are enforced by `Op::check()` (tree limit, activity days, upload chunk length ≤ `MAX_TRANSFER_CHUNK_B64`),
  answering `bad_request`; `Request::parse` stays bounds-free. TS `checkRequest` mirrors them.
- Upload and download handles are strings; Drive and sync restore handles are `u64`.
- Free-form JSON values (`omp_set.value`, `args_value`, `details`) are written with code-point-sorted keys in both
  languages. Integer-like keys order differently in JS; only the bytes differ, the value read is the same.
- `Op::journaled()` is false for `upload_*` and `update_check`.

Ctl side (`deck_core::ctl`, `ompdeck-ctl.ts`): hello `ui_hook`, snapshot `pendingUi`, `CtlCommand::AnswerUi`. For
`input`, the placeholder goes in `message`. `ask` is titled "Ask", `approve` is titled "Approval". The extension cuts
strings at 16 KiB and options at 100; the gateway applies the device caps (4 KiB / 50).

## Structured UI requests (verified, omp 18.14.0)

Facts (`agent://AskHook`, proven live on omp 18.14.0):
- Every `ask`, tool approval, `select`, `confirm`, `input` and `editor` request goes through one shared
  `ExtensionUIContext` (`extension-ui-controller.ts:150-189`).
- An extension's `ctx.ui` is a set-through Proxy, so the ctl extension wraps those members in place once in
  `session_start`. The wrapping is idempotent, guarded by `Symbol.for("ompdeck.wrapped")`.
- Each call registers a pending request and merges an `AbortController` into `opts.signal`. It then races the original
  (TUI) call against a remote answer. The first answer wins; aborting the loser hides the TUI dialog, the same pattern
  as collab's `#raceCollabDialog`. Cancel resolves `undefined`.
- This is internal omp API, so a guard is required. If any wrapped member is not a function, the extension skips that
  member and reports `ui_hook: false` in its hello. The gateway then never lists pending requests for that session,
  and the terminal path still works.

Ctl protocol (`deck_core::ctl` + `ompdeck-ctl.ts`, additive):
- The snapshot gains `pendingUi: [...]`; `LiveSnapshot.pending_ui: Vec<PendingUi>` (lenient, so unknown kinds are dropped).
- `CtlCommand::AnswerUi { id: u64, value: serde_json::Value, root }` carries the method's own result shape:
  `string|null` for select/input/editor, `bool` for confirm, the result object for `askDialog`/`approve`, and `null`
  for cancel. The root check is the same as for `kill`/`chat`. A stale id replies `no such request`.

Device wire (`api`):
- `LiveSession.pending_ui: Vec<UiRequestView>`, at most 8 entries. Each string is cut to 4 KiB and each option list to
  50 entries.
  - Common fields: `request` (`"u-"` + the decimal ctl id), `kind` = `select | confirm | input | editor | ask | approve`,
    `title`, `message?`, `timeout_ms?`.
  - `select`: `options: Vec<UiOption { label, description? }>`, `multi`, `initial?`.
  - `input`/`editor`: `prefill?`.
  - `ask`: `questions: Vec<UiQuestion { id, question, header?, options, multi, recommended? }>`.
  - `approve`: `approve: ApproveView { full, compact, session_label?, grants: Vec<{ scope, label, always_label }> }`.
- `answer_ui { live, request, value: UiAnswer }` (Control, journaled). `UiAnswer` is tagged by `type`:
  - `choice { index }`
  - `choices { indices }`
  - `text { text }`
  - `confirm { yes }`
  - `ask { answers: Vec<{ id, selected: Vec<u32>, custom? }> }`
  - `approve { decision: once | session | deny | grant { index, always } }`
  - `cancel {}`

  The gateway validates the answer against the pending request's kind and bounds, then translates it to the ctl value.
  For `ask` it builds the full `{kind:"submit", results:[…]}`, with labels taken from the stored request. It answers
  `answered { applied: false }` when the request is gone (the terminal won).
- Subagent prompts: whether they reach the main UI context is not verified yet. The P4 gate checks it; if they don't,
  they are answered in the subagent's own session terminal or shown as attention.

The terminal path (attach `input`/`control`) keeps working for every kind.

## Web Push (gateway, `deck-service`)

- **Stack:** `web-push-native` 0.5.0 for RFC 8291 encryption only (no `vapid` feature: it pulls `jwt-simple` → `rsa`,
  RUSTSEC-2023-0071), plus the existing blocking `ureq` 3 with `http_status_as_error(false)`.
- **VAPID key:** a P-256 key generated once with `ring` (constant-time ECDSA, already in the tree through rustls),
  stored owner-only as `push.key` (PKCS#8) in the gateway secrets dir next to `remote.key`. The gateway signs the
  ES256 JWT itself (about 30 lines; a test verifies the signature). The public key goes into `Welcome.push_key`. A
  lost key invalidates all subscriptions; devices resubscribe when `push_key` changes.
- **Accepted advisories:** RUSTSEC-2026-0387 (crypto-bigint 0.5 timing variability). It reaches us through p256 0.13 /
  elliptic-curve 0.13, which web-push-native 0.5 hard-requires; the fix is only in crypto-bigint 0.7.6. p256 only
  touches per-message ephemeral ECDH keys and the subscriber's public points. The only long-term secret, the VAPID
  key, never goes through it (ring). Recorded in `deny.toml`.
- **Subscriptions:**
  - `Device.push: Option<PushSub { endpoint, p256dh, auth, prefs, since_unix_ms }>` in `devices.json` (`#[serde(default)]`).
  - Deleted on revoke, on `unpair` and on a 404/410 response. Prefs live inside the subscription: `push_prefs` before
    `push_subscribe` fails (`not subscribed`); re-subscribing keeps the existing prefs.
  - Parsed by hand: `endpoint` as an `https` `Uri`, `p256dh` as a SEC1 point, `auth` exactly 16 bytes. Never with the
    crate's serde, which panics on a bad `auth` length.
- **SSRF:** the endpoint host is resolved through a ureq resolver that drops loopback, private, link-local, CGNAT,
  multicast, unspecified and documentation addresses (v4, v6 and v4-mapped). An endpoint resolving to none of the
  allowed addresses is refused at subscribe time and skipped at send time. Only `https` is accepted; there is a test
  override for a loopback receiver (env `OMP_DECK_PUSH_TEST_LOOPBACK=1`, verification only; allows `http://` and
  loopback). Redirects and proxies are disabled on the push agent.
- **Hook:**
  - `NotificationCenter::record` emits a new unconditional `Effect::Mobile { target, kind, title }` for every canonical
    event (turn finished, attention, ended). It is not filtered by the desktop policy (`desktop_wanted`).
  - `apply_effects` fans it out per device by that device's prefs. The shared per-kind notification toggles and
    `while_viewing` still apply to phone pushes; only the desktop pop-up policy is bypassed.
  - A device is skipped while it reports `presence { visible: true }` on a live connection.
  - omp still runs with `PI_NOTIFICATIONS=off`; there is one source and no duplicates.
- **Payload:**
  - Declarative Web Push JSON (WebKit, iOS 18.4+; the service worker handles the same JSON elsewhere):
    `{"web_push":8030,"notification":{"title":"OmpDeck","body":…,"navigate":"<pwa origin>/#s=<session id>","tag":…,"silent":false}}`.
  - A live session whose file is not in the gateway's latest scan yet (a session started seconds ago) has no session
    id: `navigate` is `<pwa origin>/#route=conversation&live=<percent-encoded live key>`, and the conversation screen
    follows that live session to its id. Never the Running view's opaque `r-…` id, which `#s=` would look up as an omp
    session and answer "Not found: session." (found on a real phone, 0.22.1).
  - Privacy-preserving body by default: "A session finished", "A session needs your attention", "A session ended". With
    `show_titles`, the title is included, cut to 80 characters.
  - The payload is capped at 1024 bytes (the hard limit is 3993).
- **Headers and timing:**
  - `TTL` 3600.
  - `Urgency`: `high` for attention, `normal` otherwise.
  - `Topic` = 22 characters of base64url of SHA-256(session id, else live key), so pending pushes coalesce per session.
- **Delivery:**
  - One worker thread with a bounded queue (256 entries); overflow drops the oldest.
  - 429 honours `Retry-After` (≤ 10 min); 5xx retries with backoff, up to 3 attempts; 400/401/403/413 are logged and
    not retried.
  - The VAPID JWT is cached per push-service origin for 12 h (Apple: at most one refresh per hour).
  - `sub` = `OMP_DECK_PUSH_CONTACT` (an e-mail, `mailto:` or `https:` URL), default the PWA origin. An event without a
    managed live session navigates to `<origin>/` and takes its topic from the session key.
- **Tests:**
  - the RFC 8291 appendix A vector through `decrypt`;
  - a loopback receiver round trip (encrypt → POST → decrypt);
  - SSRF refusal;
  - 410 deletes the subscription;
  - prefs and presence gating;
  - no push for an event the canonical tracker de-duplicated.

## Web Push per desktop (PWA)

The phone keeps one pairing per desktop (see "Keys" and "Several desktops"), and every gateway has its own VAPID key.
A browser allows one `PushSubscription` per service-worker registration, bound to one `applicationServerKey`, so:

- **One registration per desktop.** Desktop `<id>`'s subscription lives on the registration of the same `/sw.js`
  script at scope `/d/<id>/` (`desktopScope`, `src/push.ts`; the script is at `/`, so the narrower scope needs no
  `Service-Worker-Allowed` header). The root `/` registration (`app/update.ts`) stays the
  app-shell/update worker and holds no subscription any more.
- **Active desktop only in the UI.** `usePushSync` and `PushController` act on the active desktop's registration:
  enabling push for one desktop never touches another. The registration is created when push is turned on (or
  migrated, below); `sync` never creates one. The app re-sends `push_subscribe` on every reconnect, as before.
- **The desktop replaces its old subscription.** `push_subscribe` from a device overwrites that device's stored
  subscription (`push::subscribe` sets `d.push = Some(sub)`, keeping its prefs; `since_unix_ms` is kept when the
  endpoint is unchanged). Device ids are per desktop, so desktops never overwrite each other's.
- **The kept VAPID key is per registration.** The Cache `push-state` entry is `/__push-key?scope=<registration scope>`
  (`pushKeyUrl`), so `pushsubscriptionchange` in a desktop's worker resubscribes with that desktop's key. The old
  single `/__push-key` entry is read only by the root worker, until the migration (`legacyDone`) deletes it.
- **A tap switches desktops.** The worker's `navigate` is `/#d=<id>&…` (`navigateForDesktop`: the id first, the
  gateway's own hash parameters kept). The click posts it to an open window or opens it cold; the app reads the desktop
  first (`linkFromHash`), switches to it, then opens the route. At a cold start the link's desktop replaces the stored
  active one. Tags are `<id>:<gateway tag>`, so two desktops' notifications for the same session never replace each
  other.
- **A notification names its desktop once several are paired.** With 2+ desktops each notification's title is
  `OmpDeck — <label>` (`notificationFor`/`titleWithLabel`), where the label is what the switcher shows (the
  gateway's label, with the key-fingerprint head appended when labels collide, `summaries()`) minus a leading
  "OmpDeck on " (the gateway's default label; the title already says OmpDeck). The title, not a body
  line: Android shows it bold on its own line in the shade, and the body stays the gateway's text. With one desktop
  the notification is unchanged (`OmpDeck`). The worker has no names and cannot cheaply read IndexedDB, so the app
  keeps the label in the Cache `push-state` entry `/__push-label?scope=<registration scope>` (`pushLabelUrl`),
  written by `syncPushLabels` (`watchPushLabels` in `main.tsx` re-runs it whenever the list or a label changes:
  pairing added or replaced, forgotten, a collision appearing or going away). It deletes the entries while only one
  desktop remains, and forgetting a desktop deletes its entry with its key. A missing or unreadable entry means an
  unlabelled title. The root registration never carries one. Migration from version 1 needs nothing (one desktop).
- **Migration from version 1.** The v1 subscription is on the root registration. The keystore migration marks the
  migrated desktop (`meta['legacy-push']`). When that desktop's app syncs with notifications permitted, it registers
  `/d/<id>/`, subscribes there with the desktop's `push_key`, re-sends `push_subscribe` (how a changed endpoint
  reaches the desktop), then unsubscribes the root subscription and clears the marker. Subscribing comes first: if it
  fails, the root subscription is kept and delivery continues. Turning push on or off also retires the root one.
- **Forgetting a desktop** unsubscribes and unregisters its scope registration and deletes its kept key (plus the
  root subscription and the old key entry when it was the migrated one). Other desktops' registrations are untouched;
  every step runs, and the first failure is reported.
- **Not covered:** a phone that is paired with several desktops receives a push from each, but only the active
  desktop's app sees its live state; the tap switches to the pushing desktop.

## Relay serves the PWA (`deck-relay`)

- **`--web-dir <dir>`:** if absent, the relay uses `<exe dir>/web`, then `/usr/share/omp-deck-relay/web`, else it
  serves no app and logs that once at startup.
  - Files are read into memory at startup (total at most 64 MiB; precompressed `.br`/`.gz` siblings are served when
    `Accept-Encoding` allows) and are re-read on restart only.
  - `/` = `index.html`. There is no SPA fallback, `/v1/*` never serves files, and unknown paths answer 404.
  - Static routes coexist with `/v1/gateway`, `/v1/device` and `/healthz`; the explicit routes win.
- **Headers on every static response:**
  - `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self'
    data: blob:; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none';
    form-action 'none'; frame-ancestors 'none'`.
    - `style-src 'unsafe-inline'` is required by xterm 6, which injects style elements. This is accepted:
      `script-src` stays strict.
    - The relay's own `wss://` origin is covered by `'self'` (CSP 3). An integration test proves the WebSocket
      connects under this CSP in Chromium.
  - `Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`, `Cross-Origin-Opener-Policy: same-origin`,
    `Cross-Origin-Resource-Policy: same-origin`, `X-Frame-Options: DENY`.
  - `Permissions-Policy: camera=(), microphone=(), geolocation=(), payment=(), usb=()`.
  - `Strict-Transport-Security: max-age=31536000` under `--tls-cert`.
- **Cache-Control (as built):** everything under `assets/` (the hashed build output) gets
  `public, max-age=31536000, immutable`; every other file (`index.html`, `sw.js`, `manifest.webmanifest`, icons) gets
  `no-cache`. Every body carries a content-hash `ETag`, and `If-None-Match` answers 304. Dotfiles are never served.
  Other methods on a file answer 405 with `Allow: GET, HEAD`. Content types are set explicitly: `.webmanifest` →
  `application/manifest+json`, `.js` → `text/javascript`, `.css`, `.svg`, `.png`, `.woff2`, `.ico`, `.json`, `.txt`;
  anything else is `application/octet-stream`.
- **Transport fix.** The 10 s whole-connection `Guarded` deadline (`transport.rs:134-175`) cut keep-alive. It now
  applies per request:
  - request headers must complete within 10 s of the connection starting, or of the first byte of the next request on a
    kept-alive connection;
  - idle keep-alive at most 30 s (outgoing response bytes count as activity);
  - `Config::request_timeout` became `header_timeout`, plus `keep_alive_timeout`, `write_stall_timeout` and
    `max_requests_per_connection` (the 100th response carries `Connection: close`). axum 0.8's `serve` exposes no
    hyper timer, so this is implemented by a router middleware and a body wrapper.
  - a response write that makes no progress for 30 s closes the connection;
  - at most 100 requests per connection.

  Slowloris protection is kept and WebSocket upgrades are unchanged.
- **Packaging:**
  - The release builds the PWA (`bun run build` in `pwa/`) and ships `dist/` as `web/`: in the relay tarball next to
    the binary, at `/usr/share/omp-deck-relay/web` in the image (the Containerfile gets a bun build stage), and as an
    `--web-dir` example in the unit file.
  - CI builds the PWA in the `audit`/`check` job and runs its tests.
  - `docs/relay.md` documents all of this.

## PWA (`pwa/`)

- **Stack:**
  - React 19.3 + TypeScript 7, bundled by bun with no Vite. `build.ts` runs two builds: the app from `index.html` with
    hashed names into `dist/assets/`, and `src/sw.ts` → `dist/sw.js`, unhashed, with the precache list injected.
  - `public/` holds `manifest.webmanifest` (`id: "/"`, `scope: "/"`, `start_url: "/?source=pwa"`, `display:
    standalone`, theme colours, 192/512 `any` + 512 `maskable` icons) and a 180 px `apple-touch-icon`.
  - Separate tsconfigs for the DOM and the WebWorker.
- **Vendored upstream rendering:** `src/vendor/omp-collab/` holds:
  - collab-web's `tool-render/` (all renderers + CSS);
  - `components/transcript/{Markdown.tsx, math.ts}`;
  - `lib/format.ts`;
  - pi-utils' `marked/*`, `marked-list.ts` and `math-delimiters.ts`;
  - a trimmed wire-types file.

  Each file carries an origin header with the upstream commit. `pwa/THIRD_PARTY_LICENSES` carries the MIT notice
  with both copyright lines. An adapter maps `api::Item` → wire entries: it groups an assistant turn, pairs tool
  calls and results, and uses `args_value`/`details`/images. It falls back to `summary`/text when they are absent
  (an older gateway).
- **Safety:** no `dangerouslySetInnerHTML` except the vendored Markdown's escaped output and highlighter output; tool
  output and diffs render as text nodes. KaTeX loads lazily. highlight.js is the core build with about 12 languages,
  loaded lazily.
- **State:**
  - One client object owns the transport (`src/remote`), the session, a reconnect with backoff (4029 retried up to
    about 75 s; a close code never deletes the pairing), outcome recovery (open requests are re-read after a
    reconnect and `resync`), feeds and views.
  - A small external store (`useSyncExternalStore`) holds it.
  - Offline: the last known state shows as stale and every mutation is disabled. Nothing is queued for later.
- **Keys:**
  - IndexedDB `ompdeck` (version 2) holds one record per paired desktop in the `pairings` store, keyed by the pairing id,
    plus a `meta` store with the active id (`active`) and the migration marker (`legacy-push`, below). Each record has
    the static secret, the gateway key, the relay URL, the room/token material and that pairing's own `last`
    timestamp, persisted atomically with the key.
  - **Pairing id** = lowercase hex of the gateway's static key from the invite. Pairing again with the same desktop
    replaces that one record; a different key adds one. All desktops share one relay URL; each has its own room,
    token, gateway key and VAPID key.
  - `last` is kept per pairing and persisted before use (a lost `last` locks the device out, `mobile-relay.md` R5).
  - The `Client` still talks to a `PairingStore` bound to one pairing (`KeyStore.scoped(id)`: load, save, reserve a
    timestamp, clear), so the Noise, relay and `Client` logic is unchanged. A scoped store refuses to save another
    desktop's key.
  - **An upgrade blocked by an old window.** Version 1 keeps its connection for the page's whole life and has no
    `versionchange` handler, so while an old window is open (a browser tab *and* the installed app, say) the version 2
    open request is blocked, and boot would sit on a blank screen. Therefore: `boot()` registers the worker first, then
    `KeyStore.open(indexedDB, hooks)`. On `onblocked` it shows "Finishing the update…" (non-fatal; the request stays
    pending and completes when the others close) and posts `RELOAD_OTHERS` to the worker. The root worker then
    `navigate`s every *controlled* window except the sender to its own URL, so the old windows reload into the new code
    and let go (a window it cannot navigate is left alone; the screen tells the user to close it). Once open, the new
    code's `db.onversionchange` closes its connection and reloads the page, so it never blocks a later upgrade. Covered
    by two e2e scenarios (old window reloaded by the worker; old window the worker cannot reload) with the
    `test/smoke/holder.ts` stand-in for the old app. Not covered: a blocking window that is uncontrolled *and* never
    closed (the screen stays until the user closes it), and a page on new code still served by an old active worker
    (it ignores `RELOAD_OTHERS`; the screen asks the user to close the other windows).
  - **Migration from version 1:** the old `pairing/current` record moves into `pairings` in the `onupgradeneeded` of
    the version bump, becomes active and carries its `last` over; no re-pairing. The old store is deleted. A damaged or
    foreign record counts as "not paired" for its slot (a damaged v1 record leaves the app unpaired; a damaged v2
    record is skipped).
  - A fresh static key is generated for every pairing (R5 A4).
  - `navigator.storage.persist()` is requested after pairing.
  - A missing key means "re-pair" (for that desktop).
  - The app shows "Add to Home Screen to keep your pairing and get notifications" when not standalone.
- **Several desktops** (`src/app/desktops.ts`, `Desktops`):
  - **One live connection: the active desktop.** Switching stops the current `Client` (its link closes with a normal
    close) and starts one for the chosen pairing. The whole app remounts by pairing id, so screens, inbox, drafts and
    navigation reset for the new desktop. Inactive desktops are never connected; they reach the phone through Web Push
    only. The active id is remembered across launches.
  - **Switcher:** the active desktop's name is a chip in the header row of Sessions, Agents, Inbox and Settings
    (next to the connection status); it opens a sheet listing every paired desktop (gateway name, this phone's name
    there, the active one marked) and "Pair another desktop". Pushed screens (a conversation, a diff) show no chip;
    Back reaches it. Settings has a "Desktops" card with the same list.
  - **Same labels:** two desktops whose gateways report the same label (two gateways on one host, e.g. WSL
    instances) get ` · <first 4 characters of the key fingerprint>` appended, only while the labels collide.
  - **Pairing a desktop** runs on its own client scoped to the invite's id, so the active desktop stays connected
    until the new pairing is stored; on success the new desktop becomes active. With no pairings the screen reads as
    before; with some, an invite for a new desktop says "Pair another desktop" (the others stay paired), and an
    invite for an already paired desktop says it replaces this phone's pairing with it.
  - **Forget/unpair** (Settings → This device) applies to the active desktop only (its keys, its push subscription and
    its service-worker registration). The next remaining desktop becomes active, or the pairing screen shows when none
    is left.
- **Pairing:** read `#pair=`, then `history.replaceState` to remove it at once; an invite opened while the app runs
  (`hashchange`) goes to the pairing flow without dropping any desktop. Show the fingerprint to compare with
  the desktop, retry a pairing admission briefly on `UNAUTHORIZED` (`mobile-relay.md:115`), then reconnect.
- **Navigation** (bottom nav as in the mockups): Sessions, Agents, Inbox, Settings. The screens cover every
  coverage-matrix row:
  - **Sessions** and **Session manager** (inventory, filters, multi-select, bulk delete/copy/move/stop, the running
    view with RAM and kill).
  - **Conversation:**
    - transcript, tool cards, composer with attachments and send/steer/follow-up/insert, interrupt;
    - approval cards (draft confirm and UI requests);
    - model and effort picker; runtime actions (handoff, compact variants, rename/auto-name, plan, retry);
    - context meter and breakdown; copy last response; export HTML download.
  - **Terminal:** xterm 6 as a renderer only (DOM renderer on phones, WebGL only on desktop browsers); our own input
    field, touch scroll and selection; a key bar (Esc, Tab, Ctrl, arrows, Enter, Ctrl+C); Observe/Input/Control modes;
    `geometry_owned` handling; no terminal query replies and OSC 52 ignored (R4).
  - **Agents:** dashboard cards with a distinct subagent accordion, subagent transcript, kill/revive/chat.
  - **Session tree:** filters, search, fork-at, go here.
  - **Projects and git:**
    - desktop-folder browser; new session in a folder;
    - worktree create/move/finish/remove/park/continue;
    - changes and diffs, stage/unstage/discard/delete-untracked;
    - review comments (line, range, general) → insert/send.
  - **Backup and settings:** Drive config backup (sign in on desktop, back up, versions, restore with plan preview,
    delete); project sync (rows, parts, session opt-out, back up, versions, restore with per-session conflict choice,
    delete); omp settings editor (searchable, typed editors, reset, secrets set/clear only); update checks and update
    jobs (omp, plugins, channel, OmpDeck).
  - **Workspace:** open on desktop (with split) and close on desktop, inbox, push preferences, device self-settings
    (rename, unpair), theme picker (6 themes, Aurora glass default), connection/gateway status.
- **Destructive confirmations:** a bottom sheet with a styled header and a body naming exactly what goes (counts,
  sizes, names), and a red primary button. It covers delete, discard, delete untracked, kill, worktree remove/park,
  restore apply (with the overwrite warning) and backup delete. The confirmation is a PWA step (the gateway does not
  enforce it, R4).
- **Service worker:**
  - The same `/sw.js` runs in several registrations: the root `/` one is the app-shell and update worker, and each
    desktop's push subscription lives on its own registration at `/d/<pairing id>/` (below). Only the root worker
    installs, activates and serves the shell cache, so a desktop-scope worker that updates first never deletes the
    cache the root worker serves from.
  - Precaches the hashed app shell, cache-first; navigation falls back to the cached `index.html`.
  - Never caches transcripts, `/v1/*` or secrets.
  - On `push`, it shows the declarative payload (always `showNotification`, with a generic fallback on bad JSON).
    The desktop is read from `self.registration.scope`; the notification's tag is namespaced per desktop and its
    `navigate` carries `d=<id>`; with 2+ desktops the title carries the desktop's label (kept per scope by the app).
  - `notificationclick` focuses or opens the app at `navigate`.
  - `RELOAD_OTHERS` (root worker only): reloads the other controlled windows (see "An upgrade blocked by an old window").
  - `pushsubscriptionchange` resubscribes, and the app re-checks `getSubscription()` at every start.
  - Updates are user-confirmed ("Update available → Reload" posts `SKIP_WAITING`), and the reload waits for open
    requests.
- **Permission prompts** (notifications) are only requested from a tap. On iOS the PWA explains that it must first be
  added to the Home Screen.

## As built: gateway (P4) and PWA (P6)

Gateway (`crates/deck-service`):
- **Device layer:** `gateway/engine/device/{s4,views,tree,dirs,transfer}.rs`.
- **Moved logic:**
  - `review.rs` (prompt builder);
  - `ui_answer.rs` (typed answer → ctl value);
  - `attachments.rs` (durable per-owner store);
  - `engine/worktree.rs` (every worktree flow, parked-branch re-check every 10 min, leftover cleanup);
  - `push/{mod,vapid,ssrf,sender}.rs`.
- **Local protocol additions:**
  - Mutations:
    - `Review`, `AnswerUi`, `OpenOnDesktop`, `CloseOnDesktop`, `BackupConnectOnDesktop` (Drive only: active destination
      Drive, Google credentials present, desktop reachable);
    - `WorktreeNew{repo, branch, base_ref, dest, open}`, `WorktreeMoveSession{target, branch}`, `WorktreeFinish{worktree}`,
      `WorktreeRetire{worktree, park}`, `WorktreeContinue{primary, branch, session, place}`;
    - `Command::{Steer, Compose}`.
  - `Query::WorktreeVerdict`, `Topic::Worktrees(dir)`, `Done::Retired`, `GatewayError::NoLiveControl`.
  - `ServerEvent`/`ClientEvent` `OpenSession{file, split}`, `CloseSession{file}`, `OpenUrl{url}`.
  - App flags `--open <file> [--split none|right|down]` and `--open-url <url>`, used when no GUI is connected.
  - `ProjectApplyRestore.stop` is ignored; the gateway derives the stop list.
- **Worktree flows:** commands a flow types into a session (finish prompt, `/move`) run under internal keys. Their
  events and draft confirmations go to the client that started the flow; they are declined when that client leaves.
  Known limit: a prompt already queued is not withdrawn when the client loses its rights mid-flow.
- **Device semantics:**
  - `Welcome.features` always lists `s4`; the device's own hello is the gate for new variants.
  - `pending_ui` is listed only when the ctl hello reports `ui_hook`.
  - Browse roots are home plus the parents of projects outside home.
  - `sync_add` adds a row with every part off (as on the desktop).
  - `app_update{apply}` is allowed only for UserDir, AppImage and WindowsDir installs.
  - Finished uploads are kept up to 16 per device; the oldest is dropped first.
  - `unpair` revokes once the reply is flushed, at the latest after 5 s.
  - `DoneView::Text` is reserved: no gateway result maps to it yet.
- `backup_delete` / `sync_delete` take 1–100 distinct ids and delete them in order in one journaled job (local
  `Mutation::DeleteBackups` / `ProjectDeleteBackups`). The job stops at the first failure, naming the ids already
  deleted.
- Security fixes (S4-1…S4-9):
  - Export/blob reserve a download slot before any worker runs: 1 export and 2 blob workers per device. Export files
    are deleted when read, expired, refused or when the session ends.
  - `blob` serves raster types only.
  - Browse is clamped to its roots, and displays outside home are `<root name>/rest`. A project directly under `/`
    is its own root; `/` is never a root.
  - Push: 5 s connect / 10 s total timeouts, 16 queued per device, port 443 only, and the extra SSRF ranges (Teredo,
    NAT64, `::ffff:0:0:0/96`, 6to4 relay). Revoke/unsubscribe bump a per-device generation that every queued or
    in-flight push carries, so a purge also stops retries of an attempt already running.
- Post-review/verify hardening:
  - Device folders: the browse roots plus every project folder, compared on resolved paths (symlinks followed,
    `strip_verbatim` spelling), via `jobs::Confine`.
    - A path whose tail doesn't exist yet is allowed only below its deepest existing entry, which must be a
      directory, and without `..`.
    - Dangling links are refused.
    - `dir_of` returns the resolved path.
    - Launch, sync and restore re-check it on the worker and act on the resolved path; a token that resolves outside
      the roots answers `NotFound`.
    - A device-launched session may therefore group under the resolved spelling of a symlinked folder.
  - `blob` serves only when the declared raster type matches the file's signature bytes.
  - Revocation vs uploads (`attachments::Revocations`): a store that finishes after its owner was revoked removes the
    directory and fails. The marker lives only while that owner's writers or cleanup run, so a fresh pairing with the
    same id uploads again.
  - `Feed::Inventory` is re-projected on `Topic::Running`.
  - Worktree removal re-checks live controllers before any process is killed.
    - The engine collects late controllers (unclassified, not moved by the flow) without I/O.
    - A worker runs `deck_core::worktree::classify` on them (symlinks resolved). Inside, or location unknown → refused.
    - Back on the engine, any session that appeared during that window is refused without a look (the user retries).
    - Sessions the flow itself moved are exempt, because omp's breadcrumb can lag after `/move`.
  - Finish/Continue replace an exited controller instead of resolving to it.
  - Finish counts the turn as over when the session is idle after a busy look, or when the session file shows a user
    message followed by an assistant message. Fast turns can fall between ticks.
    - The user message must be timestamped at or after the prompt, or after the previously counted turn.
    - The file is read from the length marked at submit; if that is unknown, it is read with timestamp filtering only.
      A failed read never sets a trusted offset.
    - Reads run one at a time, at most every 400 ms, bound to the prompt's request seq.
    - Delivery plus idle alone never ends the turn.
    - Known limit: a user message written less than 400 ms before the idle look that ends a busy-observed turn counts
      as part of that turn.
  - `ReviewItem.line_side: Option<review::Side>`: the desktop sends the comment's old/new side and the builder quotes
    exactly that side; `None` (device path) infers it from the diff.
  - Desktop: Cancel finishing is offered only for this window's own jobs and confirmed only after the gateway acks it.
    A continued session opens once its controller record arrives (10 s bound).
  - Relay: directory symlinks never shadow their targets (cycle check on the ancestry only), and `Accept-Encoding`
    q-values decide br vs gzip.
  - PWA:
    - uploads are paced by one shared token bucket per client (`pacerFor`, kept across reconnects). It runs at 85% of
      the relay's device bucket (1 MiB/s, 4 MiB burst), counts wire bytes, and backs off once per link loss after a
      4029;
    - blob loads are queued two at a time and retried on `too_many_requests`;
    - history entries carry the route stack;
    - a reload resumes from the current entry;
    - the base entry is rewritten when the root changes;
    - an `unobserved` delivery keeps the draft as uncertain.
- Live gate `tests/remote_device_live.rs` uses real omp, the relay and a scripted model (`tests/support/fake_model.rs`).
  It covers ask/approve answered from the phone, the terminal answering first (`applied:false`), steer vs follow-up,
  compose with an image reaching the model byte-identically, the blob round trip, one push per finished turn (none
  while visible), and permission refusals.
- omp facts the gate found:
  - Enter is ignored in an approval dialog for its first 500 ms (`APPROVE_ARM_MS`).
  - Attached images reach the model unchanged only when both sides are at least 200 px and the file is under
    128 000 bytes.
  - `LiveSession.busy` is the injector queue; omp's own turn state is `activity`.
- `OMP_DECK_PUSH_TEST_LOOPBACK` is honoured only in builds with the cargo feature `test-push-loopback`. deck-service
  enables it for its own tests through a self dev-dependency, so `cargo test --release` keeps the push tests while
  release artifacts (`cargo build --release -p …`) never contain it (resolver 2+/3 does not unify dev-dependency
  features into normal builds).
- `Query::WorktreeDestination{repo, branch, dest}` returns the default destination and the inline verdict. Its texts are
  identical to the former desktop `check_destination`, and `WorktreeNew` applies the same check. The desktop dialog shows
  it inline again, debounced.
- `DeviceView.push: bool` (local protocol): the Mobile access card shows "Notifications on/off" per device.

PWA (`pwa/`):
- **Structure:**
  - screens `src/app/screens/**` (conversation, fleet, settings subtrees);
  - client `src/app/client.ts` (`DEVICE_FEATURES = ["s4"]`, local `unsupported` for s4 calls when the desktop lacks
    `s4`, `<NeedsS4>` state);
  - `transfer.ts`, `push-client.ts`, `update.ts`.
  - The vendored rendering comes from upstream omp commit `1ff12a7`. `THIRD_PARTY_LICENSES` carries three copyright lines:
    Mario Zechner, Can Bölük, Stencil Labs.
- **Build:**
  - Fonts are emitted as hashed files under `assets/`, never `data:` URLs, which the CSP blocks.
  - The manifest and icon links are injected by `build.ts`.
  - Typecheck runs in two tiers: strict for plain `.ts`, upstream's flags for TSX/vendored code, plus a WebWorker config.
- **Deep links:** `#route=<tab|screen>&session=&agent=&live=&project=&section=` and `#s=<session id>`, optionally
  with `d=<pairing id>` first (`#d=<id>&s=…`): the link then opens on that desktop, switching to it first when it is
  not active. A `d` that is not a paired desktop drops the whole link. They work at boot (the stored active desktop
  is replaced by the link's), from service-worker navigate messages and on `hashchange`. A link's `title`/`display`
  is never honoured (S4-3): screens and confirmations take names from feeds by id. Push `navigate` must be
  same-origin; only path + hash are kept. History entries record their desktop, so Back entries written for another
  desktop read as "no route" after a switch.
- **Push (per desktop):** see "Web Push per desktop" below.
- **Interaction choices:**
  - A multi-select UI request is answered one choice per tap, because omp toggles one option per call.
  - Fork from a conversation needs the session to be live (it needs its project).
  - Review comments use new-file line numbers; removed lines cannot be commented on.
  - Rename needs a live session (omp `/rename`).
  - The composer follows omp's `activity`, exactly as the gateway gates `compose`:
    - send needs `idle`; steer and follow-up need `busy`;
    - `unknown` ("Starting", omp has not drawn its title) and `attention` take insert only.

    A send before omp is idle would be refused (`Precondition`). A refusal or failure stays visible in the hint while
    the text is kept for a retry.
  - The terminal's line input is enabled only once a view is attached in a typing mode, so lines typed during a
    re-attach are never dropped. It is labelled "Line to send to the terminal", distinct from xterm's own "Terminal
    input".
- **Smoke harness:** `pwa/test/smoke/` (`serve.ts`, `shoot.ts`, routes in `routes.ts`) is never shipped (`build.test.ts`).
  `bun run e2e` runs 7 harness scenarios with zero console errors.
- **Live gate:** `bun run live` builds `dist/` and starts `crates/deck-service/tests/pwa_live_stack.rs` (`#[ignore]`; the
  real relay serving `dist/`, a real gateway, real omp on the scripted model, under `dbus-run-session`). It drives
  Chromium on a persistent profile through these steps:
  - pair with a fingerprint match and `#pair=` stripped;
  - the WebSocket under the CSP;
  - real FCM `pushManager.subscribe`, accepted by the SSRF guard;
  - a new session (Starting → Send);
  - prompt; ask answered on the phone; steer mid-turn;
  - an image attachment byte-identical at the model;
  - one decrypted push for a backgrounded turn;
  - HTML export download; terminal Input line reaching omp;
  - 0 console errors and 0 CSP violations.

  The launcher's `/live` endpoint reports controller state when a step stalls.

## Steps and gates

| Step | Deliverable | Owner | Gate |
|---|---|---|---|
| P1 | Wire additions (Rust + TS mirror + `api-s4.json` + freeze pins) | deck-remote implementer | Both decoders accept `api-v1.json` unchanged and every `api-s4.json` vector; exhaustive `all_variants`; `names_unknown` in step |
| P2 | Relay web serving + transport fix + packaging | deck-relay implementer | Chromium loads the app over keep-alive under the CSP and connects its WebSocket; tests for headers, 404s, precompressed bodies and the per-request deadline |
| P3 | Core prerequisites: rich transcript items (structured args, details, images + blob lookup), directory listing (directories only), shared review prompt builder | deck-core / deck-service implementers | Unit tests on fixtures |
| P4 | Gateway: every new op/feed translated; push; uploads/downloads; steer; moved desktop logic | deck-service implementers (2, split by area) | `remote_device` e2e extended: compose with an attachment reaches omp; steer while busy; download of an export; push to a loopback receiver decrypts; folder browse + new session in a folder; worktree create/finish through the gateway |
| P5 | Desktop cutover to the moved logic; `ServerEvent` open/close/split; Mobile access shows each device's push state | deck-ui | Desktop worktree/review/restore/fork flows unchanged in behaviour (existing tests + smoke) |
| P6 | PWA app: shell, themes, pairing, client/store, every screen, terminal, service worker, push client | PWA implementers (split by area) | `bun test` + typecheck; Playwright (installed Chromium) e2e against a real relay + gateway + omp: pair, list, open a session, prompt, steer, approve, terminal input, download, push subscription stubbed |
| P7 | Security review of the slice-4 surface + verify + docs | reviewers, deck-verify | Findings fixed or accepted in writing |

Real-phone delivery (Android Chrome, iOS 16.4+ Home Screen, real push services) is slice 5's release proof. Slice 4
proves everything that can run on this machine and in CI.

## Slice 5: real-phone proof (Android)

Proven on a Motorola Edge+ 2023 (Android 14, Chrome 156) over USB, by hand first and then as the re-runnable gate
`bun run phone` (`pwa/e2e/phone/`). iPhone (iOS 16.4+ Home Screen web app) is **not** proven: no device available.

- **Setup:** the same launcher as `bun run live` (`pwa/e2e/live/stack.ts`): real relay serving `dist/`, real
  sandboxed gateway, real omp on the scripted model. The relay listens on a fixed port (`OMPDECK_LIVE_RELAY_PORT`,
  default `PHONE_RELAY_PORT` = 47319), `adb reverse` maps it, so the phone's Chrome loads `http://127.0.0.1:47319`.
  That is a secure context, so the service worker and Web Push work. Push goes through **the real push service**
  (`fcm.googleapis.com`), not the loopback receiver. TLS termination (Caddy/nginx, `docs/relay.md`) is not part of
  this proof.
- **Driving:** input is physical. The gate finds web elements in `uiautomator` dumps (Chrome exposes the page's
  accessibility tree with screen bounds) and sends `input tap` / `input text` / `keyevent`, so taps, the soft
  keyboard, Chrome's permission prompts, the Android file chooser and the notification shade are the real OS
  paths. DevTools (`adb forward … localabstract:chrome_devtools_remote` + Playwright `connectOverCDP`) only
  observes: the transcript, WebSockets, CSP reports and console errors.
- **Steps (all passing):**
  1. pairing link opened by an Android `VIEW` intent (what a scanned QR does); fingerprint matches the desktop; approved;
  2. still paired after a reload; device WebSocket open;
  3. Settings → Notifications → push switch → Chrome's "Allow" prompt (first run on the origin) → subscribed at
     `fcm.googleapis.com`; the desktop's Mobile access shows push on;
  4. new session; a prompt typed with the phone's keyboard; the composer stays above the keyboard (`visualViewport`);
  5. omp's question answered with taps;
  6. image attached through Android's chooser ("Photos & videos"), reaching the model byte-identical;
  7. a 20 s turn (`PUSH-LONG-MARK`); back to the session list; Home while the turn still runs → exactly one
     "OmpDeck — A session finished" notification in Android's list (~13 s after leaving); tapping it in the shade
     brings Chrome back **on that session**, and the notification closes;
  8. relay restarted (`POST /relay-restart`; same port, room and enrolment): the phone reconnects on its own (~3 s)
     and the next prompt goes through;
  9. 0 CSP violations, 0 console errors.
  10. **Two desktops** (a second real gateway in its own sandbox and room on the same relay; `startFirstOfTwo` /
      `startSecondDesktop` in `stack.ts`, test launcher env `OMPDECK_LIVE_SECOND_ROOM`, `OMPDECK_LIVE_JOIN_RELAY`,
      `OMPDECK_LIVE_JOIN_KEY`): desktop B paired through the phone; switcher lists both and A → B → A works with only
      the active gateway online; notifications enabled on both → `/d/<idA>/` and `/d/<idB>/` each hold a
      `fcm.googleapis.com` subscription, the root registration none; a 30 s turn on B (`PUSH-SWITCH-MARK`), switch to
      A while it runs → one notification shown by B's registration (`data.desktop` = B, tag `<idB>:…`, title
      `OmpDeck — <B's label>`, also what the shade shows), none by A's;
      tapping it switches the app to B on that session; forgetting B removes its pairing and registration, A keeps
      its subscription and answers a prompt. Whole gate ≈ 281 s.
- **Fixed (found here):** a live session's id came only from an open handle on its jsonl (`follow_session_files`);
  omp writes a fresh session's file when the first reply lands and drops the handle when it rewrites the file (the
  title), so after a long *first* turn the gateway never learned the file: the push carried only the live key and the
  app stayed on "Starting the session…". Now `drive_live` follows a session with no known file over live control and
  adopts the snapshot's `sessionFile` once it exists on disk (rechecked every second). Gate: step 10's push turn is
  the session's first; `remote_device_live.rs` (g).
- **Hygiene:** before and after a run the gate clears the origin's site data (pairing, service worker, push
  subscription) with `Storage.clearDataForOrigin` on a **tab's** CDP session (Android Chrome answers the
  browser session with "Internal error"), closes the origin's tabs, removes the pushed image, collapses the shade
  and removes its port mappings. Chrome's notification permission for the origin stays granted, so later runs
  skip the prompt (the gate reports which case ran). Phone settings are never changed.
- **Findings:**
  - **Fixed:** the notification badge was the full-colour `icon-192.png`; Android draws badges from the alpha
    channel only, so it showed a blank disc. Now `public/badge-96.png` (white stacked cards with `>_` cut out,
    transparent), precached.
  - **Harness:** while DevTools is attached Chrome keeps the page `visible` after Home, so the app keeps reporting
    `presence` visible and the gateway (correctly) skips the push. The gate detaches before Home. Chrome's DevTools
    socket also refuses connections while Chrome is in the background, so cleanup brings Chrome to the front first.
  - **Harness:** `adb reverse --remove` does not drop established connections, so it cannot simulate a network loss;
    the gate restarts the relay instead.
  - **Accessibility:** uiautomator lists the composer's textarea with `NAF="true"` and no name even though it has
    `aria-label="Message"`; the gate finds it as Chrome's only `EditText` without a resource id. Not changed.

Run: phone unlocked, USB debugging on, `cd pwa && bun run phone` (`ANDROID_SERIAL` picks one of several devices).
Screenshots go to `/tmp/pwa-smoke/phone/`.
