# omp session actions & control channels for the OmpDeck status bar

Verified against the installed binary — `omp --version` printed **`omp/18.12.5`** on 2026-10-21 (docs/notes/omp-cli.md says 18.12.3; the binary was updated since; behaviour below was observed on 18.12.5).
Source of truth = the JS embedded in the binary (`strings -n 6 ~/.local/bin/omp`; cite `packages/coding-agent/src/<file>.ts` + symbol — the oh-my-pi checkout is 18.4.10 and OLDER).
All experiments: sandbox `PI_CODING_AGENT_DIR=/tmp/ompact/agent`, `PI_CONFIG_DIR=/tmp/ompact/cfg`, tmux socket `-L omp-deck-test-act` with OmpDeck's `docs/notes/tmux.conf`, model = a local fake OpenAI-compatible SSE server (127.0.0.1:18777, via `models.yml`, no real provider, no real prompt), collab relay = throwaway local relay (127.0.0.1:7567). Everything deleted afterwards (see end).
Sandbox setup facts you need to reproduce: set `startup.setupWizard=false` (else a 5-step onboarding takes over the pane); `models.yml` provider needs `apiKey` (literal or env-var name; a literal counts as "configured") – use `auth: oauth` for a provider that must count as *unauthenticated*.

---------------------------------------------------------------------------------------------------
## 0. TL;DR architecture recommendation

| need | mechanism | why |
|---|---|---|
| **live status** (model, thinking, busy, ctx %, session name, cwd, queue count) | (1) **collab guest** over the local relay (push, ~100 ms) — if available; (2) fallback: tail the session `.jsonl` + tmux title OSC | collab `state` frames carry exactly the footer data; no polling |
| tokens in/out, cost, git, PR, subagents | session jsonl (`usage` on assistant messages) + `git` on cwd; subagents from collab `agents` frames / `bus` frames | not in `state` frame |
| idle vs busy | title OSC `π <glyph> <title>`: `>` idle, braille spinner = busy (verified); collab `state.isStreaming` / registry `busy` (verified) | |
| **current session file** of a tab (after /new, /fork, /resume, /move) | `<agentDir>/terminal-sessions/<pane_tty basename with "/"→"-">` e.g. `pts-11` (2 lines: cwd, session-file path; verified updates on /new, /fork, /move) | omp rewrites it on every session switch |
| **actions** | slash-command injection into the pane (bytes in §5) for session-changing ones; **direct hotkeys** for model/thinking/plan/hub; **new tab** for fork-to-new-tab | collab guests CANNOT run slash commands (verified) |
| model + thinking dropdown | inject `/switch <provider/id>[:level]` and `/effort <level>` (§6) | no picker, arg form verified |

---------------------------------------------------------------------------------------------------
## 1. Slash commands in omp 18.12.5 (interactive)

Registry: `packages/coding-agent/src/slash-commands/builtin-{lifecycle,control,modes,collaboration,marketplace,session,skills}.ts`, dispatched by `hz(text, {ctx})` in `builtin-registry.ts` (`handleTui` preferred, else `handle`). Commands are dispatched **before** the streaming/steer queue logic in `InputController` `editor.onSubmit` ⇒ **a slash command runs immediately even while a turn is streaming** (verified), but many handlers refuse/abort themselves (column "mid-stream").

Legend — **Mode**: `I` = runs immediately (result as status/warning line in the transcript); `P` = opens a picker/overlay (user is looking at the terminal, fine); `P?` = picker only when arg omitted. **V** = observed in sandbox; **S** = from source only.

### Session lifecycle / context
| command | args | description (bundle) | mode | mid-stream | notes |
|---|---|---|---|---|---|
| `/new` | – | Start a new session | I V | S (no guard seen; `handleNewCommand`→`handleClearCommand`) | switches session file (breadcrumb updated V); collab room rotates (V: guest gets `bye`, close 4001, generation+1) |
| `/clear` | – | Clear context in place, keep session | I S | refuses while streaming ("Wait for the current response…") | `handleResetContextCommand` |
| `/delete` | – | Delete the current session and start a new one | I S | – | DESTRUCTIVE – confirm in OmpDeck UI first |
| `/fresh` | – | Reset provider stream state, keep transcript | I S | refuses while streaming | |
| `/compact` | `[soft\|remote\|snapcompact] [focus…]` (`snapcompact` takes no focus) | Manually compact the session context | I V | **NOT guarded: observed to abort the in-flight stream** (fake server saw `ConnectionResetError`), then compact, then auto-"resume intent" turn | idle only. Needs ≥2 messages else warning "Nothing to compact (no messages yet)" / `Compaction failed: Nothing to compact (session too small)` (V) |
| `/shake` | `[elide\|images\|thinking]` | Drop heavy content from context | I S | S | |
| `/handoff` | `[focus instructions]` | Summarize into a handoff doc and compact **in place** (same file) | I (progress loader) V | **refuses**: `Warning: Wait for the current response to finish or abort it before handing off.` (V) | needs ≥2 messages; with a tiny session errored `Handoff failed: Nothing to hand off (already compacted)` (V) |
| `/retry` | – | Retry last failed turn | I S | refuses | hotkey F5 / Alt+R |
| `/fork` | `[pane\|window\|tab]` | Fork this session (in place) or open the fork in a multiplexer pane/window | no arg: I V; with arg: spawns multiplexer split via `TMUX` env detection (S, `handleForkCommand`→`#$`) | no-arg refuses while streaming; with arg shows a confirm hook | **no-arg switches THIS process to the fork** (new file, `✔ Session forked · return to original: omp --resume <id>`; breadcrumb updated V). `/fork window` would create a window in OmpDeck's own tmux server – avoid; use `omp --fork` in a new OmpDeck tab instead |
| `/branch` (alias `/rewind`) | – | Rewind to a previous message, keep old path as a branch | P S | | double-Esc on empty composer opens it too (setting) |
| `/tree` | – | Navigate session tree | P S | | |
| `/resume` | `[session id\|@claude\|@codex]` | Resume a different session | no arg `P`; with id: I (in-place switch, S) | | |
| `/restart` | – | Restart omp with same launch flags, resuming this session | I S | | re-execs the process inside the pane |
| `/move` | `[<path>]` | Move session to another directory | I V | refuses while streaming (`Cannot move while streaming.`) | path arg triggers a **path-autocomplete popup** — see §3 pitfall. Renames the jsonl into the new project dir (V: breadcrumb → `…/-tmp-ompact-proj2/…jsonl`) |
| `/wt` (alias `/worktree`) | `[<branch>]` | Move session into a new git worktree | I S | refuses streaming | |
| `/add-dir` `/remove-dir` `/dirs` | `<path>` / `<path>` / – | multi-root workspace | I (`/dirs` V prints list) | | |
| `/rename` | `[title]` (omit = auto-generate via model) | Rename the session | I V (`Session renamed to "X".`, title OSC updated V) | fine mid-stream (V via collab: state.sessionName updated) | |
| `/pin` | `[session id]` | Pin/unpin in resume list | I S | | |
| `/exit` `/quit` (`/q`) | – | Exit | I S | | |

### Model / thinking / modes
| command | args | description | mode | notes |
|---|---|---|---|---|
| `/model` (alias `/models`) | – (TUI **ignores args**) | Switch model (roles) | **P V** (big overlay, Esc closes) | the *persisting* picker (sets role) |
| `/switch` | `[model]` — fuzzy id, `provider/id`, `@role`, `:level` suffix | Switch model **for this session only** | no arg `P V`; **with arg I V** | §6 |
| `/effort` | `[level]` | Set thinking level for this session | no arg `P V` (or status if model has no thinking); with arg **I V** | §6 |
| `/modelpreset` | `[list\|save\|switch\|delete] [name]` | role-model + thinking presets | `P?` S | |
| `/plan` | `[prompt]` | Toggle plan mode | I V | cycles on→paused→off (V: `Plan mode enabled` → `Plan mode paused.` → off; status segment `🗺 Plan` / `🗺 Plan ⏸`); `plan-review` re-opens review |
| `/goal` `/guided-goal` `/vibe` `/loop` `/queue` `/force` `/pause` | modes/queueing | `/goal` without arg opens an input overlay (V) | | |
| `/fast` `/slow` `/extended-context` `/skillful` `/computer` `/browser` `/advisor` `/prewalk` | – | toggles | I S | |

### Info / sharing
| command | description | mode |
|---|---|---|
| `/context` V, `/jobs` V, `/tools` V, `/hotkeys`, `/changelog`, `/usage`, `/stats`, `/trace`, `/todo`, `/debug`, `/hub`, `/agents`, `/git [rev]`, `/settings`, `/setup`, `/memory`, `/mcp`, `/skills`, `/marketplace`, `/plugins`, `/reload-plugins`, `/login`, `/logout`, `/ssh`, `/security` | overlays / viewers (`/context`,`/jobs`,`/tools` observed as bordered overlays closed by Esc) | P |
| `/export [--themes] [path]` | HTML export **and opens it in the browser** (`handleExportCommand` calls `openInBrowser`) — NOT run in sandbox | I S |
| `/dump` | copies transcript to the **system clipboard** + writes LLM request JSON to tmp — NOT run | I S |
| `/share` | uploads an encrypted copy to the share server (network, `share.serverUrl`, default my.omp.sh) or secret gist — NOT run | I S |
| `/copy` | picker "text or code to copy" | P S |
| `/open` | opens last link in browser | I S |
| `/collab [start\|view\|list\|stop\|status] [relayUrl]`, `/join <link>`, `/leave` | live sharing | I S |
| `/btw [q]`, `/tan <work>`, `/omfg`, `/cleanse`, `/ratchet`, `/live`, `/record`, `/forget-approvals`, `/skills`… | misc | – |

### Dedicated hotkeys (default keymap `ze` in the bundle; user overrides live in `<agentDir>/keybindings.json`, absent on this machine)
`Esc` interrupt · `Ctrl+C` clear composer (twice <500 ms = exit) · `Ctrl+D` exit · `Shift+Tab` cycle thinking · `Ctrl+P`/`Shift+Ctrl+P` cycle role models · `Alt+P` temp model picker V · `Alt+M` model/role picker · `Alt+Shift+P` plan toggle V · `Alt+A` agent hub V · `Ctrl+S` observe sessions · `Ctrl+G` external editor · `Ctrl+Q`/`Ctrl+Enter` queue "after yield" follow-up V · `Alt+↑`/`Shift+↑` dequeue · `F5`/`Alt+R` retry · `Ctrl+R` history search · `Alt+L` display reset · `Alt+Shift+C` copy prompt.

---------------------------------------------------------------------------------------------------
## 2. Control channels into a RUNNING interactive omp

| channel | exists? | evidence |
|---|---|---|
| per-instance unix socket for *commands* | **No.** The only per-instance socket is the collab registry endpoint (ops `snapshot`, `link` only) | `collab/registry.ts` `B()`: `op` must be `snapshot` or `link`, else `invalid_operation` |
| local HTTP / WS server per instance | **No** (only when hosting collab: it is a *client* of a relay) | host.ts `Ip.start` uses `Nl({role:"host"})` relay client |
| **collab** (`collab.autoStart`) | **Yes — the only programmatic channel.** Read live state + send prompts/abort. **Cannot run slash commands.** | below |
| `omp acp` / `--mode rpc` | separate headless processes over stdio; cannot attach to a TUI. ACP can `session/load` a file but then two processes write one session | `omp --help`, `--mode` values `text,json,rpc,rpc-ui,acp` |
| signals | no omp-specific `SIGUSR*` handlers found in the bundle (only SIGINT/TERM/CONT/TSTP-style) | grep of bundle |
| watched files | `config.yml` (+project config, `--config` overlays; 200 ms debounce, see omp-cli.md §A3). `keybindings.json` read at start. Session jsonl is **not** watched for input. | |
| breadcrumb `<agentDir>/terminal-sessions/<tty>` | **written** by omp (read-only for us) | §4 |
| title OSC | **emitted** | §4 |

### 2.1 Collab = live state + prompt/abort channel (verified end to end)
Docs (18.4.10 `docs/collab.md`) + the user's own working client `~/projects/omp-discord-bridge/src/collab.ts` + my probe. **This machine's real `~/.omp/agent/config.yml` already has `collab.autoStart: control` and `collab.relayUrl: ws://127.0.0.1:7466`** (relay = the omp-discord-bridge daemon, pid via `ss -ltnp | grep 7466`), so every real omp session is already hosted and listed.

**Discovery (CLI or direct):**
* `omp collab list --json` → `{"version":1,"hosts":[{"instanceId","generation","pid","sessionId","sessionName"|null,"cwd","model":{"provider","id"}|null,"startedAt":<ms>,"participants","relayConnected","inputRequired","busy":true|false|null,"access":"control"|"view"}]}` (~1.5 s). `busy` = turn in flight (V: false idle). `instanceId` stable per process; `generation` +1 on every session switch (V: /new → generation 2, new `sessionId`).
* `omp collab link <instanceId|pid> [--view] --json` → `{"version":1,"instanceId","generation","access","url"}`; url form `http://127.0.0.1:7567/#ws://127.0.0.1:7567/r/<roomId>.<key32+token16 base64url>`. Control link = 48 secret bytes (32 AES key + 16 write token); `--view` = 32 bytes.
* Registry on disk: `<configRoot>/run/collab-hosts/<id>.json` `{version,instanceId,pid,endpoint,createdAt,token}` + unix socket `<id>.sock` (`configRoot` = `$PI_CONFIG_DIR` or `~/.omp`; V: sandbox hosts appeared under `PI_CONFIG_DIR`). Socket protocol (line-framed JSON, ≤4 KiB req): send `{"v":1,"token":"<json.token>","op":"snapshot"}` → `{"ok":true,"v":1,"snapshot":{…same fields as list…}}`; `{"op":"link","access":"control","generation":N}` → `{"ok":true,"url":…}`. **Faster than spawning `omp collab list`**; errors `authentication_failed`, `stale_generation`, `access_unavailable`. (socket op shape from `collab/registry.ts` source; not hand-tested, the CLI path is.)

**Relay contract** (`packages/collab-web/scripts/local-relay.ts`, mirrored in omp-discord-bridge `relay.ts`): `GET /r/<roomId>?role=host|guest` WebSocket. Binary frames = `[4-byte BE peerId][12-byte IV][AES-256-GCM ciphertext+tag of UTF-8 JSON]`. Host→relay peerId 0=broadcast, N=to guest N; guest→host relay overwrites the 4 bytes with the sender's peer id. TEXT frames from relay to host: `{"t":"peer-joined","peer":N}` / `{"t":"peer-left",…}`; to guests `{"t":"room-closed"}` then close 4001. Close codes: 4001 room closed, 4004 no such room, 4009 host exists, 4029 full. Relay is content-blind; `ws://` allowed only for localhost/127.0.0.1/::1.
**OmpDeck needs no relay of its own when the user's relay (7466) is up** — a guest just dials the relay URL embedded in the link. If the relay is down omp shows the registry entry only after the relay connected (`relayConnected`), so *no relay ⇒ no registry ⇒ no channel*. To make OmpDeck self-sufficient: embed the ~60-line relay (tokio-tungstenite) on a loopback port and launch tabs with a per-run overlay `omp --config <overlay.yml>` containing
```yaml
collab:
  autoStart: control
  relayUrl: ws://127.0.0.1:<port>
```
(V: `--config` overlay made a fresh `omp` host itself while global `collab.autoStart` was `off`; `collab:1` segment appears in its status line.)

**Guest protocol v3** (proto must equal 3; host constant `Xne`). Verified with a Bun probe (`hello` → `welcome` …):
```
guest → host   {"t":"hello","proto":3,"name":"OmpDeck","writeToken":"<b64url 16B>"}   (omit writeToken ⇒ read-only; host replies readOnly:true)
               {"t":"prompt","text":"…","images":[…]}     {"t":"abort"}     {"t":"agent-cmd","cmd":"chat|kill|revive","agentId":…,"text":…}
               {"t":"fetch-transcript","reqId":n,"agentId":…,"fromByte":n}   {"t":"ui-response","reqId":n,"value":…}
host → guest   {"t":"welcome","proto":3,"header":{session header},"state":<State>,"agents":[…],"entryCount":n,"readOnly"?:true}
               {"t":"snapshot-chunk","entries":[…],"final":bool}            (full back-transcript, ≤512 KiB/chunk)
               {"t":"state","state":<State>}      debounced 100 ms on agent_start/end, message_end, tool_execution_end, thinking_level_changed, model_changed, advisor_cost_changed, auto_compaction_end; every 2 s while a turn runs
               {"t":"event","event":<AgentEvent>} (agent_start/end, turn_*, message_start/update/end, tool_execution_*, notice, auto_compaction_*, auto_retry_*, thinking_level_changed)
               {"t":"entry","entry":<jsonl entry>}  {"t":"agents","agents":[{id,displayName,kind,parentId,status,hasSessionFile,createdAt,lastActivity}]}
               {"t":"bus","channel","data"}  {"t":"ui-request","request":{reqId,kind:"select"|"editor",title,options}}  {"t":"ui-request-end","reqId"}
               {"t":"transcript",…}  {"t":"error","message"}  {"t":"bye","reason"}
State = {"isStreaming","isAborting","queuedMessageCount","sessionName"|null,"cwd","model":{full model object incl. contextWindow, reasoning, "thinking":{"mode":"effort","efforts":[…]}},"thinkingLevel":"low|…|off"?, "contextUsage":{"tokens","contextWindow","percent"},"participants":[{name,role:"host"|"guest",readOnly?}]}
```
Observed (sandbox): welcome right after `hello` incl. `contextUsage {tokens:1234,contextWindow:50000,percent:2.468}`; after `/effort low` typed in the pane a `state` with `"thinkingLevel":"low"` arrived ≈0.2 s later; `/switch fake/fake-2` ⇒ state.model.id=fake-2, contextWindow 50000 (context % recomputed); `/rename` ⇒ `sessionName`; turn start/end ⇒ `isStreaming` true/false.
**Not in state**: cost, tokens in/out, git branch/PR, plan mode, cwd-change events other than `cwd`. Get those from the jsonl (assistant `usage{input,output,cacheRead,cacheWrite,cost{total}}`, `model_usage`, `compaction`, `title_change`, `custom:todo_hud_state`), `git`, and (subagents) the `agents` frames.

**Guest `prompt` is NOT a slash-command channel (V):** sending `{"t":"prompt","text":"/dirs"}` made the host start an agent turn with a `custom_message` `customType:"collab-prompt"`, `details.from:"<name>"` whose content was literally `/dirs`, and the fake LLM received it as user text. Host code `#Ge` → `session.promptCustomMessage({customType, content, …},{streamingBehavior:"steer"})`; the TUI itself rejects `/…` for guests ("host-only during a collab session"). Guest prompts while a turn runs are **steered** into it. `abort` = same as Esc (`session.abort`).
**Side effects of being a guest:** host transcript gets an info notice `"<name> joined the collab session"` / `left` each time and the status line shows `collab:N` (N = participants). Keep ONE long-lived connection per tab (and don't reconnect-loop); frames larger than welcome (snapshot-chunk) are re-sent on every join (the whole back-transcript, up to 24 MiB).
**Session switch ⇒ room dies:** on `/new`, `/fork`, `/resume`, `/branch` the host sends `{"t":"bye","reason":"session switched; …"}`, relay sends `room-closed`, close 4001 (V). The registry entry reappears with `generation+1`; OmpDeck must re-`list`/`link`/rejoin (retry with backoff; first room may take ~0.5–1 s).
**Security:** a control link/token = full prompting power over that session; keep in memory only, never log. Registry dir is 0700, socket 0600.

---------------------------------------------------------------------------------------------------
## 3. Keystroke-injection facts (all proven in sandbox tmux; byte sequences also proven through an outer PTY running `tmux attach`, i.e. the way OmpDeck talks to omp)

Harness: python `pty.fork()` → `tmux -L omp-deck-test-act -f docs/notes/tmux.conf attach -t t`, window 150×45, `os.write(master, bytes)`, then `tmux capture-pane -p`.

1. **Clear composer**
   * `Ctrl+C` (`0x03`) clears the *whole* composer incl. multiline (V). Works mid-stream and does **not** abort the turn (V). Handler `InputController.handleCtrlC`: if the previous Ctrl+C was <500 ms ago → `shutdown()` (**exits omp**). ⇒ never send two `0x03` <600 ms apart, and account for the user's own last Ctrl+C (OmpDeck sees all input; keep a per-tab `last_ctrl_c`).
   * `Ctrl+U` (`0x15`) only kills the current line (V); on `aaa⏎bbb⏎ccc` it took 5 presses to empty. Exit-safe alternative: send `0x15` × (2·lines+1).
   * `Esc` does **not** clear text (V).
2. **Esc semantics:** with an autocomplete popup open → closes the popup only. With no popup: idle+text = no-op (V); **streaming = interrupts the turn** (V: 2nd Esc aborted the stream); double Esc <500 ms on an *empty* composer opens `/tree` or `/branch` (setting `doubleEscapeAction`). ⇒ only send `Esc` when idle.
3. **Enter semantics:** idle: submits. **Streaming:** composer text is queued as a *steering* message (`Steering · 1`, delivered with a `<system-notice> User interjection during work` after the turn); `Ctrl+Q` (`0x11`) or `Ctrl+Enter` queues an "After yield" follow-up (V; the bundle says Alt+Enter is *not* a binding — it only edits). **Empty composer + Enter while streaming = abort the turn** (`onSubmit`: `!text && isStreaming → session.abort`) ⇒ **never send a bare `\r` while busy**.
4. **Slash commands mid-stream:** execute immediately, not queued (V: `/plan` toggled, `/switch` and `/rename` applied; `/handoff` printed the "Wait for the current response…" warning; `/compact` aborted the stream (see §1)).
5. **Bracketed paste of a command works** (V): `ESC[200~/dirs ESC[201~` then `\r` (even in the *same* write `ESC[200~/dirs ESC[201~\r`, V) executes it. A newline *inside* the paste does not submit (V: it stays in the composer). Plain typed bytes `/dirs\r` also work (V). Paste is preferable (no per-char key handling).
6. **PITFALL – autocomplete popup eats Enter:** typing `/…` opens a popup; when the typed text is a complete command name, Enter ran it (V for `/compact /plan /dirs /fork /new /model /effort /context /jobs /tools /goal`). But for **path arguments** (`/move /tmp/x`, `/add-dir`) the popup lists `proj2/`; the first Enter **applies the completion** (V: composer became `/move /tmp/ompact/proj2/`) and only a 2nd Enter submits (V). A trailing space did not close it (V). Reliable sequence **when idle**: `paste → ~150 ms → Esc (closes popup) → Enter` (V on `/move`, `/dirs`). Don't use Esc when busy (item 2); mid-stream-capable commands (`/switch`, `/effort`, `/rename`, `/plan`) worked with plain `paste + Enter` in tests (V) — `/switch` args can also show a model popup, so prefer §6's hotkey-free fallback: verify afterwards via `state`/title/jsonl.
7. **Injector as built (`crates/deck-service/src/inject.rs`; per controller, serialized, ≥300 ms between actions).** Every step after
   the clear waits for omp's screen (OmpDeck's own grid), never a fixed delay:
   ```
   idle? (title glyph '>')                               # else refuse, or run a mid-stream-safe command without Esc
   composer draft → confirm → 0x03 (guard vs. last C-c); ≥ 650 ms, then until the composer reads empty (≤ 3 s)
   baseline = rows below the empty composer
   ESC[200~<command>ESC[201~ ; until the composer shows it (≤ 3 s) and the rows below hold still 150 ms
   rows below == baseline → 0x0d                          # no popup: no Esc at all
   else (idle only) Esc ; ≥ 150 ms, then until the rows below change (popup closed; ≤ 2 s, else give up) → 0x0d
   ```
   "Rows below change" ignores omp's status line, the last row (`composer::rows_below_changed`): omp draws the popup
   inside the editor right under the composer (`tui/src/components/editor.ts`) and the status host last
   (`tui/src/prompt/composer.ts`), so a popup always changes the rows above it or their count, while the status line
   redraws on its own — omp refreshes git counts on input (V 0.22.2: `?6` → `?7` 30 ms after the paste in a repo with
   changing files). Treating that as a popup sent Esc and stalled every prompt/command there without its Enter.
   A step omp never shows in time abandons the job without Enter (`Poll::stalled`, toast "… check the composer").
   Measured (omp 18.12.6 in tmux): the pasted text renders ~230 ms after the paste, the popup ~16 ms later; after Esc
   the popup closes in ~62 ms. After a Ctrl+C the old popup stays on screen (stale); Enter with that popup open applies
   the top (exact) completion and submits.
8. **Overlay already open** (picker/hub): injected text goes to the overlay's search box; Esc closes it. Before injecting, check the grid for a bordered overlay or accept that the button press is ignored.
9. **Direct hotkeys need no composer handling and work with text in the composer and mid-stream** (V via outer PTY/tmux): `Alt+Shift+P` = `ESC P` (`\x1bP`) toggled plan mode; `Alt+P` = `ESC p` opened the temp model picker; `Alt+A` = `ESC a` opened the hub. (tmux `extended-keys` csi-u mode from tmux.conf didn't interfere; if OmpDeck's encoder emits kitty/CSI-u (`ESC[112;3u` style) form instead, omp's key parser also understands it [UNVERIFIED].) Remember users can remap in `keybindings.json`.
10. **Keys are injected as key strokes, never raw bytes** (`inject::Write::Key` → `deck_term::Terminal::send_key` →
   `input::encode_stroke`: press + release encoded exactly like the user's own key under the active protocol). Bytes per
   protocol: tmux Esc = `ESC O [`, Enter = `\r`, Ctrl+C = `0x03`, Alt+Shift+P = `ESC P`, Alt+A = `ESC a`, F5 = `ESC[15~`;
   win32-input-mode = `CSI Vk;Sc;Uc;Kd;Cs;Rc _` down/up records wrapped in modifier records; kitty = CSI-u (+release with
   event types). Only the bracketed paste stays raw bytes. Reason (Windows bug, source-verified in microsoft/terminal
   `src/terminal/parser/stateMachine.cpp` `ProcessString` + `InputStateMachineEngine.cpp`): once ConPTY has decoded any
   win32-input-mode record (i.e. the user typed anything in the tab), it caches a trailing lone `ESC` of a write as the
   start of a sequence instead of flushing it, so the old raw `0x1b` … 150 ms … `\r` merged into `ESC \r` = Alt+Enter
   (an edit in omp): `/switch` stayed in the composer and the following `/effort` hit the draft confirm. Raw `ESC P`
   (DCS entry) and Stop's lone `ESC` had the same problem.
11. **Esc + Enter merge anywhere upstream = Alt+Enter (Linux too, 0.14.0):** omp's `StdinBuffer` (`packages/tui/src/
   stdin-buffer.ts`, 75 ms escape timeout) reads `ESC` followed by `\r` within 75 ms — or in one read — as Alt+Enter:
   a newline under the still-open popup, command unsent (the user's screen: `▎ /switch …`, an empty `▎` row, popup).
   tmux forwards a lone Esc as `\x1b` and keeps the gap, but any stall (tmux server, OmpDeck's PTY thread, ConPTY)
   delivers the queued bytes together: V with Esc→Enter gaps of 5/30 ms (stuck) vs 100/150 ms (ok), `\x1bO[\r` in one
   write (stuck), and a SIGSTOP of the test tmux server for 400 ms right at the old timed Esc (stuck). Hence item 7's
   screen-verified Enter.

### Idle / busy detection from outside (verified)
* **Title OSC 0** `π <glyph> <label>`; `label` = session title (or cwd basename). Glyph: `>` idle; braille spinner `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` ~every 100 ms while working (V: `π ⠼ Test Title X`); `!` = attention (source: `v_t()` in terminal-title; not triggered in my tests) [UNVERIFIED trigger]; `:` when reduced-motion. tmux forwards it (`set-titles on`, `#T`), `#{pane_title}` gives it. Style depends on setting `terminalTitle*` (pulse/dots/line variants exist).
* collab `state.isStreaming` / registry `busy` (V). `busy` is false during compaction/queued work gaps.
* jsonl: last assistant message `stopReason` + `usage` appended on each turn end; `custom:tool_execution_start` entries mark tool runs.
* Status-line glyph row (`⎋ Working…`, `⠼ 4s >`) is screen-scraping only.

---------------------------------------------------------------------------------------------------
## 4. Tracking which session a tab is on

* **Breadcrumb** `<agentDir>/terminal-sessions/<id>` where `<id>` = stdin tty with `/dev/` stripped and `/`→`-` (`/dev/pts/11` → `pts-11`; else `tmux-%N` if no tty) (`Qee()` in `session/…`). File = 3 lines: `<cwd>` ⏎ `<session .jsonl path>` ⏎ `cwdstat <dev> <ino>` (+ optional `fresh` line for a not-yet-persisted new session). Written by `REt()` on every session start/switch. Verified rewritten immediately by `/new`, `/fork` (new file), `/move` (file renamed into the new project dir). Get the tty with `tmux -L omp-deck display -p -t <pane> '#{pane_tty}'`. Stale files from dead ptys linger — only trust the file for the tab's current pane tty, and require `mtime` ≥ pane start.
* Actions that change a tab's session file: `/new`, `/fork`, `/resume <id>`, `/move`, `/wt`, `/delete`, `/clear`? (no – same file), `/handoff` (same file), `/restart` (same). Re-read the breadcrumb after any of them.
* **Never run two omps on one session file**: resuming a session that is live in another process was accepted (V: second `omp --resume <live file>` started fine) — OmpDeck must refuse to open a session already open in a tab.
* A fresh session's jsonl does not contain `model_change`/`thinking_level_change` until the first message is persisted (V: file only had the header-time pair); switches done before the first prompt are still applied to the live process.

---------------------------------------------------------------------------------------------------
## 4b. Status-line data → source (what omp's footer shows vs. where OmpDeck gets it)
omp segments (valid ids, `omp config set statusLine.leftSegments`): `pi, status, model, mode, path, git, pr, subagents, token_in, token_out, token_total` (+ context bar, cost, collab).
| item | best source | fallback |
|---|---|---|
| model | collab `state.model.{provider,id,name}` | last `model_change` entry in jsonl |
| thinking | `state.thinkingLevel` | last `thinking_level_change.thinkingLevel` (`configured` = what the user asked: `auto` / level) |
| context % / window | `state.contextUsage` | last assistant `contextSnapshot.promptTokens` + `model.contextWindow` from `omp models --json` |
| busy / queued | `state.isStreaming`, `queuedMessageCount` / title glyph | |
| session name | `state.sessionName` / title OSC label | last `title_change` (jsonl) |
| cwd | `state.cwd` / breadcrumb line 1 | jsonl header `cwd` |
| tokens in/out, cost | sum `message.usage` over assistant entries (+`model_usage`) | |
| git branch / PR | run `git -C <cwd> rev-parse --abbrev-ref HEAD` (+`gh pr view` optionally) | |
| plan/goal mode | screen-scrape status segment (`🗺 Plan`, `⏸`) | not in state/jsonl [UNVERIFIED: no `mode` entry type seen in 588 messages sample] |
| subagents | collab `agents` frames + `bus` | |
| collab | `participants` | |

---------------------------------------------------------------------------------------------------
## 5. Non-interactive alternatives (CLI) and hidden flags

Launch flags relevant to session actions (`omp --help` 18.12.5 + bundle `cli/args.ts` flag table):
`--resume <id|path>` (`-r`), `--continue` (`-c`), **`--fork <id|path>` (hidden; absent from --help)** (+ `--cwd <dir>`), **`--restore <path>`** (resume + continue the interrupted turn and subagents), `--goal "<objective>"` (fresh interactive session in goal mode), `--model <pat>`, `--thinking off|minimal|low|medium|high|xhigh|max|auto`, `--models a,b,c` (Ctrl+P cycle list), `--smol/--slow/--plan <model>`, `--add-dir`, `--session-dir`, `--no-session`, `--no-title`, `--approval-mode`, `--config <overlay.yml>` (repeatable), `--export <session.jsonl>`, `--profile`, `--mode text|json|rpc|rpc-ui|acp`, `-p`. **No `--handoff` flag exists** (grepped args table: none); handoff/compact are slash commands only.
Subcommands: `omp session pack|unpack|moved` (omp-cli.md §C), `omp share <id|path> [--gist]` (network: uploads encrypted copy, prints link), `omp anonymize`, `omp collab list|link`, `omp join <link>`, `omp models`, `omp stats` (**writes ~/.omp/stats.db — avoid**), `omp usage`, `omp gc`, `omp update`.
* `omp --export <session.jsonl>` (V): writes `omp-session-<ts>_<id>.html` **into the process cwd**, prints `Exported to: <name>`, exits; does NOT open a browser (unlike `/export`). ~0.7 s. Use `current_dir(dest)` and then open the file yourself.
* `--model X --thinking Y` apply on `--resume` (V: resumed tab showed model Fake One with `◑` medium).
* `omp --fork <path> --cwd <dir>`: see omp-cli.md §C4 (new session id, original untouched).

---------------------------------------------------------------------------------------------------
## 6. Model dropdown + thinking-effort switching (verified)

### 6.1 Exact syntax (no picker)
* **`/switch <selector>`** — session-only model switch with argument (`slash-commands/builtin-modes.ts` `switch`: `Pd()` → `ma({cliModel, modelRegistry, availableModels, settings,…})`, then `session.setModelTemporary(model, thinkingLevel)`). Selector forms (all V unless noted): `fake/fake-2` (provider/id), `fake-2` (bare/fuzzy id), `fake/fake-1:high` (`:level` suffix sets thinking in the same step), `@role` (S: `@smol` etc.), fuzzy (`opus`, S). Result line `Session-only model: fake/fake-2. Use Alt+M or /model for roles.`
  * Not-found: `Error: Unknown model: <x>` (S). Model not authenticated: **`Error: No API key for nokey/n-1`** (V; `setModelTemporary` → `modelRegistry.hasConfiguredAuth` check) — nothing changes. In practice unauthenticated models are not even in the available list.
  * `:level` is clamped to the model's capability: `/switch fake/fake-1:max` on a model whose top level is `xhigh` set **`xhigh`** (V jsonl `"thinkingLevel":"xhigh","configured":"xhigh"`). Switching to a non-reasoning model records `thinkingLevel:null`. Switching to a model without `:level` re-applies the *default level of that model* (or keeps `auto`), not necessarily the previous level.
  * `/model <x>`: **in the TUI the argument is ignored and the full picker opens** (`handleTui` calls `showModelSelector()`; the arg-handling `handle` is ACP-only). So use `/switch`.
* **`/effort <level>`** — session thinking level (V). Valid values are exactly the list printed in the error: `off, auto, minimal, low, medium, high, xhigh` for a model whose `thinking.efforts` is `[minimal…xhigh]`; `max` only for models that list it (V: `Error: Unknown thinking level: max. Available: off, auto, minimal, low, medium, high, xhigh`). Non-reasoning model: `Error: fake/fake-2 has no adjustable thinking level.` for *any* argument (V). Success line `Thinking set to high.`; `auto` ⇒ jsonl `thinkingLevel:"high","configured":"auto"`; `off` ⇒ `thinkingLevel:"off","configured":"off"`. `/effort` with no arg opens a picker (`off, auto, min, low, medium, high, xhigh` with descriptions, V). Hotkey `Shift+Tab` (`ESC[Z`) cycles levels.
* Global levels vocabulary (`--thinking` and `config set defaultThinkingLevel`): `off|minimal|low|medium|high|xhigh|max|auto` (omp-cli.md; error text `Valid values: minimal, low, medium, high, xhigh, max, auto`).
* Bytes to inject (idle **or** busy — both V): `ESC[200~/switch anthropic/claude-opus-5-5:high ESC[201~` `0d`; `ESC[200~/effort low ESC[201~` `0d`. Prefix with `0x03`(+650 ms) if the composer may hold text. If a popup swallows Enter (model-id completion), add `0x1b` before `0d` **only when idle**. `\x1b[Z` (Shift+Tab) as a keyboard-only fallback for "next level".
* **Mid-stream switching is allowed** (V): `/switch fake/fake-2` and `/effort low` executed during a running SLOW turn, were persisted immediately (jsonl `model_change` 13:15:33 while the turn was still streaming), the status line/title/`state` changed at once, the in-flight request was NOT aborted (stream completed on the old model's connection); the *next* request uses the new model (`flushPendingModelSwitch` runs at turn end for deferred cases). `/effort` mid-stream on a no-thinking model just errors.

### 6.2 Persistence / confirmation
Session jsonl entries (V, `session-manager.appendModelChange / appendThinkingLevelChange`):
```
{"type":"model_change","id":…,"parentId":…,"timestamp":"…Z","runtime":{"version":"18.12.5"},"model":"fake/fake-2","role":"temporary","resolvedModelIsFallback":false}
{"type":"thinking_level_change",…,"thinkingLevel":"high"|"off"|null,"configured":"high"|"auto"|"off"|null}
```
`role:"temporary"` = `/switch`/Alt+P (session-only; global `modelRoles.default` NOT changed); the initial pair at session start has no `role`; `/model` picker → `role:"default"` etc. (persisting; S). `thinkingLevel` = effective level, `configured` = what was requested (`auto`). A `/effort` with no change appends nothing. The same facts are pushed live in the collab `state` frame (`model`, `thinkingLevel`), so the status bar can confirm within ~0.2 s without file I/O; jsonl confirmation lags only by the append flush. NOTE: entries are not written to disk until the session has its first persisted message.

### 6.3 Listing choices
`omp models --json` (V, ~0.75 s in sandbox; ~1 s on the real catalog per omp-cli.md; run with `current_dir($HOME)` and the same `PI_CODING_AGENT_DIR`/profile as the tab):
```
{"models":[{"provider":"fake","kind":"chat","id":"fake-1","selector":"fake/fake-1","name":"Fake One","contextWindow":100000,"maxTokens":4096,
            "reasoning":true,"thinking":["minimal","low","medium","high","xhigh"],"input":["text"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"pricingStatus":"unknown"},
           {…"id":"fake-2","reasoning":false,"thinking":null…}]}
```
* **Only authenticated ("available") chat models are listed** — a provider whose key/OAuth is missing is omitted (V: `nokey/n-1` absent; `--kind all` adds search/stt/tts/tiny kinds). So no `available` flag is needed; presence = usable. `thinking` = ordered effort levels the model supports (`null` for non-reasoning); offer `off` + `auto` + that list in the dropdown (matches `/effort`'s Available list; `max` appears in that array for models that support it). `selector` is the string to put after `/switch `. `omp models refresh` forces a network catalog fetch (avoid).
* Related settings (via `omp config get/list --json`, omp-cli.md §A): `modelRoles` (record: `default`, `smol`, `slow`, `task`, `plan`… values like `anthropic/claude-opus-5-5:high`, i.e. same selector+`:level` syntax as `/switch`), `enabledModels` (array; narrows the picker/available set, `getAvailableModels` applies it), `--models`/`cycleModels` (Ctrl+P list = `scopedModels`), `defaultThinkingLevel`. Recommended dropdown: group by provider; mark role models (`@default`, `@smol`…) from `modelRoles`; apply `enabledModels` filter if non-empty; show `contextWindow` and cost per M.
* Current model/level for the dropdown header: `state.model`/`state.thinkingLevel` (collab) else last `model_change`/`thinking_level_change` in jsonl.

---------------------------------------------------------------------------------------------------
## 7. Recommendation per button

Notation: `INJ(x)` = injector of §3.7 with command text `x`; `IDLE` = require idle (title glyph `>` / `isStreaming=false`).

| button | mechanism | precondition | exact bytes / argv |
|---|---|---|---|
| **Model ▾ (pick)** | slash injection | any state (mid-stream OK, applies to next request) | `INJ("/switch <provider/id>[:level]")` |
| **Thinking ▾** | slash injection (options from `omp models` `thinking` + `off`,`auto`) | any state; model must be reasoning | `INJ("/effort <level>")`; next-level: `ESC[Z` |
| Model picker (omp UI) | hotkey | any | `ESC p` (Alt+P, temp) / `ESC m` (Alt+M, roles) |
| **Plan mode** | hotkey (cycles on/pause/off) | any | `ESC P` (Alt+Shift+P) — or `INJ("/plan")` |
| **Compact** | slash injection | IDLE (mid-stream aborts the turn) | `INJ("/compact")`, variants `/compact soft`, `/compact snapcompact`, `/compact <focus>` |
| **Handoff** | slash injection | IDLE, ≥2 messages | `INJ("/handoff [focus]")` (progress loader; same file) |
| **Fork (new tab)** | CLI in new OmpDeck tab | source may be live | argv `omp --fork <abs jsonl> --cwd <dir>` (omp-cli.md §C4) — preferred over `/fork window` |
| Fork in place | slash | IDLE | `INJ("/fork")` then re-read breadcrumb |
| **New session** | slash (same tab) | any, but switches file | `INJ("/new")`; re-read breadcrumb; re-join collab (new generation) |
| Clear context | slash | IDLE, confirm | `INJ("/clear")` |
| **Rename/title** | slash, or edit jsonl via omp only | any | `INJ("/rename <title>")` (no arg = model-generated title; costs a model call) |
| **Move to dir** | slash (live) or pack/unpack (closed tab) | IDLE, dir exists | `INJ_PATH("/move <abs dir>")` (needs the Esc step; no trailing slash) |
| **Resume other** | OmpDeck's own sidebar → open tab with `omp --resume <path>` | – | (in-place `/resume <id>` possible but moves the tab's session) |
| **Tree / Branch** | slash or hotkey | IDLE (overlay) | `INJ("/tree")`, `INJ("/branch")` |
| **Agent hub** | hotkey | any | `ESC a` |
| **Retry** | hotkey | IDLE after failure | `ESC[15~` (F5) or `ESC r` (Alt+R) |
| Abort turn | **collab `{"t":"abort"}`** (clean) or Esc | busy | collab frame; else `0x1b` (only if composer has no popup; Esc on empty composer twice <500 ms opens tree) |
| Queue message | collab `prompt` (steers) or paste+`Ctrl+Q` (`0x11`) for follow-up | busy | |
| Send prompt text | collab `prompt` (no composer disturbance) | any | `{"t":"prompt","text":…}` |
| **Export HTML** | CLI | – | `omp --export <jsonl>` with `current_dir(dest)`; then open file |
| Copy last message | OmpDeck reads last assistant `message` from jsonl and copies itself | – | (`/copy` is a picker; `/dump` overwrites clipboard with the whole transcript) |
| **Share** | CLI `omp share <jsonl>` (stdout link; network, ask user) or `INJ("/share")` | – | |
| Context breakdown | slash | any | `INJ("/context")` (overlay; Esc closes) |
| Collab host/stop | not needed (autoStart) | | |

---------------------------------------------------------------------------------------------------
## 8. Open points / UNVERIFIED
* Registry socket ops (`op:"snapshot"`) described from source, not hand-driven; the CLI (`omp collab list/link --json`) is verified.
* Title glyph `!` (attention) trigger and `terminalTitle` style variants not exercised.
* `/handoff` success path not exercised (sandbox session below the minimum size); `/clear`, `/delete`, `/export`, `/dump`, `/share`, `/copy`, `/resume <id>`, `/fork pane|window` not run (side effects: clipboard/browser/network/tmux windows). Mode/overlay labels for those are from source.
* CSI-u/kitty-encoded forms of Alt-hotkeys not tested; legacy `ESC <key>` verified.
* A resumed-while-live duplicate session did not error in my test; real-world conflict behaviour (`session-owners` lock under `<baseRoot>/run/session-owners`) not explored.
* Early sandbox runs (before I added `PI_CONFIG_DIR`) may have started shared helper daemons under `~/.omp/run/daemons` (text-predict etc.); real `~/.omp/agent/**` was never written by me (reads only: config.yml, terminal-sessions, a session jsonl, collab list).

## 9. Cleanup performed
`tmux -L omp-deck-test-act kill-server` + removed `/tmp/tmux-1000/omp-deck-test-act`; killed the fake provider (18777) and relay (7567); removed `/tmp/ompact` (sandbox agent/config dirs, scripts, exported HTML); the strings dump `/tmp/omp-strings.txt` removed at the end.
