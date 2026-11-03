// ompdeck-advisor.ts — WATCHDOG.yml (advisor configuration) load / save / discovery for OmpDeck's extensions.
// A port of omp 18.16.0 `advisor/config.ts` + `advisor/watchdog.ts#collectConfigCandidates`: omp's own module is NOT
// importable from an extension (the compiled binary only exposes package.json `exports` subpaths, and `advisor/*` is
// not one of them), but its ingredients are. Imported by ompdeck-ctl.ts and ompdeck-sess.ts from the same directory.
// Semantics mirrored from omp: entries are validated one by one (a bad advisor drops out with a warning), a missing or
// unparseable file is an empty document, an empty document deletes the file, discovery merges user < project ancestors
// < project leaf by advisor slug, `.yaml` is edited in place when no `.yml` exists.
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { YAML } from "bun";
import { getAgentDir, slugify } from "@oh-my-pi/pi-utils";
import { repo } from "@oh-my-pi/pi-natives/vcs";
import { expandAtImports } from "@oh-my-pi/pi-coding-agent/discovery/at-imports";
import { BUILTIN_TOOL_NAMES, normalizeToolNames } from "@oh-my-pi/pi-coding-agent/tools/builtin-names";
import {
  ADVISOR_REVIEW_MODES,
  ADVISOR_SESSION_SCOPES,
  ADVISOR_SYNC_BACKLOG_MODES,
} from "@oh-my-pi/pi-tui/overlays/advisor-config";

export type Scope = "project" | "user";
export type Dirs = { projectDir: string; agentDir: string };

const KNOWN_TOOLS = new Set<string>(BUILTIN_TOOL_NAMES);
const NUMERIC_BACKLOG: Record<number, string> = { 1: "1", 3: "3", 5: "5" };
// Fields this module understands; anything else in a WATCHDOG.yml (written by a newer omp, or by hand) is carried through unchanged.
const DOC_KEYS = new Set(["instructions", "maxNotesPerUpdate", "advisors", "warnings", "path"]);
const ENTRY_KEYS = new Set(["name", "model", "tools", "reviewMode", "reviewInterval", "syncBacklog", "instructions", "enabled", "maxNotesPerUpdate", "scope", "includeReasoning"]);
const extrasOf = (o: Record<string, any>, known: Set<string>): Record<string, any> => Object.fromEntries(Object.entries(o).filter(([k, v]) => !known.has(k) && v !== undefined));
// JSON is valid YAML (flow style), so unknown values of any shape are written back losslessly.
const extraLines = (indent: string, o: Record<string, any>, known: Set<string>): string[] => Object.entries(extrasOf(o, known)).map(([k, v]) => `${indent}${JSON.stringify(k)}: ${JSON.stringify(v)}`);
const isObj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
const posInt = (v: unknown): v is number => typeof v === "number" && Number.isInteger(v) && v >= 1 && v <= Number.MAX_SAFE_INTEGER;
const finitePos = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v) && v >= 1;
const strArr = (v: unknown): v is string[] => Array.isArray(v) && v.every((x) => typeof x === "string");

/** `<vcs root or cwd>` and the agent dir: omp's `/advisor configure` dirs. */
export function dirsFor(cwd: string): Dirs {
  let projectDir = cwd;
  try { projectDir = repo(cwd)?.root() ?? cwd; } catch { projectDir = cwd; }
  return { projectDir, agentDir: getAgentDir() };
}

export function configFilePath(scope: Scope, dirs: Dirs): string {
  return path.join(scope === "user" ? dirs.agentDir : dirs.projectDir, "WATCHDOG.yml");
}

/** The canonical `.yml`, unless only a `.yaml` exists for the scope. */
export function resolveEditPath(scope: Scope, dirs: Dirs): string {
  const dir = scope === "user" ? dirs.agentDir : dirs.projectDir;
  const yml = path.join(dir, "WATCHDOG.yml");
  const yaml = path.join(dir, "WATCHDOG.yaml");
  if (!fs.existsSync(yml) && fs.existsSync(yaml)) return yaml;
  return yml;
}

/** One advisor entry validated like omp's arktype schema; a string describes the first problem. */
function checkEntry(e: unknown): string | Record<string, any> {
  if (!isObj(e)) return "must be an object";
  if (typeof e.name !== "string") return "name must be a string";
  if (e.model !== undefined && typeof e.model !== "string") return "model must be a string";
  if (e.tools !== undefined && !strArr(e.tools)) return "tools must be a list of strings";
  if (e.reviewMode !== undefined && !(ADVISOR_REVIEW_MODES as readonly unknown[]).includes(e.reviewMode)) return `reviewMode must be one of ${ADVISOR_REVIEW_MODES.join(", ")}`;
  if (e.reviewInterval !== undefined && !posInt(e.reviewInterval)) return "reviewInterval must be an integer >= 1";
  if (e.syncBacklog !== undefined && !(ADVISOR_SYNC_BACKLOG_MODES as readonly unknown[]).includes(e.syncBacklog) && ![1, 3, 5].includes(e.syncBacklog as number)) return `syncBacklog must be one of ${ADVISOR_SYNC_BACKLOG_MODES.join(", ")}, 1, 3, 5`;
  if (e.instructions !== undefined && typeof e.instructions !== "string") return "instructions must be a string";
  if (e.enabled !== undefined && typeof e.enabled !== "boolean") return "enabled must be a boolean";
  if (e.maxNotesPerUpdate !== undefined && typeof e.maxNotesPerUpdate !== "number") return "maxNotesPerUpdate must be a number";
  if (e.scope !== undefined && !(ADVISOR_SESSION_SCOPES as readonly unknown[]).includes(e.scope)) return `scope must be one of ${ADVISOR_SESSION_SCOPES.join(", ")}`;
  if (e.includeReasoning !== undefined && typeof e.includeReasoning !== "boolean") return "includeReasoning must be a boolean";
  return e;
}

function parseDoc(doc: Record<string, unknown>, file: string) {
  const warnings: string[] = [];
  const instructions = typeof doc.instructions === "string" ? doc.instructions : undefined;
  if (doc.instructions !== undefined && instructions === undefined) warnings.push(`${file}: instructions must be a string — ignored`);
  const sharedMax = finitePos(doc.maxNotesPerUpdate) ? Math.trunc(doc.maxNotesPerUpdate) : undefined;
  if (doc.advisors !== undefined && !Array.isArray(doc.advisors)) warnings.push(`${file}: advisors must be a list — ignored`);
  const entries: Record<string, any>[] = [];
  for (const [i, raw] of (Array.isArray(doc.advisors) ? doc.advisors : []).entries()) {
    const r = checkEntry(raw);
    if (typeof r === "string") {
      const n = isObj(raw) && typeof raw.name === "string" && raw.name.trim() ? `"${raw.name}"` : `#${i + 1}`;
      warnings.push(`${file}: advisor ${n} dropped — ${r}`);
      continue;
    }
    entries.push(r);
  }
  return { instructions, entries, sharedMax, warnings };
}

/** A file for editing: raw, un-merged, un-expanded. Missing / unparseable → an empty document (never throws). */
export async function loadDoc(file: string): Promise<Record<string, any>> {
  let text: string;
  try { text = await Bun.file(file).text(); } catch (e: any) {
    return e?.code === "ENOENT" ? { advisors: [] } : { advisors: [], warnings: [`${file}: ${String(e)}`] };
  }
  let parsed: unknown;
  try { parsed = YAML.parse(text); } catch (e) { return { advisors: [], warnings: [`${file}: failed to parse YAML (${String(e)})`] }; }
  if (!isObj(parsed)) return { advisors: [], warnings: [`${file}: expected a YAML mapping — file skipped`] };
  const { instructions, entries, sharedMax, warnings } = parseDoc(parsed, file);
  const advisors = entries.map((a) => {
    const out: Record<string, any> = { name: a.name };
    if (typeof a.model === "string" && a.model.trim()) out.model = a.model;
    if (a.tools !== undefined) out.tools = [...a.tools];
    if (a.reviewMode !== undefined) out.reviewMode = a.reviewMode;
    if (a.reviewInterval !== undefined) out.reviewInterval = a.reviewInterval;
    if (a.syncBacklog !== undefined) out.syncBacklog = typeof a.syncBacklog === "number" ? NUMERIC_BACKLOG[a.syncBacklog] : a.syncBacklog;
    if (typeof a.instructions === "string" && a.instructions.trim()) out.instructions = a.instructions;
    if (a.enabled !== undefined) out.enabled = a.enabled;
    if (finitePos(a.maxNotesPerUpdate)) out.maxNotesPerUpdate = Math.trunc(a.maxNotesPerUpdate);
    if (a.scope !== undefined) out.scope = a.scope;
    if (a.includeReasoning !== undefined) out.includeReasoning = a.includeReasoning;
    return { ...extrasOf(a, ENTRY_KEYS), ...out };
  });
  const doc: Record<string, any> = { advisors };
  if (instructions?.trim()) doc.instructions = instructions;
  if (sharedMax !== undefined) doc.maxNotesPerUpdate = sharedMax;
  Object.assign(doc, extrasOf(parsed, DOC_KEYS));
  if (warnings.length > 0) doc.warnings = warnings;
  return doc;
}

function yamlString(lines: string[], indent: string, key: string, value: string): void {
  const leadingWs = value.split("\n").some((l) => /^[ \t]/.test(l));
  if (!value.includes("\n") || leadingWs) { lines.push(`${indent}${key}: ${YAML.stringify(value)}`); return; }
  const normalized = value.replaceAll("\r\n", "\n");
  let trailing = 0;
  for (let i = normalized.length - 1; i >= 0 && normalized[i] === "\n"; i--) trailing++;
  const chomp = trailing === 0 ? "|2-" : trailing === 1 ? "|2" : "|2+";
  const body = trailing === 0 ? normalized : normalized.slice(0, -trailing);
  lines.push(`${indent}${key}: ${chomp}`);
  for (const l of body.split("\n")) lines.push(`${indent}  ${l}`);
  for (let i = 1; i < trailing; i++) lines.push(`${indent}  `);
}

/** Canonical hand-editable YAML; "" for an empty document. Throws on a value omp would refuse. */
export function serialize(doc: Record<string, any>): string {
  const lines: string[] = [];
  if (typeof doc.instructions === "string" && doc.instructions.trim()) yamlString(lines, "", "instructions", doc.instructions);
  if (finitePos(doc.maxNotesPerUpdate)) lines.push(`maxNotesPerUpdate: ${Math.trunc(doc.maxNotesPerUpdate)}`);
  lines.push(...extraLines("", doc, DOC_KEYS));
  const advisors: any[] = Array.isArray(doc.advisors) ? doc.advisors : [];
  if (advisors.length > 0) {
    lines.push("advisors:");
    for (const a of advisors) {
      const bad = checkEntry(a);
      if (typeof bad === "string") throw new Error(`advisor ${isObj(a) && typeof a.name === "string" ? `"${a.name}"` : ""}: ${bad}`);
      lines.push(`  - name: ${YAML.stringify(a.name)}`);
      if (typeof a.model === "string" && a.model.trim()) lines.push(`    model: ${YAML.stringify(a.model)}`);
      if (a.tools !== undefined) {
        if (a.tools.length === 0) lines.push("    tools: []");
        else { lines.push("    tools:"); for (const t of a.tools) lines.push(`      - ${YAML.stringify(t)}`); }
      }
      if (a.reviewMode !== undefined) lines.push(`    reviewMode: ${YAML.stringify(a.reviewMode)}`);
      if (a.syncBacklog !== undefined) {
        const s = typeof a.syncBacklog === "number" ? NUMERIC_BACKLOG[a.syncBacklog] : a.syncBacklog;
        lines.push(`    syncBacklog: ${YAML.stringify(s)}`);
      }
      if (typeof a.instructions === "string" && a.instructions.trim()) yamlString(lines, "    ", "instructions", a.instructions);
      if (a.enabled !== undefined) lines.push(`    enabled: ${a.enabled}`);
      if (finitePos(a.maxNotesPerUpdate)) lines.push(`    maxNotesPerUpdate: ${Math.trunc(a.maxNotesPerUpdate)}`);
      if (a.reviewInterval !== undefined) lines.push(`    reviewInterval: ${a.reviewInterval}`);
      if (a.scope !== undefined) lines.push(`    scope: ${YAML.stringify(a.scope)}`);
      if (a.includeReasoning !== undefined) lines.push(`    includeReasoning: ${a.includeReasoning}`);
      lines.push(...extraLines("    ", a, ENTRY_KEYS));
    }
  }
  return lines.length === 0 ? "" : `${lines.join("\n")}\n`;
}

/** Writes the document; an empty one removes the file. Returns whether the file is gone. */
export async function saveDoc(file: string, doc: Record<string, any>): Promise<boolean> {
  const content = serialize(doc);
  if (!content.trim()) { fs.rmSync(file, { force: true }); return true; }
  await Bun.write(file, content);
  return false;
}

type Candidate = { path: string; content: string; level: "user" | "project"; depth: number };

async function collectCandidates(cwd: string, agentDir: string, filenames: string[]): Promise<Candidate[]> {
  const home = os.homedir();
  const userPaths = new Set<string>();
  let repoRoot: string | null = null;
  try { repoRoot = repo(cwd)?.root() ?? null; } catch { repoRoot = null; }
  const candidates = new Set<string>();
  for (const f of filenames) { const p = path.resolve(agentDir, f); candidates.add(p); userPaths.add(p); }
  let current = cwd;
  while (true) {
    for (const f of filenames) { candidates.add(path.resolve(current, ".omp", f)); candidates.add(path.resolve(current, f)); }
    if (current === (repoRoot ?? home)) break;
    const parent = path.dirname(current);
    if (parent === current) break;
    current = parent;
  }
  const items: Candidate[] = [];
  for (const candidate of candidates) {
    let content: string;
    try { content = await Bun.file(candidate).text(); } catch { continue; }
    const parent = path.dirname(candidate);
    const baseName = parent.split(path.sep).pop() ?? "";
    const isUser = userPaths.has(candidate);
    const ownerDir = baseName === ".omp" ? path.dirname(parent) : parent;
    const ownerBase = ownerDir.split(path.sep).pop() ?? "";
    if (isUser || !ownerBase.startsWith(".") || baseName === ".omp") {
      const rel = path.relative(cwd, ownerDir);
      items.push({ path: candidate, content, level: isUser ? "user" : "project", depth: rel === "" ? 0 : rel.split(path.sep).filter(Boolean).length });
    }
  }
  items.sort((a, b) => (a.level !== b.level ? (a.level === "user" ? -1 : 1) : b.depth - a.depth));
  return items;
}

/** The merged roster of every WATCHDOG.yml/.yaml (user, project ancestors, project leaf): what omp applies to a session. */
export async function discover(cwd: string, agentDir: string) {
  const items = await collectCandidates(cwd, agentDir, ["WATCHDOG.yml", "WATCHDOG.yaml"]);
  const advisors = new Map<string, Record<string, any>>();
  const shared: string[] = [];
  let sharedMax: number | undefined;
  const warnings: string[] = [];
  for (const item of items) {
    let parsed: unknown;
    try { parsed = YAML.parse(item.content); } catch (e) { warnings.push(`${item.path}: failed to parse YAML (${String(e)}) — file skipped`); continue; }
    if (!isObj(parsed)) { warnings.push(`${item.path}: expected a YAML mapping — file skipped`); continue; }
    const d = parseDoc(parsed, item.path);
    warnings.push(...d.warnings);
    if (d.instructions?.trim()) { const x = (await expandAtImports(d.instructions, item.path)).trim(); if (x) shared.push(x); }
    if (d.sharedMax !== undefined) sharedMax = d.sharedMax;
    for (const e of d.entries) {
      const instructions = typeof e.instructions === "string" && e.instructions.trim() ? (await expandAtImports(e.instructions, item.path)).trim() || undefined : undefined;
      let tools: string[] | undefined;
      if (e.tools !== undefined) {
        tools = e.tools.length === 0 ? [] : normalizeToolNames(e.tools).filter((n: string) => KNOWN_TOOLS.has(n));
        if (e.tools.length > 0 && tools!.length === 0) tools = undefined;
      }
      advisors.set(slugify(e.name) || "advisor", {
        name: e.name,
        model: typeof e.model === "string" && e.model.trim() ? e.model.trim() : undefined,
        tools,
        reviewMode: e.reviewMode,
        reviewInterval: e.reviewInterval,
        syncBacklog: e.syncBacklog === undefined ? undefined : typeof e.syncBacklog === "number" ? NUMERIC_BACKLOG[e.syncBacklog] : e.syncBacklog,
        maxNotesPerUpdate: finitePos(e.maxNotesPerUpdate) ? Math.trunc(e.maxNotesPerUpdate) : undefined,
        enabled: e.enabled,
        scope: e.scope,
        includeReasoning: e.includeReasoning,
        instructions,
      });
    }
  }
  return { advisors: [...advisors.values()], sharedInstructions: shared.length > 0 ? shared.join("\n\n") : undefined, sharedMaxNotesPerUpdate: sharedMax, warnings };
}
