// ompdeck-ctl.ts — OmpDeck control extension (protocol v:1).
// Load with: omp -e <path>. Unix: env OMPDECK_CTL_DIR selects the socket dir (Bun.listen on <dir>/ctl.sock).
// Windows: env OMPDECK_CTL_PIPE is the full named-pipe name (node:net); OmpDeck computes it, it is never derived here.
// Inert when the variable of the OS is unset.
// Any failure (version drift of omp internals, I/O) degrades to "no socket" or an error reply; it never throws into omp.
import * as fs from "node:fs";
import * as crypto from "node:crypto";
import * as net from "node:net";
import * as path from "node:path";

const IS_WIN = process.platform === "win32";
const DIR = IS_WIN ? undefined : process.env.OMPDECK_CTL_DIR;
// A named pipe has no owner/mode check, so a connection must prove it knows OMPDECK_CTL_TOKEN (derived by OmpDeck from
// a private per-user secret) before it is sent anything or heard.
const TOKEN = IS_WIN ? process.env.OMPDECK_CTL_TOKEN : undefined;
const PIPE = IS_WIN && TOKEN ? process.env.OMPDECK_CTL_PIPE : undefined;
const LOG = process.env.OMPDECK_CTL_LOG; // debug only
const log = (s: string) => {
  if (!LOG) return;
  try { fs.appendFileSync(LOG, `${new Date().toISOString()} ${s}\n`); } catch {}
};
const G = globalThis as any; // process-wide: the factory runs for the main session and for every subagent session

export default async function ompdeckCtl(api: any) {
  if (!DIR && !PIPE) return; // not launched by OmpDeck -> inert
  try {
    // Internal modules are bundled in the omp binary; literal specifiers only (non-literal dynamic import fails).
    let AgentLifecycleManager: any, AgentRegistry: any, sessionFileBelongsToRoot: any, SessionManager: any;
    try {
      ({ AgentLifecycleManager } = await import("@oh-my-pi/pi-coding-agent/registry/agent-lifecycle"));
      ({ AgentRegistry } = await import("@oh-my-pi/pi-coding-agent/registry/agent-registry"));
      ({ sessionFileBelongsToRoot } = await import("@oh-my-pi/pi-coding-agent/registry/persisted-agents"));
    } catch (e) { log("internal import failed: " + e); return; }
    // Optional: only fork_at needs it; without it that op answers with an error.
    try { ({ SessionManager } = await import("@oh-my-pi/pi-coding-agent/session/session-manager")); } catch (e) { log("session-manager import failed: " + e); }
    // Optional: the /context breakdown. Literal specifiers; a missing module just omits `context` from snapshots.
    let contextRuntime: any, contextUsage: any;
    try { contextRuntime = await import("@oh-my-pi/pi-coding-agent/session/context-usage-runtime"); } catch (e) { log("context-usage-runtime import failed: " + e); }
    try { contextUsage = await import("@oh-my-pi/pi-tui/status-line/context-usage"); } catch (e) { log("context-usage import failed: " + e); }
    if (!AgentLifecycleManager || !AgentRegistry || typeof sessionFileBelongsToRoot !== "function") { log("internal API missing"); return; }

    const isMain = (ctx: any) => ctx?.agent?.kind === "main";
    api.on("session_start", (_ev: any, ctx: any) => { try { if (isMain(ctx)) start(ctx); } catch (e) { log("start failed: " + e); } });
    api.on("session_switch", (ev: any, ctx: any) => { try { if (isMain(ctx)) { log(`session_switch ${ev?.reason}`); G.__ompdeckCtl?.setCtx(ctx); G.__ompdeckCtl?.recompute(); } } catch (e) { log("switch failed: " + e); } });
    // Context numbers move on these; `recompute` is cheap (cached per message) and never throws.
    for (const name of ["agent_end", "turn_end", "model_select", "session_compact"]) {
      try { api.on(name, (_ev: any, ctx: any) => { try { if (isMain(ctx)) G.__ompdeckCtl?.recompute(); } catch (e) { log(name + " failed: " + e); } }); } catch (e) { log("on " + name + " failed: " + e); }
    }
    api.on("session_shutdown", (_ev: any, ctx: any) => { try { if (isMain(ctx)) stop(); } catch (e) { log("stop failed: " + e); } });

    function start(ctx: any) {
      if (G.__ompdeckCtl) { G.__ompdeckCtl.setCtx(ctx); G.__ompdeckCtl.recompute(); return; }
      const registry = AgentRegistry.global();
      const clients = new Set<any>();
      let mainCtx = ctx;
      let pending: any;
      const rootFile = (): string | null => { try { return mainCtx.sessionManager.getSessionFile?.() ?? null; } catch { return null; } };
      const mainIdle = (): boolean | null => { try { return typeof mainCtx.isIdle === "function" ? !!mainCtx.isIdle() : null; } catch { return null; } };
      const mainLeaf = (): string | null => { try { return mainCtx.sessionManager.getLeafId?.() ?? null; } catch { return null; } };

      // ---- /context breakdown (omp's computeSessionContextBreakdown + per-tool schema tokens), cached ----
      let contextCache: any = null;
      let contextKey = "";
      const toolTokenCaches = new WeakMap<object, WeakMap<object, number>>(); // tokenizer -> tool -> tokens
      const mainSession = (): any => { try { return registry.list().find((r: any) => r.kind === "main")?.session; } catch { return undefined; } };
      const modelKey = (): string => { const m = mainSession()?.model; return m ? `${m.provider}/${m.id}` : ""; };
      // The main session's in-memory model and thinking level: the file records them only once the session is persisted (first message).
      const mainModel = (): { selector: string; thinkingLevel: string | null; configured: string | null } | null => {
        try {
          const sess = mainSession();
          const key = modelKey();
          if (!sess || !key) return null;
          const level = sess.thinkingLevel;
          const configured = typeof sess.configuredThinkingLevel === "function" ? sess.configuredThinkingLevel() : null;
          return {
            selector: key,
            thinkingLevel: typeof level === "string" ? level : null,
            configured: typeof configured === "string" ? configured : null,
          };
        } catch (e) { log("model state failed: " + e); return null; }
      };
      // The OAuth account the main session uses for its model's provider: the session's sticky credential, else omp's
      // default pick (`authStorage.oauth.identity`, what omp's own status line shows). Undefined for api-key providers.
      // `pinned`: the sticky record is explicit (`/session pin`); `credentials`: the stored accounts it can be pinned to.
      type Identity = { email: string | null; accountId: string | null; projectId: string | null; orgId: string | null };
      type Account = Identity & { provider: string; pinned: boolean; credentials: (Identity & { credentialId: string })[] };
      const idStr = (v: unknown): string | null => (typeof v === "string" && v ? v : null);
      const identityOf = (v: any): Identity => ({ email: idStr(v?.email), accountId: idStr(v?.accountId), projectId: idStr(v?.projectId), orgId: idStr(v?.orgId) });
      const providerAccounts = (auth: any, provider: string, sessionId: string): any[] => {
        const list = typeof auth?.oauth?.accounts === "function" ? auth.oauth.accounts(provider, sessionId) : [];
        return (Array.isArray(list) ? list : []).filter((a: any) => (typeof a?.credentialId === "string" && a.credentialId) || typeof a?.credentialId === "number");
      };
      const mainAccount = (): Account | null => {
        try {
          const sess = mainSession();
          const provider = sess?.model?.provider;
          const auth = sess?.modelRegistry?.authStorage;
          if (typeof provider !== "string" || !provider || typeof auth?.oauth?.identity !== "function") return null;
          const id = auth.oauth.identity(provider, sess.sessionId);
          if (!id) return null;
          let pinned = false;
          try { const sticky = auth.sessions?.get?.(provider, sess.sessionId); pinned = sticky?.type === "oauth" && sticky.explicit === true; } catch {}
          let credentials: Account["credentials"] = [];
          try {
            // credentialId is omp's numeric row id: sent as a string, mapped back by pin_account.
            credentials = providerAccounts(auth, provider, sess.sessionId)
              .map((a: any) => ({ credentialId: String(a.credentialId), ...identityOf(a) }));
          } catch (e) { log("account list failed: " + e); }
          return { provider, ...identityOf(id), pinned, credentials };
        } catch (e) { log("account state failed: " + e); return null; }
      };
      const perToolTokens = (sess: any): [string, number][] => {
        const tokenizer = sess.agent?.tokenizer;
        const tools: any[] = sess.agent?.state?.tools ?? [];
        if (!tokenizer || typeof tokenizer.countTokens !== "function" || sess.model?.toolSchemasInContext === false) return [];
        let toolTokens = toolTokenCaches.get(tokenizer);
        if (!toolTokens) { toolTokens = new WeakMap(); toolTokenCaches.set(tokenizer, toolTokens); }
        const est = contextUsage?.estimateToolSchemaTokens;
        const out: [string, number][] = [];
        for (const t of tools) {
          if (typeof t?.name !== "string") continue;
          let n = toolTokens.get(t);
          if (n === undefined) {
            try {
              n = typeof est === "function"
                ? est([t], tokenizer, sess.settings?.revision ?? 0)
                : tokenizer.countTokens([t.name, t.description ?? "", JSON.stringify(t.parameters ?? {})]);
            } catch { n = undefined; }
            if (typeof n === "number" && Number.isFinite(n)) toolTokens.set(t, n);
          }
          if (typeof n === "number" && Number.isFinite(n)) out.push([t.name, Math.max(0, Math.round(n))]);
        }
        return out.sort((a, b) => b[1] - a[1]);
      };
      const recompute = () => {
        try {
          const sess = mainSession();
          contextKey = modelKey();
          if (!sess || typeof contextRuntime?.computeSessionContextBreakdown !== "function") { contextCache = null; return; }
          const b = contextRuntime.computeSessionContextBreakdown(sess, { snapcompactSavings: true });
          if (!b || !(b.contextWindow > 0)) { contextCache = null; return; }
          let anchored = false;
          try { anchored = !!sess.getContextBreakdown?.()?.anchored; } catch {}
          const saved = b.snapcompact?.savedTokens;
          contextCache = {
            model: b.model?.name ?? b.model?.id ?? null,
            contextWindow: b.contextWindow, usedTokens: b.usedTokens, anchored,
            categories: (b.categories ?? []).map((c: any) => ({ id: c.id, label: c.label, tokens: c.tokens })),
            autoCompactBuffer: b.autoCompactBufferTokens ?? 0, freeTokens: b.freeTokens ?? 0,
            thresholdTokens: b.thresholdTokens ?? null,
            snapcompactSaved: typeof saved === "number" && saved > 0 ? saved : null,
            tools: perToolTokens(sess).map(([name, tokens]) => ({ name, tokens })),
            skills: Array.isArray(sess.skills) ? sess.skills.length : 0,
            computedAt: Date.now(),
          };
        } catch (e) { log("context compute failed: " + e); contextCache = null; }
        schedule();
      };

      const snapshot = () => {
        const root = rootFile();
        let agents: any[] = [];
        try {
          // The registry is process-global and keeps agents of earlier sessions (/new, /resume): scope to the current root like the Hub.
          agents = registry.list()
            .filter((r: any) => r.kind !== "main" && r.sessionFile && root && sessionFileBelongsToRoot(r.sessionFile, root))
            .map((r: any) => ({
              id: r.id, name: r.displayName, kind: r.kind, parent: r.parentId ?? null, status: r.status, // running|idle|parked|aborted
              activity: r.activity ?? null, live: !!r.session, streaming: r.session?.isStreaming ?? null,
              createdAt: r.createdAt, lastActivity: r.lastActivity, sessionFile: r.sessionFile,
              model: r.history?.resolvedModel ?? null,
            }));
        } catch (e) { log("snapshot failed: " + e); }
        return { t: "agents", pid: process.pid, sessionFile: root, mainIdle: mainIdle(), leaf: mainLeaf(), agents, context: contextCache, model: mainModel(), account: mainAccount(), pendingUi: [...pendingUi.values()].map(p => p.summary) };
      };
      const write = (c: any, l: string) => { try { c.write(l); } catch {} };
      const push = (m: unknown) => {
        if (!clients.size) return;
        let l: string; try { l = JSON.stringify(m) + "\n"; } catch { return; }
        for (const c of clients) write(c, l);
      };
      const schedule = () => { if (pending) return; pending = setTimeout(() => { pending = undefined; push(snapshot()); }, 50); };

      // ---- structured UI requests: omp's dialogs (ask, approvals, select/confirm/input/editor) raced against a remote answer ----
      // `ctx.ui` is one object shared by the TUI and the tools, so wrapping its members in place sees every request. The original (TUI) call
      // and the remote answer race; the loser is aborted through the merged signal, so a phone answer closes the TUI dialog.
      // Internal omp API: a member that is not a function is skipped and `uiHook` stays false (the gateway then lists no requests).
      const uiState = (G.__ompdeckUi ??= { pending: new Map<number, { summary: any; resolve: (v: any) => void }>(), seq: 0, hook: false, onChange: () => {} });
      const pendingUi: Map<number, { summary: any; resolve: (v: any) => void }> = uiState.pending;
      uiState.onChange = schedule;
      const str = (v: any, max = 16384): string | undefined => typeof v === "string" ? (v.length > max ? v.slice(0, max) : v) : undefined;
      const opt = (o: any) => {
        const label = str(typeof o === "string" ? o : o?.label); if (label === undefined) return undefined;
        const description = typeof o === "object" ? str(o?.description) : undefined;
        return description === undefined ? { label } : { label, description };
      };
      const opts = (list: any) => (Array.isArray(list) ? list : []).slice(0, 100).map(opt).filter((o: any) => o);
      const summarize = (kind: string, a: any[]): any => {
        const o = a[kind === "askDialog" || kind === "approve" ? 1 : 2];
        const timeoutMs = typeof o?.timeout === "number" && Number.isFinite(o.timeout) && o.timeout > 0 ? Math.round(o.timeout) : undefined;
        switch (kind) {
          case "select": return { kind: "select", title: str(a[0]) ?? "", options: opts(a[1]), multi: o?.selectionMarker === "checkbox", initial: Number.isInteger(o?.initialIndex) ? o.initialIndex : undefined, timeoutMs };
          case "confirm": return { kind: "confirm", title: str(a[0]) ?? "", message: str(a[1]), timeoutMs };
          case "input": return { kind: "input", title: str(a[0]) ?? "", message: str(a[1]), timeoutMs };
          case "editor": return { kind: "editor", title: str(a[0]) ?? "", prefill: str(a[1], 65536), timeoutMs };
          case "askDialog": return {
            kind: "ask", title: "Ask", timeoutMs,
            questions: (Array.isArray(a[0]) ? a[0] : []).slice(0, 20).map((q: any) => ({
              id: String(q?.id ?? ""), question: str(q?.question) ?? "", header: str(q?.header), options: opts(q?.options), multi: !!q?.multi,
              recommended: Number.isInteger(q?.recommended) ? q.recommended : undefined,
            })),
          };
          default: { // approve
            const p = a[0] ?? {};
            return {
              kind: "approve", title: "Approval", timeoutMs,
              approve: {
                full: str(p.full) ?? "", compact: str(p.compact) ?? "", session: str(p.session),
                grants: (Array.isArray(p.grants) ? p.grants : []).slice(0, 20).map((g: any) => ({ scope: String(g?.scope ?? ""), label: str(g?.label) ?? "", alwaysLabel: str(g?.alwaysLabel) ?? "" })),
              },
            };
          }
        }
      };
      const wrapUi = (ui: any) => {
        if (!ui || typeof ui !== "object") return;
        const MARK = Symbol.for("ompdeck.wrapped");
        if (ui[MARK]) return; // wrapped by an earlier start() of this process; its closures share `uiState`
        ui[MARK] = true;
        const sigAt: Record<string, number> = { select: 2, confirm: 2, input: 2, editor: 2, askDialog: 1, approve: 1 };
        let wrapped = 0;
        for (const [m, at] of Object.entries(sigAt)) {
          const orig = ui[m];
          if (typeof orig !== "function") { log(`ui.${m} is not a function: not hooked`); continue; }
          ui[m] = (...args: any[]) => {
            const id = ++uiState.seq, ac = new AbortController();
            const call = args.slice();
            while (call.length <= at) call.push(undefined);
            const o = { ...(call[at] ?? {}) };
            o.signal = o.signal ? AbortSignal.any([o.signal, ac.signal]) : ac.signal;
            call[at] = o;
            const { promise: remoteP, resolve: remote } = Promise.withResolvers<any>();
            try { pendingUi.set(id, { summary: { id, ...summarize(m, args) }, resolve: remote }); } catch (e) { log("ui summarize failed: " + e); }
            uiState.onChange();
            return Promise.race([new Promise<any>(r => r(orig.apply(ui, call))), remoteP]).finally(() => { pendingUi.delete(id); ac.abort(); uiState.onChange(); });
          };
          wrapped++;
        }
        // A partial wrap would hide the requests of the skipped members, so only a complete one reports a hook.
        uiState.hook = wrapped === Object.keys(sigAt).length;
      };

      // fork_at: omp's AgentSession.#zr: a cut at a tool-calling assistant (or inside its batch) extends over the matching toolResults so no call is left unpaired.
      function forkCut(sm: any, id: string): string {
        const answered = new Set<string>();
        let assistant: any;
        const branch: any[] = sm.getBranch(id);
        for (let i = branch.length - 1; i >= 0; i--) {
          const e = branch[i];
          if (e.type !== "message") continue;
          if (e.message.role === "toolResult") { answered.add(e.message.toolCallId); continue; }
          if (e.message.role === "assistant") assistant = e.message;
          break;
        }
        const pending = new Set<string>();
        for (const b of assistant?.content ?? []) if (b.type === "toolCall" && !answered.has(b.id)) pending.add(b.id);
        let cut = id, cur = id;
        while (pending.size > 0) {
          const kids: any[] = sm.getChildren(cur);
          const hit = kids.findLast((k: any) => k.type === "message" && k.message.role === "toolResult" && pending.has(k.message.toolCallId));
          if (hit) { pending.delete(hit.message.toolCallId); cut = cur = hit.id; continue; }
          if (kids.length !== 1 || kids[0].type === "message") break;
          cur = kids[0].id;
        }
        return cut;
      }

      const sameRoot = (cmd: any): boolean => { const want = rootFile(); return typeof cmd.root === "string" && !!want && cmd.root === want; };
      const commandContext = (): any => registry.get("Main")?.session?.extensionRunner?.createCommandContext?.();
      // Redraws omp's transcript after a session-level tree move (the command context does the drawing, but only for moves it
      // makes itself). A move that appended an entry (branch summary, re-answered ask) leaves the leaf on the file's last entry:
      // reload rebuilds the screen from it. A bare move is in memory only: undo it and replay it through the command context.
      async function redraw(oldLeaf: string | null, id: string, moved: any): Promise<void> {
        const cc = commandContext();
        if (moved?.summaryEntry || moved?.askReanswerCommitted) {
          if (typeof cc?.reload !== "function") throw new Error("redraw unavailable");
          await cc.reload();
          if (moved.editorText && typeof mainCtx.ui?.setEditorText === "function" && !String(mainCtx.ui.getEditorText?.() ?? "").trim()) mainCtx.ui.setEditorText(moved.editorText);
        } else {
          if (oldLeaf === null || typeof mainCtx.sessionManager.branch !== "function") throw new Error("redraw unavailable");
          mainCtx.sessionManager.branch(oldLeaf);
          await cc.navigateTree(id, { summarize: false });
        }
      }
      // == the /tree selector's "re-answer ask": omp's own ask dialog opens in the tab; the new answer becomes a sibling result
      // and the turn resumes. Resolves once that happened (or the dialog was cancelled), however long the user takes.
      async function reanswerAsk(cmd: any): Promise<any> {
        if (!sameRoot(cmd)) return { ok: false, error: "session changed" };
        if (mainIdle() !== true) return { ok: false, error: "busy" };
        const sm = mainCtx.sessionManager;
        const sess = mainSession();
        const id = String(cmd.entryId ?? "");
        const entry = sm.getEntry?.(id);
        if (!entry) return { ok: false, error: "no such entry" };
        if (entry.type !== "message" || entry.message?.role !== "toolResult" || entry.message?.toolName !== "ask") return { ok: false, error: "not an ask result" };
        if (typeof sess?.navigateTree !== "function" || typeof sess.buildAskReanswerContext !== "function") return { ok: false, error: "re-answering unavailable" };
        let AskTool: any, ToolAbortError: any;
        try {
          ({ AskTool } = await import("@oh-my-pi/pi-coding-agent/tools/ask"));
          ({ ToolAbortError } = await import("@oh-my-pi/pi-coding-agent/tools/tool-errors"));
        } catch (e) { log("ask import failed: " + e); return { ok: false, error: "re-answering unavailable" }; }
        const oldLeaf = mainLeaf();
        const probe = await sess.navigateTree(id, { allowAskReopen: true, requireIdle: true });
        if (probe?.cancelled) return { ok: false, error: "navigation cancelled" };
        if (!probe?.reopenAsk) {
          // omp could not recover the original questions and made the plain leaf move onto the result.
          await redraw(oldLeaf, id, probe);
          schedule();
          return { ok: true, leaf: mainLeaf(), message: "The original questions are unavailable: moved to the answer instead." };
        }
        const toolSession = { cwd: sm.getCwd(), hasUI: true, settings: sess.settings, getSessionFile: () => sm.getSessionFile() ?? null, getSessionSpawns: () => null, getPlanModeState: () => sess.getPlanModeState?.() };
        let answer: any;
        try {
          answer = await new AskTool(toolSession).execute("tree-reanswer", { questions: probe.reopenAsk.questions }, undefined, undefined, sess.buildAskReanswerContext(mainCtx.ui));
        } catch (e: any) {
          if (e instanceof ToolAbortError) return { ok: false, error: "cancelled" };
          throw e;
        }
        if (answer?.details?.chatRedirect) return { ok: false, error: "Chat about this is not available when re-answering from the tree" };
        const done = await sess.navigateTree(id, { allowAskReopen: true, reanswerAskResult: answer, requireIdle: true });
        if (done?.cancelled) return { ok: false, error: "navigation cancelled" };
        await redraw(oldLeaf, id, done);
        if (done.askReanswerCommitted) sess.resumeAfterAskReanswer();
        schedule();
        return { ok: true, leaf: mainLeaf() };
      }
      // == `/advisor configure`: read or save one WATCHDOG.yml, then apply the merged roster to the running session.
      async function advisorConfig(cmd: any): Promise<any> {
        if (typeof cmd.root === "string" && !sameRoot(cmd)) return { ok: false, error: "session changed" };
        const scope = cmd.scope === "user" ? "user" : cmd.scope === "project" ? "project" : null;
        if (!scope) return { ok: false, error: "unknown scope" };
        let adv: any;
        try { adv = await import("./ompdeck-advisor.ts"); } catch (e) { log("advisor module failed: " + e); return { ok: false, error: "advisor configuration unavailable" }; }
        const dirs = adv.dirsFor(mainCtx.sessionManager.getCwd());
        const file = adv.resolveEditPath(scope, dirs);
        if (cmd.doc === null || cmd.doc === undefined) return { ok: true, value: { path: file, doc: await adv.loadDoc(file) } };
        const doc = cmd.doc;
        if (typeof doc !== "object" || !Array.isArray(doc.advisors)) return { ok: false, error: "doc.advisors must be a list" };
        const removed = await adv.saveDoc(file, doc);
        let active: number | null = null;
        const sess = mainSession();
        if (typeof sess?.applyAdvisorConfigs === "function") {
          const found = await adv.discover(mainCtx.sessionManager.getCwd(), dirs.agentDir);
          active = sess.applyAdvisorConfigs(found.advisors, found.sharedInstructions, found.sharedMaxNotesPerUpdate);
        }
        schedule();
        const what = removed ? `Removed ${scope} WATCHDOG.yml` : `Saved ${scope} WATCHDOG.yml`;
        return { ok: true, value: { path: file, doc: await adv.loadDoc(file), removed, active }, message: active === null ? what : `${what} — ${active} advisor${active === 1 ? "" : "s"} active.` };
      }
      // == omp's task mode of the model picker: the Task subagent model, layered onto the runtime settings (never persisted).
      async function taskModelOverride(cmd: any): Promise<any> {
        if (typeof cmd.root === "string" && !sameRoot(cmd)) return { ok: false, error: "session changed" };
        const sess = mainSession();
        if (!sess?.settings) return { ok: false, error: "settings unavailable" };
        let task: any;
        try { task = await import("@oh-my-pi/pi-coding-agent/task/settings"); } catch (e) { log("task settings failed: " + e); return { ok: false, error: "task model override unavailable" }; }
        const setting = task.cfgTaskAgentModelOverrides;
        if (typeof cmd.model === "string" && cmd.model) {
          if (!/^[^/\s]+\/\S+$/.test(cmd.model)) return { ok: false, error: "model must be provider/id" };
          setting.override(sess.settings, { ...setting.get(sess.settings), task: cmd.model });
        } else {
          setting.override(sess.settings, undefined); // drop the runtime layer: the configured value applies again
        }
        const now = setting.get(sess.settings)?.task;
        return { ok: true, value: Array.isArray(now) ? now[0] ?? null : now ?? null };
      }

      // omp never forwards model_changed / thinking_level_changed to extensions: after a change, refresh the cached context
      // breakdown (it is keyed by model) and push a snapshot ourselves.
      const refreshNow = () => { try { recompute(); } catch {} try { push(snapshot()); } catch {} };
      // == TUI `/switch <selector>` (builtin-modes.ts + selector-controller.ts switchSessionModel/#applySessionModel), minus the UI
      // notice. Not gated on idle, like the slash command. The compact-first branch is the caller's: over context -> needs_compact.
      // Both routes lean on omp internals: anything missing or unexpected answers "unsupported" before changing anything,
      // so the gateway falls back to the typed /switch / /effort.
      const UNSUPPORTED = { ok: false, error: "unsupported" };
      async function switchModel(cmd: any): Promise<any> {
        const sess = mainSession();
        const selector = typeof cmd.selector === "string" ? cmd.selector.trim() : "";
        if (!sess || !selector) return { ok: false, error: "no model selector" };
        let resolver: any;
        try { resolver = await import("@oh-my-pi/pi-coding-agent/config/model-resolver"); } catch (e) { log("model-resolver import failed: " + e); return UNSUPPORTED; }
        const fn = (o: any, k: string) => typeof o?.[k] === "function";
        if (!fn(resolver, "resolveCliModel") || !fn(resolver, "getModelMatchPreferences") || !fn(sess, "setModelTemporary")
          || !fn(sess, "resolveTemporaryModelThinkingLevel") || !fn(sess, "getContextUsage") || !sess.modelRegistry || !sess.settings) {
          log("switch_model: an omp internal is missing");
          return UNSUPPORTED;
        }
        let resolved: any;
        try {
          const scoped = (Array.isArray(sess.scopedModels) ? sess.scopedModels : []).map((s: any) => s.model ?? s).filter(Boolean);
          resolved = resolver.resolveCliModel({
            cliModel: selector, modelRegistry: sess.modelRegistry, availableModels: scoped.length > 0 ? scoped : undefined,
            settings: sess.settings, preferences: resolver.getModelMatchPreferences(sess.settings),
          });
        } catch (e) { log("resolveCliModel failed: " + e); return UNSUPPORTED; }
        if (!resolved || typeof resolved !== "object" || !("model" in resolved)) return UNSUPPORTED;
        const model = resolved.model;
        if (!model) return { ok: false, error: `Unknown model: ${selector}` };
        if (typeof model.provider !== "string" || typeof model.id !== "string") return UNSUPPORTED;
        // Exactly selector-controller.ts switchSessionModel: `contextWindow ?? 0`, `getContextUsage()?.tokens ?? 0` (unknown
        // usage, e.g. before the first reply, counts as 0); only a present non-number value is drift.
        const window = model.contextWindow ?? 0;
        if (typeof window !== "number" || Number.isNaN(window)) return UNSUPPORTED;
        if (window > 0) {
          let tokens: unknown;
          try { tokens = sess.getContextUsage()?.tokens ?? 0; } catch (e) { log("getContextUsage failed: " + e); return UNSUPPORTED; }
          if (typeof tokens !== "number" || Number.isNaN(tokens)) return UNSUPPORTED;
          if (tokens > window) return { ok: false, error: "needs_compact" };
        }
        let level: any;
        try { level = resolved.thinkingLevel ?? sess.resolveTemporaryModelThinkingLevel(model); } catch (e) { log("thinking level lookup failed: " + e); return UNSUPPORTED; }
        try { await sess.setModelTemporary(model, level); } catch (e: any) { return { ok: false, error: String(e?.message ?? e) }; }
        refreshNow();
        const key = `${model.provider}/${model.id}`;
        const note = `Session-only model: ${key}`;
        return { ok: true, value: { model: key, thinking: typeof sess.thinkingLevel === "string" ? sess.thinkingLevel : null }, message: resolved.warning ? `${note} (${resolved.warning})` : note };
      }
      // == TUI `/effort <level>`: resolveThinkingArgument + session.setThinkingLevel (not persisted).
      async function setEffort(cmd: any): Promise<any> {
        const sess = mainSession();
        if (!sess) return { ok: false, error: "no session" };
        let helper: any;
        try { helper = await import("@oh-my-pi/pi-coding-agent/slash-commands/helpers/effort"); } catch (e) { log("effort import failed: " + e); return UNSUPPORTED; }
        if (typeof helper?.resolveThinkingArgument !== "function" || typeof sess.setThinkingLevel !== "function") return UNSUPPORTED;
        let r: any;
        try { r = helper.resolveThinkingArgument(sess, String(cmd.level ?? "")); } catch (e) { log("resolveThinkingArgument failed: " + e); return UNSUPPORTED; }
        if (r && typeof r === "object" && r.error) return { ok: false, error: String(r.error) };
        if (!r || typeof r !== "object" || typeof r.level !== "string") return UNSUPPORTED;
        try { sess.setThinkingLevel(r.level); } catch (e: any) { return { ok: false, error: String(e?.message ?? e) }; }
        refreshNow();
        return { ok: true, value: { thinking: typeof sess.thinkingLevel === "string" ? sess.thinkingLevel : null }, message: `Thinking set to ${r.level}.` };
      }

      async function handle(cmd: any): Promise<any> {
        if (cmd.t === "list") return { ok: true, snapshot: snapshot() };
        if (cmd.t === "fork_at" || cmd.t === "navigate") {
          const want = rootFile();
          if (typeof cmd.root !== "string" || !want || cmd.root !== want) return { ok: false, error: "session changed" };
          const sm = mainCtx.sessionManager;
          const id = String(cmd.entryId ?? "");
          const entry = sm.getEntry?.(id);
          if (!entry) return { ok: false, error: "no such entry" };
          if (cmd.t === "navigate") {
            // navigateTree has no streaming guard of its own: idle only.
            if (mainIdle() !== true) return { ok: false, error: "busy" };
            const cc = commandContext();
            if (typeof cc?.navigateTree !== "function") return { ok: false, error: "navigation unavailable" };
            const oldLeaf = mainLeaf();
            if (cmd.summarize === true) {
              // The command context's navigateTree cannot carry custom instructions: run the session's own (what /tree does), then redraw.
              const sess = mainSession();
              if (typeof sess?.navigateTree !== "function") return { ok: false, error: "navigation unavailable" };
              const instructions = typeof cmd.customInstructions === "string" && cmd.customInstructions.trim() ? cmd.customInstructions : undefined;
              const r = await sess.navigateTree(id, { summarize: true, customInstructions: instructions, requireIdle: true });
              if (r?.aborted) return { ok: false, error: "summarization cancelled" };
              if (r?.cancelled) return { ok: false, error: "navigation cancelled" };
              await redraw(oldLeaf, id, r);
            } else {
              const r = await cc.navigateTree(id, { summarize: false });
              if (r?.cancelled) return { ok: false, error: "navigation cancelled" };
            }
            schedule();
            const leaf = mainLeaf();
            return leaf === oldLeaf && id === oldLeaf ? { ok: true, leaf, message: "Already at this point" } : { ok: true, leaf };
          }
          if (entry.type !== "message") return { ok: false, error: "only message entries can be forked" };
          if (!SessionManager) return { ok: false, error: "fork unavailable" };
          // Second manager on the same file: the live tab keeps its session, leaf and screen.
          await sm.flush?.();
          const file = sm.getSessionFile?.();
          if (!file) return { ok: false, error: "session has no file" };
          const m2 = await SessionManager.open(file, undefined, undefined, { suppressBreadcrumb: true });
          try {
            const newFile = m2.createBranchedSession(forkCut(m2, id), { copyArtifacts: true });
            await m2.flush?.();
            if (!newFile) return { ok: false, error: "fork produced no file" };
            return { ok: true, newFile };
          } finally { try { await m2.close?.(); } catch {} }
        }
        if (cmd.t === "answer_ui") {
          const want = rootFile();
          if (typeof cmd.root !== "string" || !want || cmd.root !== want) return { ok: false, error: "session changed" };
          const p = pendingUi.get(Number(cmd.id));
          if (!p) return { ok: false, error: "no such request" };
          p.resolve(cmd.value ?? undefined); // null = cancel, as a local Esc
          return { ok: true };
        }
        if (cmd.t === "pin_account") {
          // == `/session pin` (AgentSession.pinCurrentProviderOAuthAccount); null credentialId clears the session's sticky
          // account (authStorage.sessions.clear), so omp ranks the accounts again on the next request.
          const want = rootFile();
          if (typeof cmd.root === "string" && want && cmd.root !== want) return { ok: false, error: "session changed" };
          const sess = mainSession();
          const provider = sess?.model?.provider;
          if (!sess || typeof provider !== "string" || provider !== cmd.provider) return { ok: false, error: "the session's model changed provider" };
          if (sess.isStreaming) return { ok: false, error: "busy: wait for the reply to finish" };
          if (cmd.credentialId === null || cmd.credentialId === undefined) {
            const sessions = sess.modelRegistry?.authStorage?.sessions;
            if (typeof sessions?.clear !== "function") return { ok: false, error: "unpinning unavailable" };
            sessions.clear(provider, sess.sessionId);
          } else {
            if (typeof sess.pinCurrentProviderOAuthAccount !== "function") return { ok: false, error: "pinning unavailable" };
            const target = providerAccounts(sess.modelRegistry?.authStorage, provider, sess.sessionId).find((a: any) => String(a.credentialId) === String(cmd.credentialId));
            if (!target || !sess.pinCurrentProviderOAuthAccount(target.credentialId)) return { ok: false, error: "that account is no longer available" };
          }
          schedule();
          return { ok: true };
        }
        if (cmd.t === "set_label") {
          // == the /tree selector's label edit: a `label` entry appended to the live session (any state; it is only an append).
          if (!sameRoot(cmd)) return { ok: false, error: "session changed" };
          const sm = mainCtx.sessionManager;
          const id = String(cmd.entryId ?? "");
          if (!sm.getEntry?.(id)) return { ok: false, error: "no such entry" };
          if (typeof sm.appendLabelChange !== "function") return { ok: false, error: "labels unavailable" };
          const label = typeof cmd.label === "string" && cmd.label.trim() ? cmd.label.trim() : undefined;
          sm.appendLabelChange(id, label);
          schedule();
          return { ok: true };
        }
        if (cmd.t === "reanswer_ask") return reanswerAsk(cmd);
        if (cmd.t === "advisor_config") return advisorConfig(cmd);
        if (cmd.t === "task_model_override") return taskModelOverride(cmd);
        if (cmd.t === "switch_model") return switchModel(cmd);
        if (cmd.t === "set_effort") return setEffort(cmd);
        if (cmd.t !== "kill" && cmd.t !== "revive" && cmd.t !== "chat") return { ok: false, error: "unknown cmd" };
        // The tab may have moved to another root session (/new, /resume) since the command was composed.
        const want = rootFile();
        if (typeof cmd.root !== "string" || !want || cmd.root !== want) return { ok: false, error: "session changed" };
        const lifecycle = AgentLifecycleManager.global();
        const ref = registry.get(String(cmd.id));
        if (!ref || ref.kind !== "sub") return { ok: false, error: "no such subagent" };
        // The registry keeps agents of earlier sessions (/new, /fork, /resume): only the current root's agents are controllable.
        const root = rootFile();
        let owned = false;
        try { owned = !!(root && ref.sessionFile && sessionFileBelongsToRoot(ref.sessionFile, root)); } catch {}
        if (!owned) return { ok: false, error: "no such subagent" };
        switch (cmd.t) {
          case "kill": // == Hub `x`
            if (ref.status === "running" && ref.session) await ref.session.abort({ reason: "OmpDeck kill" });
            return { ok: true, released: await lifecycle.release(ref.id, ref, { tombstone: true }) };
          case "revive": // == Hub `r`
            await lifecycle.ensureLive(ref.id); return { ok: true, status: registry.get(ref.id)?.status };
          default: { // chat == transcript-viewer input: revives parked, steers running, prompts idle
            const s = await lifecycle.ensureLive(ref.id);
            // prompt() resolves when the whole turn ends: reply once it has been accepted (it did not reject
            // within the grace window) and only log a later failure.
            const turn = Promise.resolve(s.prompt(String(cmd.text ?? ""), { streamingBehavior: "steer" }));
            const early = await Promise.race([
              turn.then(() => undefined, (e: any) => ({ error: String(e?.message ?? e) })),
              new Promise<undefined>(r => setTimeout(r, 300)),
            ]);
            turn.catch((e: any) => log("chat turn failed: " + e));
            return early ? { ok: false, error: early.error } : { ok: true };
          }
        }
      }
      const reply = (s: any, rid: any, r: any) => { let l: string; try { l = JSON.stringify({ t: "reply", rid, ...r }) + "\n"; } catch { l = JSON.stringify({ t: "reply", rid, ok: false, error: "unserializable reply" }) + "\n"; } write(s, l); };

      // ---- endpoint ----
      const onOpen = (s: any) => {
        s.data = { buf: "" }; clients.add(s);
        write(s, JSON.stringify({ t: "hello", v: 1, pid: process.pid, ui_hook: uiState.hook }) + "\n" + JSON.stringify(snapshot()) + "\n");
      };
      const onData = (s: any, chunk: any) => {
        const st = s.data; if (!st) return;
        st.buf += chunk.toString();
        if (st.buf.length > 8 * 1024 * 1024) { st.buf = ""; return; } // runaway client without newlines
        for (let i; (i = st.buf.indexOf("\n")) >= 0;) {
          const line = st.buf.slice(0, i); st.buf = st.buf.slice(i + 1);
          let cmd: any; try { cmd = JSON.parse(line); } catch { continue; }
          if (!cmd || typeof cmd !== "object") continue;
          let p: Promise<any>; try { p = handle(cmd); } catch (e: any) { p = Promise.reject(e); }
          p.then(r => reply(s, cmd.rid, r), e => reply(s, cmd.rid, { ok: false, error: String(e?.message ?? e) }));
        }
      };
      const onClose = (s: any) => { clients.delete(s); };
      let server: { stop(): void };
      let releaseEndpoint: () => void = () => {}; // Unix: unlink our socket, drop the lock, remove the dir
      let close: () => void = () => {};
      if (PIPE) {
        // Windows: a named pipe has no files to own. A listen error (name held by another omp, or any I/O failure) means
        // "owned elsewhere": tear down quietly. No lock file, no mode check, no unlink.
        const want = Buffer.from(TOKEN!);
        const tokenOk = (line: string): boolean => {
          let m: any; try { m = JSON.parse(line); } catch { return false; }
          if (!m || m.t !== "auth" || typeof m.token !== "string") return false;
          const got = Buffer.from(m.token);
          return got.length === want.length && crypto.timingSafeEqual(got, want);
        };
        const AUTH_MS = 2000, MAX_PENDING = 8;
        let pending = 0; // connections that have not authenticated yet: a flood of them is cut off
        const srv = net.createServer((s: any) => {
          try { s.setEncoding("utf8"); } catch {}
          if (pending >= MAX_PENDING) { s.destroy(); return; }
          pending++;
          let authed = false, counted = true, pre = "";
          const settle = () => { if (counted) { counted = false; pending--; } };
          const timer = setTimeout(() => { if (!authed) { settle(); s.destroy(); } }, AUTH_MS);
          const end = () => { clearTimeout(timer); settle(); if (authed) onClose(s); };
          s.on("data", (chunk: any) => {
            if (authed) return onData(s, chunk);
            pre += chunk;
            const i = pre.indexOf("\n");
            if (i < 0) { if (pre.length > 1024) { clearTimeout(timer); settle(); s.destroy(); } return; }
            clearTimeout(timer); settle();
            if (!tokenOk(pre.slice(0, i))) { s.destroy(); return; }
            authed = true; onOpen(s);
            const rest = pre.slice(i + 1); pre = "";
            if (rest) onData(s, rest);
          });
          s.on("close", end);
          s.on("error", end);
        });
        srv.on("error", (e: any) => {
          log("pipe listen failed: " + e);
          if (G.__ompdeckCtl?.close === close) G.__ompdeckCtl = undefined;
          close();
        });
        try { srv.listen(PIPE); } catch (e) { log("listen failed: " + e); return; }
        server = { stop() { try { srv.close(); } catch {} for (const c of clients) { try { c.destroy(); } catch {} } clients.clear(); } };
      } else {
        // The dir must be a private real directory of ours (created 0700 if missing); never chmod/follow anything.
        const me = process.getuid?.();
        const sockPath = path.join(DIR!, "ctl.sock");
        if (Buffer.byteLength(sockPath) >= 108) { log("socket path too long"); return; }
        try { fs.mkdirSync(DIR!, { recursive: true, mode: 0o700 }); } catch (e) { log("mkdir failed: " + e); return; }
        try {
          const st = fs.lstatSync(DIR!);
          if (!st.isDirectory() || (me !== undefined && st.uid !== me) || (st.mode & 0o077) !== 0) { log("ctl dir not private"); return; }
        } catch (e) { log("ctl dir stat failed: " + e); return; }
        // Exclusive ownership via an O_EXCL pid file; a lock whose pid is dead is reclaimed, a live one means another omp serves this dir.
        const lockPath = path.join(DIR!, "ctl.lock");
        // Lock content: "<pid> <starttime>" (/proc/<pid>/stat field 22, parsed after the last ')'). Stale = pid dead or start time differs (pid reuse).
        const startTime = (pid: number): string | null => {
          try { const st = fs.readFileSync(`/proc/${pid}/stat`, "utf8"); return st.slice(st.lastIndexOf(")") + 1).trim().split(/\s+/)[19] ?? null; } catch { return null; }
        };
        const myStart = startTime(process.pid);
        const myLock = `${process.pid} ${myStart ?? ""}`.trim();
        const parseLock = (t: string): { pid: number; start: string | null } => { const [p, s] = t.trim().split(/\s+/); return { pid: parseInt(p, 10), start: s ?? null }; };
        const ownerLive = (t: string): boolean => {
          const { pid, start } = parseLock(t);
          if (!Number.isFinite(pid) || pid <= 0) return false;
          const now = startTime(pid);
          if (now === null) return false;
          return start === null ? true : start === now;
        };
        const tryLock = (): boolean => {
          try { const fd = fs.openSync(lockPath, "wx", 0o600); try { fs.writeSync(fd, myLock); } finally { fs.closeSync(fd); } return true; }
          catch (e: any) { if (e?.code !== "EEXIST") { log("lock failed: " + e); } return false; }
        };
        let locked = tryLock();
        if (!locked) {
          let holder = ""; try { holder = fs.readFileSync(lockPath, "utf8"); } catch {}
          if (holder !== myLock && ownerLive(holder)) { log(`dir owned by live lock ${holder}`); return; }
          // Stale (dead pid, or unreadable/garbage): move it aside atomically, confirm it is the one we judged stale, retry once.
          const aside = `${lockPath}.stale.${process.pid}`;
          try {
            fs.renameSync(lockPath, aside);
            let got = ""; try { got = fs.readFileSync(aside, "utf8"); } catch {}
            if (got === holder) { fs.unlinkSync(aside); locked = tryLock(); }
            else { try { fs.linkSync(aside, lockPath); } catch {} try { fs.unlinkSync(aside); } catch {} log("lost lock reclaim race"); return; }
          } catch (e) { log("stale lock reclaim failed: " + e); return; }
          if (!locked) return;
        }
        const unlock = () => { try { if (fs.readFileSync(lockPath, "utf8") === myLock) fs.unlinkSync(lockPath); } catch {} };
        // We hold the lock, so any socket here is a leftover of a dead process; still only unlink an actual socket of ours.
        try {
          const st = fs.lstatSync(sockPath);
          if (!st.isSocket() || (me !== undefined && st.uid !== me)) { log("refusing to replace non-socket at ctl.sock"); unlock(); return; }
          fs.unlinkSync(sockPath);
        } catch (e: any) { if (e?.code !== "ENOENT") { log("sock stat failed: " + e); unlock(); return; } }
        let bun: any;
        try {
          bun = Bun.listen<object>({
            unix: sockPath,
            socket: { open: onOpen, close: onClose, error: onClose, data: onData },
          });
        } catch (e) { log("listen failed: " + e); unlock(); return; }
        server = { stop() { try { bun.stop(true); } catch {} } };
        let ino: number | undefined; try { ino = fs.lstatSync(sockPath).ino; } catch {}
        releaseEndpoint = () => {
          // Unlink only an endpoint that is still ours (same inode), then the lock, then the dir if that left it empty.
          try { if (ino !== undefined && fs.lstatSync(sockPath).ino === ino) fs.unlinkSync(sockPath); } catch {}
          unlock();
          try { fs.rmdirSync(DIR!); } catch {}
        };
      }
      let offRegistry: any, tick: any, offProgress: any;
      close = () => {
        try { if (typeof offRegistry === "function") offRegistry(); } catch {}
        try { if (typeof offProgress === "function") offProgress(); } catch {}
        try { clearInterval(tick); clearTimeout(pending); } catch {}
        try { server.stop(); } catch {}
        releaseEndpoint();
      };
      try {
        try { wrapUi(ctx.ui); } catch (e) { log("ui hook failed: " + e); uiState.hook = false; }
        offRegistry = registry.onChange(schedule); // registered/status_changed/metadata_changed(activity)/removed
        tick = setInterval(() => { if (!clients.size) return; if (modelKey() !== contextKey) recompute(); push(snapshot()); }, 1000); // safety net for isStreaming/mainIdle
        try { offProgress = api.events.on("task:subagent:progress", schedule); } catch (e) { log("progress listener failed: " + e); }
        G.__ompdeckCtl = {
          setCtx(c: any) { mainCtx = c; schedule(); },
          recompute,
          close,
        };
      } catch (e) { log("setup failed: " + e); close(); return; }
      log("listening " + (PIPE ?? path.join(DIR!, "ctl.sock")));
      recompute();
    }
    function stop() { const c = G.__ompdeckCtl; G.__ompdeckCtl = undefined; c?.close(); }
  } catch (e) { log("factory failed: " + e); }
}
