// ompdeck-sess.ts — OmpDeck session helper (batch): the session-store operations omp offers only inside a process.
// Run headless:
//   env OMPDECK_SESS_REQ=<file, one NDJSON request per line> OMPDECK_SESS_OUT=<result file>
//   omp --mode rpc --no-session --no-extensions -e <path>
// Inert when either variable is unset. Appends one JSON line per request to OMPDECK_SESS_OUT as soon as that request
// is done: {"rid":<n>,"ok":true,…data} | {"rid":<n>,"ok":false,"error":"busy"|"<message>"}.
// A request without a line means failure (an omp-internal specifier below no longer resolves, or omp died).
//
// Requests (`op`):
//   set_pinned        {id, pinned}                       -> {pinned}
//   rename_session    {file, title}                      -> {title}          error "busy" when a live omp owns the session
//   set_label         {file, entryId, label|null}        -> {leaf}           same lease rule; unknown entry -> its message
//   foreign_sessions  {source:"claude"|"codex"}          -> {sessions:[{source,id,path,cwd,title?,created,modified,messageCount?,firstMessage?}]}
//   import_foreign    {source, id, cwd}                  -> {file}           a fresh omp session id; `cwd` replaces a vanished project dir
//   advisor_config    {cwd, scope:"project"|"user"}      -> {path, doc}      doc = omp's WatchdogConfigDoc (raw, un-merged)
//   save_advisor_config {cwd, scope, doc}                -> {path, removed}  an empty doc deletes the file
import * as fs from "node:fs";
import * as path from "node:path";

const REQ = process.env.OMPDECK_SESS_REQ;
const OUT = process.env.OMPDECK_SESS_OUT;

const report = (r: unknown) => {
  try { fs.appendFileSync(OUT!, JSON.stringify(r) + "\n"); } catch {}
};

// The session id is the `id` of the header (type "session"): line 1, or line 2 when omp put a padded `title` line first.
function headerId(file: string): string | undefined {
  let fd: number | undefined;
  try {
    fd = fs.openSync(file, "r");
    const buf = Buffer.alloc(16 * 1024);
    const n = fs.readSync(fd, buf, 0, buf.length, 0);
    for (const line of buf.subarray(0, n).toString("utf8").split("\n").slice(0, 3)) {
      let h: any;
      try { h = JSON.parse(line); } catch { continue; }
      if (h?.type === "session") return typeof h.id === "string" && h.id ? h.id : undefined;
    }
    return undefined;
  } catch {
    return undefined;
  } finally {
    if (fd !== undefined) try { fs.closeSync(fd); } catch {}
  }
}

const BUSY = "busy";
const str = (v: unknown, what: string): string => {
  if (typeof v !== "string" || !v) throw new Error(`${what} is required`);
  return v;
};
const scopeOf = (v: unknown): "project" | "user" => {
  if (v === "project" || v === "user") return v;
  throw new Error("scope must be project or user");
};

export default async function ompdeckSess(_api: any) {
  if (!REQ || !OUT) return; // not launched by OmpDeck -> inert
  let requests: any[];
  try {
    requests = fs.readFileSync(REQ, "utf8").split("\n").filter((l) => l.length > 0).map((l) => JSON.parse(l))
      .filter((r) => r && typeof r === "object" && typeof r.rid === "number" && typeof r.op === "string");
  } catch {
    return;
  }

  // Imports are lazy and per op: one omp-internal specifier drifting must only fail the ops that need it.
  const storage = async () => import("@oh-my-pi/pi-coding-agent/session/session-storage");
  const manager = async () => (await import("@oh-my-pi/pi-coding-agent/session/session-manager")).SessionManager;
  const utils = async () => import("@oh-my-pi/pi-utils");

  // WATCHDOG.yml access: omp's own advisor/config is not importable from an extension, see ompdeck-advisor.ts.
  const advisor = async () => import("./ompdeck-advisor.ts");

  const ops: Record<string, (r: any) => Promise<any>> = {
    async set_pinned(r) {
      const id = str(r.id, "id");
      if (typeof r.pinned !== "boolean") throw new Error("pinned is required");
      const pins = await import("@oh-my-pi/pi-coding-agent/session/session-pins");
      await pins.setSessionPinned(id, r.pinned, (await utils()).getAgentDir());
      return { pinned: r.pinned };
    },

    async rename_session(r) {
      const file = str(r.file, "file");
      const title = str(r.title, "title");
      const SM = await manager();
      // renameSessionTitle claims the lease itself, before loading; null = not owned by us or nothing was written.
      const stored = await SM.renameSessionTitle(file, title, {});
      if (stored) return { title: stored };
      const id = headerId(file);
      if (!id) throw new Error("not an omp session file");
      const release = new (await storage()).FileSessionStorage().claimSession(id, file);
      if (!release) return { ok: false, error: BUSY };
      try { release(); } catch {}
      throw new Error("the title was refused (empty after cleaning?)");
    },

    async set_label(r) {
      const file = str(r.file, "file");
      const entryId = str(r.entryId, "entryId");
      const label = typeof r.label === "string" && r.label.trim() ? r.label : undefined;
      const id = headerId(file);
      if (!id) throw new Error("not an omp session file");
      // Same protocol as SessionManager.renameSessionTitle: claim the id for this file first (null = a live omp owns it),
      // then open (it claims again itself, re-entrantly). Holding a lease of our own instead makes open() see the session
      // as owned elsewhere and write the change into a continuation file.
      const fss = new (await storage()).FileSessionStorage();
      const release = fss.claimSession(id, file);
      if (!release) return { ok: false, error: BUSY };
      try {
        const SM = await manager();
        const m = await SM.open(file, undefined, fss, { suppressBreadcrumb: true, throwIfMissing: true });
        try {
          m.appendLabelChange(entryId, label);
          await m.flush?.();
          return { leaf: m.getLeafId?.() ?? null };
        } finally { try { await m.close?.(); } catch {} }
      } finally { try { release(); } catch {} }
    },

    async foreign_sessions(r) {
      const source = r.source === "claude" || r.source === "codex" ? r.source : undefined;
      if (!source) throw new Error("source must be claude or codex");
      const imp = await import("@oh-my-pi/pi-coding-agent/session/foreign-session-import");
      const list: any[] = await imp.createForeignSessionStore(source).list();
      const trunc = (s: unknown) => (typeof s === "string" ? s.slice(0, 400) : undefined);
      return {
        sessions: list.slice(0, 5000).map((s) => ({
          source: s.source, id: s.id, path: s.path, cwd: s.cwd, title: trunc(s.title),
          created: s.created instanceof Date ? s.created.getTime() : Number(s.created) || 0,
          modified: s.modified instanceof Date ? s.modified.getTime() : Number(s.modified) || 0,
          messageCount: typeof s.messageCount === "number" ? s.messageCount : undefined,
          firstMessage: trunc(s.firstMessage),
        })),
      };
    },

    async import_foreign(r) {
      const source = r.source === "claude" || r.source === "codex" ? r.source : undefined;
      if (!source) throw new Error("source must be claude or codex");
      const id = str(r.id, "id");
      const cwd = str(r.cwd, "cwd");
      const imp = await import("@oh-my-pi/pi-coding-agent/session/foreign-session-import");
      const store = imp.createForeignSessionStore(source);
      const info = (await store.list()).find((s: any) => s.id === id);
      if (!info) throw new Error(`no ${source} session ${id}`);
      const m = await imp.persistForeignSession(store, info, { fallbackCwd: cwd, suppressBreadcrumb: true });
      try {
        await m.flush?.();
        const file = m.getSessionFile?.();
        if (!file) throw new Error("the import produced no session file");
        return { file: path.resolve(file) };
      } finally { try { await m.close?.(); } catch {} }
    },

    async advisor_config(r) {
      const adv = await advisor();
      const file = adv.resolveEditPath(scopeOf(r.scope), adv.dirsFor(str(r.cwd, "cwd")));
      return { path: file, doc: await adv.loadDoc(file) };
    },

    async save_advisor_config(r) {
      const adv = await advisor();
      const file = adv.resolveEditPath(scopeOf(r.scope), adv.dirsFor(str(r.cwd, "cwd")));
      const doc = r.doc;
      if (!doc || typeof doc !== "object" || !Array.isArray(doc.advisors)) throw new Error("doc.advisors must be a list");
      return { path: file, removed: await adv.saveDoc(file, doc) };
    },
  };

  for (const req of requests) {
    try {
      const op = ops[req.op];
      if (!op) { report({ rid: req.rid, ok: false, error: `unknown op ${req.op}` }); continue; }
      const out = await op(req);
      report({ ok: true, ...out, rid: req.rid });
    } catch (e) {
      report({ rid: req.rid, ok: false, error: String((e as any)?.message ?? e) });
    }
  }
}
