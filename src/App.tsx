import { FormEvent, MouseEvent, ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CREW, RoleId } from "./crew";
import { Formina, CrewSpin, ROLE_COLOR } from "./Formina";
import "./App.css";

type Line =
  | { kind: "you"; text: string; files?: { name: string; preview?: string }[] }
  | { kind: "them"; text: string; from?: RoleId; live?: boolean }
  | { kind: "brief"; text: string; from: RoleId; to: RoleId; live: boolean }
  | { kind: "clip"; text: string; title?: string; from?: RoleId }
  | { kind: "wrote"; path: string; diff: string; live?: boolean }
  | { kind: "event"; text: string }
  | { kind: "trace"; text: string; from?: RoleId; live?: boolean }
  | { kind: "think"; text: string; live: boolean; from?: RoleId }
  | { kind: "work"; ms: number; summary: string; lines: Line[]; from?: RoleId }
  | { kind: "job"; ms: number; who: RoleId[]; lines: Line[] }
  | { kind: "todo"; items: { content: string; status: string }[]; from?: RoleId }
  | {
      kind: "ask";
      who: RoleId;
      question: string;
      answer?: string;
      skipped?: boolean;
    }
  | {
      kind: "switch";
      fromPath: string;
      fromName: string;
      toPath: string;
      toName: string;
    };

type PendingFile = {
  name: string;
  data: string;
  preview?: string;
};

const MAX_ATTACH = 12;
const MAX_ATTACH_BYTES = 12 * 1024 * 1024;

type WorkPiece = {
  role: RoleId;
  brief: string;
  text: string;
  from?: string;
  paths?: string[];
};

function userFacingCopy(_p: WorkPiece): boolean {
  // Con la squadra a Coordinatore + Coder non ci sono più pezzi "da copiare"
  // prodotti da altri ruoli: il blocco copiabile nasce solo quando il
  // Coordinatore scrive a sé un pezzo standalone (in chat è un fence
  // markdown, con la copia del pre). Nessun auto-recap copiabile.
  return false;
}

function pieceBody(text: string) {
  const cut = text.search(/\n\*\*Notes\*\*/i);
  return (cut >= 0 ? text.slice(0, cut) : text).trim();
}

function copyPieces(raw: string): { title: string; text: string }[] {
  const body = pieceBody(raw);
  if (!body) return [];
  const re = /^===\s*(.+?)\s*===\s*$/gm;
  const marks = [...body.matchAll(re)];
  if (marks.length === 0) {
    return [{ title: "Text", text: body }];
  }
  if (marks.length === 1 && (marks[0].index ?? 0) > 0) {
    return [{ title: "Text", text: body }];
  }
  const out: { title: string; text: string }[] = [];
  for (let i = 0; i < marks.length; i++) {
    const title = marks[i][1].trim() || `Text ${i + 1}`;
    const start = (marks[i].index ?? 0) + marks[i][0].length;
    const end = i + 1 < marks.length ? (marks[i + 1].index ?? body.length) : body.length;
    const text = body.slice(start, end).trim();
    if (text) out.push({ title, text });
  }
  return out.length ? out : [{ title: "Text", text: body }];
}


function fenceTicks(text: string) {
  let ticks = "```";
  while (text.includes(ticks)) ticks += "`";
  return ticks;
}

function fencePiece(text: string) {
  const body = text.replace(/\n+$/, "");
  const ticks = fenceTicks(body);
  return `${ticks}\n${body}\n${ticks}`;
}

function copyAsMarkdown(raw: string): string {
  return copyPieces(raw)
    .map((piece) => {
      const fence = fencePiece(piece.text);
      if (isGenericClipTitle(piece.title)) return fence;
      return `**${piece.title}**\n\n${fence}`;
    })
    .join("\n\n");
}

function copyEcho(spoken: string, raw: string) {
  const want = spoken.trim();
  if (!want) return false;
  return copyPieces(raw).some((piece) => piece.text.trim() === want);
}

function withCopyRecap(
  list: Line[],
  raw: string,
  spoken?: string | null,
): Line[] {
  const fences = copyAsMarkdown(raw);
  if (!fences) return list;
  const extra =
    spoken?.trim() && !copyEcho(spoken, raw) ? spoken.trim() : "";
  const text = extra ? `${extra}\n\n${fences}` : fences;
  // Dedup only within this order — a repeated order that lands the same
  // words is not the same event as the earlier order that already showed
  // this exact block. Checking the whole chat here would silently drop
  // the block on a second identical answer.
  if (feedHasThem(sinceLastOrder(list), "coordinatore", text)) return list;
  const last = list.at(-1);
  if (
    last &&
    last.kind === "them" &&
    (last.from ?? "coordinatore") === "coordinatore" &&
    !last.text.includes(fences)
  ) {
    return [
      ...list.slice(0, -1),
      { ...last, text: `${last.text.trim()}\n\n${fences}` },
    ];
  }
  return [...list, { kind: "them", text, from: "coordinatore" }];
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const raw = String(reader.result || "");
      const cut = raw.indexOf(",");
      resolve(cut >= 0 ? raw.slice(cut + 1) : raw);
    };
    reader.onerror = () => reject(reader.error ?? new Error("Could not read the file."));
    reader.readAsDataURL(file);
  });
}

type WorkspaceInfo = {
  path: string;
  name: string;
  exists: boolean;
  source: string;
  chosen: boolean;
};

type CloudStatus = {
  configured: boolean;
  connected: boolean;
  signed_in: boolean;
  email: string;
  version: number;
  last: string;
  vault: string | null;
};

type CliCmd = {
  op?: string;
  text?: string;
  path?: string;
  paths?: string[];
  name?: string;
};

function shortPath(p: string, root?: string) {
  const n = p.replace(/\\/g, "/");
  if (root) {
    const r = root.replace(/\\/g, "/").replace(/\/$/, "");
    if (n === r) return r.split("/").filter(Boolean).at(-1) || n;
    if (n.startsWith(`${r}/`)) return n.slice(r.length + 1) || n;
  }
  const cut = n.lastIndexOf("/sandbox/");
  if (cut >= 0) return n.slice(cut + "/sandbox/".length) || n;
  const parts = n.split("/").filter(Boolean);
  return parts.at(-1) || n;
}

function crewName(id: RoleId) {
  return CREW.find((r) => r.id === id)?.name ?? id;
}

function isGenericClipTitle(title?: string) {
  const t = title?.trim();
  return !t || t === "Text" || t === "Testo";
}

function switchLineText(line: Extract<Line, { kind: "switch" }>) {
  return `Switched workspace ${line.fromName} → ${line.toName}.

The chosen folder is now ${line.toPath}. Relative paths, file tools, run cwd, and .puck/memory.md are this folder. User memory (comune.md) is unchanged. Messages above may be about ${line.fromName} (${line.fromPath}). Do not keep working as if you were still in that folder. Do not mix the two unless the User's current order says so.`;
}

function dragWindow(e: MouseEvent) {
  if (e.button !== 0) return;
  const el = e.target as HTMLElement;
  if (el.closest("button, input, a, textarea, select")) return;
  void getCurrentWindow().startDragging();
}

type AskReply = {
  text: string;
  woke: string[];
  work: { role: string; brief: string; text: string; from?: string }[];
};

type CrewPulse = {
  kind: string;
  role?: string;
  text?: string;
  brief?: string;
  from?: string;
  patchId?: string;
  path?: string;
};

function emptyThreads(): Record<RoleId, Line[]> {
  return {
    coordinatore: [],
    coder: [],
  };
}

function asRoleId(id: string | undefined): RoleId | null {
  if (!id) return null;
  return CREW.some((r) => r.id === id) ? (id as RoleId) : null;
}

type TodoItem = { content: string; status: string };

type UiSession = {
  version: number;
  activeId?: RoleId;
  woken?: RoleId[];
  recent?: RoleId[];
  threads?: Partial<Record<RoleId, Line[]>>;
  feed?: Line[];
  crewTodos?: Partial<Record<RoleId, TodoItem[]>>;
};

function asRoleList(raw: unknown): RoleId[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((id) => asRoleId(typeof id === "string" ? id : undefined))
    .filter((id): id is RoleId => id !== null);
}

function asLines(raw: unknown): Line[] {
  if (!Array.isArray(raw)) return [];
  return (raw as Line[]).filter((line) => {
    if (!line || !line.kind || line.kind === "think") return false;
    if (line.kind === "ask" && !line.answer && !line.skipped) return false;
    return true;
  });
}

function asThreads(raw: unknown): Record<RoleId, Line[]> {
  const next = emptyThreads();
  if (!raw || typeof raw !== "object") return next;
  const obj = raw as Record<string, unknown>;
  for (const role of CREW) {
    next[role.id] = asLines(obj[role.id]).filter((line) => line.kind !== "brief");
  }
  return next;
}

function asFeed(raw: unknown): Line[] {
  return asLines(raw);
}

function feedFromThreads(threads: Record<RoleId, Line[]>): Line[] {
  const out: Line[] = [];
  const add = (id: RoleId, line: Line) => {
    if (line.kind === "them" && !line.from) {
      out.push({ ...line, from: id });
      return;
    }
    if (line.kind === "work" && !line.from) {
      out.push({ ...line, from: id });
      return;
    }
    if (line.kind === "trace" && !line.from) {
      out.push({ ...line, from: id });
      return;
    }
    out.push(line);
  };
  for (const line of threads.coordinatore) add("coordinatore", line);
  for (const role of CREW) {
    if (role.id === "coordinatore") continue;
    for (const line of threads[role.id]) add(role.id, line);
  }
  return out;
}

function readSession(raw: unknown): UiSession | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as { version?: unknown };
  if (o.version !== 1) return null;
  return raw as UiSession;
}

function bumpRole(
  setRecent: (update: (prev: RoleId[]) => RoleId[]) => void,
  id: RoleId,
) {
  if (id === "coordinatore") return;
  setRecent((prev) => (prev[0] === id ? prev : [id, ...prev.filter((x) => x !== id)]));
}

function askContextText(line: Extract<Line, { kind: "ask" }>) {
  const q = line.question.trim();
  if (line.skipped || !line.answer?.trim()) {
    return `You asked the User:\n${q}\nThey skipped. That was not a new order.`;
  }
  return `You asked the User:\n${q}\nThey answered:\n${line.answer.trim()}\nThat answer is context for the order you were already executing, not a new order.`;
}

function threadTurns(owner: RoleId, lines: Line[]): { role: string; content: string }[] {
  const out: { role: "user" | "assistant"; content: string }[] = [];
  for (const line of lines) {
    if (line.kind === "switch") {
      out.push({ role: "user", content: switchLineText(line) });
      continue;
    }
    if (line.kind === "work") {
      out.push({ role: "assistant", content: line.summary });
      continue;
    }
    if (line.kind === "ask") {
      out.push({ role: "assistant", content: askContextText(line) });
      continue;
    }
    if (line.kind !== "them") continue;
    if (line.from === owner) {
      out.push({ role: "assistant", content: line.text });
    } else {
      out.push({ role: "user", content: line.text });
    }
  }
  return out;
}

function closeLiveThink(list: Line[], from?: RoleId): Line[] {
  const out: Line[] = [];
  for (const line of list) {
    if (line.kind !== "think") {
      out.push(line);
      continue;
    }
    if (from !== undefined && line.from !== from) {
      out.push(line);
      continue;
    }
    if (!line.text.trim()) continue;
    out.push(line.live ? { ...line, live: false } : line);
  }
  return out;
}

function parseTodoItems(raw: string | undefined): TodoItem[] {
  if (!raw) return [];
  try {
    const v = JSON.parse(raw) as unknown;
    if (!Array.isArray(v)) return [];
    return v
      .map((it) => {
        const row = it as { content?: unknown; status?: unknown };
        return {
          content: String(row.content ?? "").trim(),
          status: String(row.status ?? "pending"),
        };
      })
      .filter((it) => it.content);
  } catch {
    return [];
  }
}

function todoKey(content: string) {
  return content.trim().toLowerCase();
}

function mergeCrewTodos(prev: TodoItem[], next: TodoItem[]): TodoItem[] {
  if (!next.length) return [];
  const fresh = new Map(next.map((it) => [todoKey(it.content), it]));
  const used = new Set<string>();
  const out: TodoItem[] = [];
  for (const old of prev) {
    const k = todoKey(old.content);
    const hit = fresh.get(k);
    if (hit) {
      out.push(hit);
      used.add(k);
    } else if (old.status === "in_progress" || old.status === "done") {
      out.push({ content: old.content, status: "done" });
    }
  }
  for (const it of next) {
    const k = todoKey(it.content);
    if (!used.has(k)) out.push(it);
  }
  return out;
}

function asCrewTodos(raw: unknown): Partial<Record<RoleId, TodoItem[]>> {
  if (!raw || typeof raw !== "object") return {};
  const out: Partial<Record<RoleId, TodoItem[]>> = {};
  for (const [id, items] of Object.entries(raw as Record<string, unknown>)) {
    const who = asRoleId(id);
    if (!who || who === "coordinatore" || !Array.isArray(items)) continue;
    const list = items
      .map((it) => {
        const row = it as { content?: unknown; status?: unknown };
        return {
          content: String(row.content ?? "").trim(),
          status: String(row.status ?? "pending"),
        };
      })
      .filter((it) => it.content);
    if (list.length) out[who] = list;
  }
  return out;
}

function crewTodosFromLines(lines: Line[]): Partial<Record<RoleId, TodoItem[]>> {
  const out: Partial<Record<RoleId, TodoItem[]>> = {};
  for (const line of lines) {
    if (line.kind !== "todo") continue;
    const who = line.from;
    if (!who || who === "coordinatore") continue;
    out[who] = line.items;
  }
  return out;
}

function upsertWrote(
  list: Line[],
  path: string,
  diff: string,
  live: boolean,
): Line[] {
  const foundLive = [...list]
    .map((l, i) => ({ l, i }))
    .reverse()
    .find(({ l }) => l.kind === "wrote" && l.path === path && l.live);

  if (live) {
    if (foundLive && foundLive.l.kind === "wrote") {
      const next = [...list];
      next[foundLive.i] = {
        kind: "wrote",
        path,
        diff: diff.trim() ? diff : foundLive.l.diff,
        live: true,
      };
      return next;
    }
    return [...list, { kind: "wrote", path, diff, live: true }];
  }

  if (foundLive && foundLive.l.kind === "wrote") {
    const next = [...list];
    next[foundLive.i] = {
      kind: "wrote",
      path,
      diff: diff.trim() ? diff : foundLive.l.diff,
      live: false,
    };
    return next;
  }

  const lastWrote = [...list]
    .map((l, i) => ({ l, i }))
    .reverse()
    .find(({ l }) => l.kind === "wrote" && l.path === path);

  if (lastWrote && lastWrote.l.kind === "wrote" && lastWrote.l.live) {
    const next = [...list];
    next[lastWrote.i] = {
      kind: "wrote",
      path,
      diff: diff.trim() ? diff : lastWrote.l.diff,
      live: false,
    };
    return next;
  }

  return [...list, { kind: "wrote", path, diff, live: false }];
}

function upsertTrace(
  list: Line[],
  who: RoleId,
  text: string,
  live: boolean,
): Line[] {
  if (!text.trim()) return list;
  const mine = (l: Line) =>
    l.kind === "trace" && (l.from ?? who) === who;

  const foundLive = [...list]
    .map((l, i) => ({ l, i }))
    .reverse()
    .find(({ l }) => mine(l) && l.kind === "trace" && l.live);

  if (live) {
    if (foundLive && foundLive.l.kind === "trace") {
      const next = [...list];
      next[foundLive.i] = { kind: "trace", text, from: who, live: true };
      return next;
    }
    return [...list, { kind: "trace", text, from: who, live: true }];
  }

  if (foundLive && foundLive.l.kind === "trace") {
    const next = [...list];
    next[foundLive.i] = { kind: "trace", text, from: who, live: false };
    return next;
  }

  return [...list, { kind: "trace", text, from: who, live: false }];
}

function upsertThink(list: Line[], text: string, live: boolean, from?: RoleId): Line[] {
  const mine = (l: Line) =>
    l.kind === "think" && (from === undefined || l.from === from);
  const lastIdx = [...list]
    .map((l, i) => ({ l, i }))
    .reverse()
    .find(({ l }) => mine(l))?.i;
  if (live) {
    if (lastIdx != null && list[lastIdx]?.kind === "think" && list[lastIdx].live) {
      const last = list[lastIdx];
      if (last.kind === "think" && !text.trim() && last.text.trim()) {
        const next = [...list];
        next[lastIdx] = { ...last, live: false };
        next.push({ kind: "think", from, text: "", live: true });
        return next;
      }
      const next = [...list];
      next[lastIdx] = {
        kind: "think",
        from,
        text: text.trim() ? text : last.kind === "think" ? last.text : "",
        live: true,
      };
      return next;
    }
    return [...list, { kind: "think", from, text, live: true }];
  }
  if (lastIdx != null && list[lastIdx]?.kind === "think") {
    const last = list[lastIdx];
    const kept = (text.trim() ? text : last.kind === "think" ? last.text : "").trim();
    const next = [...list];
    if (!kept) {
      next.splice(lastIdx, 1);
      return next;
    }
    next[lastIdx] = { kind: "think", from, text: kept, live: false };
    return next;
  }
  if (!text.trim()) return list;
  return [...list, { kind: "think", from, text, live: false }];
}

function workAfterSpeak(list: Line[], from: RoleId, after: number) {
  return list.slice(after + 1).some((l) => {
    if (l.kind === "think") return (l.from ?? from) === from;
    if (l.kind === "trace") return (l.from ?? from) === from;
    if (l.kind === "todo") return (l.from ?? from) === from;
    if (l.kind === "wrote") return from === "coder";
    return false;
  });
}

function upsertSpeak(list: Line[], from: RoleId, text: string, live: boolean): Line[] {
  const lastFrom = [...list]
    .map((l, i) => ({ l, i }))
    .reverse()
    .find(({ l }) => l.kind === "them" && l.from === from);
  if (lastFrom && lastFrom.l.kind === "them") {
    const cur = lastFrom.l;
    if (workAfterSpeak(list, from, lastFrom.i)) {
      const next = [...list];
      if (cur.live) {
        next[lastFrom.i] = { ...cur, live: false };
      }
      if (!text.trim()) return next;
      return [...next, { kind: "them", from, text, live }];
    }
    const same =
      cur.live ||
      !text.trim() ||
      cur.text === text ||
      text.startsWith(cur.text) ||
      cur.text.startsWith(text);
    if (cur.live || (!live && same)) {
      const next = [...list];
      next[lastFrom.i] = {
        kind: "them",
        from,
        text: text.trim() ? text : cur.text,
        live,
      };
      return next;
    }
  }
  if (!text.trim()) return list;
  return [...list, { kind: "them", from, text, live }];
}

function upsertBrief(
  list: Line[],
  from: RoleId,
  to: RoleId,
  text: string,
  live: boolean,
): Line[] {
  const line: Line = {
    kind: "brief",
    from,
    to,
    text,
    live,
  };
  const patch = (cur: Extract<Line, { kind: "brief" }>): Line => ({
    ...line,
    text: text.trim() ? text : cur.text,
  });
  for (let i = list.length - 1; i >= 0; i--) {
    const cur = list[i]!;
    if (cur.kind === "brief" && cur.from === from && cur.to === to && cur.live) {
      const next = [...list];
      next[i] = patch(cur);
      return next;
    }
    if (cur.kind === "work") {
      const inner = cur.lines.findIndex(
        (x) => x.kind === "brief" && x.from === from && x.to === to && x.live,
      );
      if (inner >= 0) {
        const brief = cur.lines[inner];
        if (brief?.kind !== "brief") continue;
        const next = [...list];
        const lines = [...cur.lines];
        lines[inner] = patch(brief);
        next[i] = { ...cur, lines };
        return next;
      }
    }
  }
  if (!text.trim()) return list;
  for (let i = list.length - 1; i >= 0; i--) {
    const cur = list[i]!;
    if (cur.kind === "work" && (cur.from ?? to) === to) {
      const next = [...list];
      next[i] = { ...cur, lines: [line, ...cur.lines] };
      return next;
    }
  }
  return [...list, line];
}

function formatWorked(ms: number) {
  const s = Math.max(1, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const r = s % 60;
  if (m < 60) return r ? `${m}m ${r}s` : `${m}m`;
  const h = Math.floor(m / 60);
  const rm = m % 60;
  return rm ? `${h}h ${rm}m` : `${h}h`;
}

function sameTurnFollow(line: Line, who: RoleId) {
  if (line.kind === "brief" && line.from === who) return true;
  if (line.kind === "them" && (line.from ?? who) === who) return true;
  if (line.kind === "clip" && (line.from ?? "coder") === who) return true;
  if (line.kind === "trace" && (line.from ?? who) === who) return true;
  if (line.kind === "todo" && (line.from ?? who) === who) return true;
  if (line.kind === "wrote" && who === "coder") return true;
  return false;
}

function turnWho(line: Line): RoleId | null {
  if (line.kind === "trace") return line.from ?? null;
  if (line.kind === "wrote") return "coder";
  if (line.kind === "them" && line.from && line.from !== "coordinatore") return line.from;
  if (line.kind === "clip") return line.from ?? "coder";
  return null;
}

function isLooseTurnBit(line: Line, who: RoleId) {
  if (line.kind === "trace") return (line.from ?? who) === who;
  if (line.kind === "wrote") return who === "coder";
  if (line.kind === "them") return (line.from ?? who) === who;
  if (line.kind === "clip") return (line.from ?? "coder") === who;
  if (line.kind === "todo") return (line.from ?? who) === who;
  return false;
}

function freezeLiveSpeak(list: Line[], who: RoleId): Line[] {
  return list.map((line) =>
    line.kind === "them" && (line.from ?? who) === who && line.live
      ? { ...line, live: false }
      : line,
  );
}

function isWorkRole(role: RoleId) {
  return role === "coder";
}

function isRoleJobLine(line: Line, who: RoleId) {
  if (
    line.kind === "work" ||
    line.kind === "job" ||
    line.kind === "you" ||
    line.kind === "switch"
  ) {
    return false;
  }
  if (line.kind === "ask") return line.who === who;
  if (line.kind === "event") return true;
  if (line.kind === "think") return (line.from ?? "coordinatore") === who;
  if (line.kind === "brief") return line.to === who;
  if (line.kind === "them") return (line.from ?? who) === who;
  if (line.kind === "trace") return (line.from ?? who) === who;
  if (line.kind === "todo") return (line.from ?? who) === who;
  if (line.kind === "wrote") return who === "coder";
  if (line.kind === "clip") return false;
  return false;
}

function jobPreview(ms: number) {
  return `Task finished in ${formatWorked(ms)}`;
}

const KITCHEN_SHAPE = 3;

function isCrewKitchen(lines: Line[]) {
  return lines.some((l) => {
    if (l.kind === "work") return (l.from ?? "coder") !== "coordinatore";
    if (l.kind === "brief") return l.to !== "coordinatore";
    if (l.kind === "wrote") return true;
    if (l.kind === "clip") return true;
    if (l.kind === "them" && l.from && l.from !== "coordinatore") return true;
    if (l.kind === "think" && l.from && l.from !== "coordinatore") return true;
    return false;
  });
}

function isOtherCrewLine(line: Line) {
  if (line.kind === "work") return (line.from ?? "coder") !== "coordinatore";
  if (line.kind === "job" || line.kind === "wrote" || line.kind === "clip") {
    return true;
  }
  if (line.kind === "ask") return line.who !== "coordinatore";
  if (line.kind === "them") return !!line.from && line.from !== "coordinatore";
  if (line.kind === "think") return !!line.from && line.from !== "coordinatore";
  if (line.kind === "trace") return !!line.from && line.from !== "coordinatore";
  if (line.kind === "todo") return !!line.from && line.from !== "coordinatore";
  if (line.kind === "brief") {
    return line.to !== "coordinatore" && line.from !== "coordinatore";
  }
  return false;
}

function isCoordOwnLine(line: Line) {
  if (line.kind === "work") {
    return (
      (line.from ?? "coder") === "coordinatore" &&
      !line.lines.some(isOtherCrewLine)
    );
  }
  if (
    line.kind === "job" ||
    line.kind === "you" ||
    line.kind === "switch" ||
    line.kind === "clip" ||
    line.kind === "wrote"
  ) {
    return false;
  }
  if (line.kind === "ask") return line.who === "coordinatore";
  if (line.kind === "event") return true;
  if (line.kind === "think") {
    return (line.from ?? "coordinatore") === "coordinatore";
  }
  if (line.kind === "brief") {
    return (line.from ?? "coordinatore") === "coordinatore";
  }
  if (line.kind === "trace") {
    return (line.from ?? "coordinatore") === "coordinatore";
  }
  if (line.kind === "them") {
    return (line.from ?? "coordinatore") === "coordinatore";
  }
  return false;
}

function hasCopyFence(text: string) {
  return text.includes("```");
}

function kitchenHasHandoff(lines: Line[]) {
  return lines.some((l) => l.kind === "brief" && l.to !== "coordinatore");
}

function isCoordSpeak(line: Line) {
  return (
    line.kind === "them" &&
    (line.from ?? "coordinatore") === "coordinatore" &&
    (!line.live || hasCopyFence(line.text))
  );
}

function sinceLastOrder(list: Line[]): Line[] {
  for (let i = list.length - 1; i >= 0; i--) {
    if (list[i]!.kind === "you") return list.slice(i + 1);
  }
  return list;
}

function feedHasThem(list: Line[], who: RoleId, text: string): boolean {
  for (const line of list) {
    if (
      line.kind === "them" &&
      (line.from ?? who) === who &&
      line.text === text
    ) {
      return true;
    }
    if (line.kind === "work" && feedHasThem(line.lines, who, text)) return true;
    if (line.kind === "job" && feedHasThem(line.lines, who, text)) return true;
  }
  return false;
}

function feedHasThink(list: Line[], text: string): boolean {
  const want = text.trim();
  if (!want) return false;
  for (const line of list) {
    if (line.kind === "think" && line.text.trim() === want) return true;
    if (line.kind === "work" && feedHasThink(line.lines, text)) return true;
    if (line.kind === "job" && feedHasThink(line.lines, text)) return true;
  }
  return false;
}

function dropDupRoleSpeak(lines: Line[]): Line[] {
  return lines.filter((line) => {
    if (line.kind !== "them" || !line.from || line.from === "coordinatore") {
      return true;
    }
    const who = line.from;
    return !lines.some(
      (other) =>
        other.kind === "work" &&
        (other.from ?? who) === who &&
        feedHasThem(other.lines, who, line.text),
    );
  });
}

function peelFencedCoord(lines: Line[]): { kitchen: Line[]; report: Line[] } {
  const report: Line[] = [];
  const peelThem = (line: Line): Line | null => {
    if (
      line.kind === "them" &&
      (line.from ?? "coordinatore") === "coordinatore" &&
      hasCopyFence(line.text)
    ) {
      report.push({ ...line, live: false });
      return null;
    }
    return line;
  };
  const kitchen = lines.flatMap((line) => {
    if (line.kind === "work" && (line.from ?? "") === "coordinatore") {
      const inner = line.lines.flatMap((l) => {
        const kept = peelThem(l);
        return kept ? [kept] : [];
      });
      return [{ ...line, lines: inner }];
    }
    const kept = peelThem(line);
    return kept ? [kept] : [];
  });
  return { kitchen, report };
}

function splitCoordReport(lines: Line[]): { kitchen: Line[]; report: Line[] } {
  const peeled = peelFencedCoord(lines);
  const kitchen = [...peeled.kitchen];
  const clips: Line[] = [];
  const peelClips = () => {
    while (kitchen.length && kitchen[kitchen.length - 1]!.kind === "clip") {
      clips.unshift(kitchen.pop()!);
    }
  };
  peelClips();
  let seenCrewWork = false;
  let idx = -1;
  for (let i = 0; i < kitchen.length; i++) {
    const line = kitchen[i]!;
    if (
      (line.kind === "work" && line.from && line.from !== "coordinatore") ||
      line.kind === "wrote"
    ) {
      seenCrewWork = true;
    }
    if (seenCrewWork && isCoordSpeak(line)) idx = i;
  }
  let speak: Line | undefined;
  if (idx >= 0) {
    const [taken] = kitchen.splice(idx, 1);
    if (
      taken &&
      !(taken.kind === "them" && feedHasThink(kitchen, taken.text))
    ) {
      speak = taken;
    }
    peelClips();
  }
  const report: Line[] = [];
  if (clips.length) {
    const raw = clips
      .filter((line): line is Extract<Line, { kind: "clip" }> => line.kind === "clip")
      .map((line) =>
        isGenericClipTitle(line.title)
          ? line.text
          : `=== ${line.title} ===\n${line.text}`,
      )
      .join("\n\n");
    report.push(
      ...withCopyRecap(
        speak ? [speak] : [],
        raw,
        speak && speak.kind === "them" ? speak.text : null,
      ),
    );
  } else if (speak) {
    report.push(speak);
  }
  return { kitchen, report: [...peeled.report, ...report] };
}

function wrapCoordWork(lines: Line[], ms?: number): Line[] {
  const { kitchen: raw, report } = splitCoordReport(lines);
  const body = dropDupRoleSpeak(raw);
  const mine: Line[] = [];
  const rest: Line[] = [];
  let heldMs = 0;
  let cards = 0;
  for (const line of body) {
    if (
      line.kind === "work" &&
      (line.from ?? "coder") === "coordinatore" &&
      !line.lines.some(isOtherCrewLine)
    ) {
      mine.push(...line.lines);
      heldMs = Math.max(heldMs, line.ms);
      cards += 1;
    } else if (isCoordOwnLine(line)) {
      mine.push(line);
    } else {
      rest.push(line);
    }
  }
  if (!rest.length && !kitchenHasHandoff(mine)) {
    while (mine.length) {
      const last = mine[mine.length - 1]!;
      if (last.kind !== "them") break;
      report.unshift({ ...last, live: false });
      mine.pop();
    }
  }
  if (!mine.length) {
    if (!rest.length && !report.length) return lines;
    return [...rest, ...report];
  }
  if (cards === 1 && rest.length === body.length - 1 && !ms && !report.length) {
    return lines;
  }
  const elapsed = Math.max(1000, ms ?? heldMs);
  return [
    {
      kind: "work",
      from: "coordinatore",
      ms: elapsed,
      summary: `Coordinator worked for ${formatWorked(elapsed)}`,
      lines: mine,
    },
    ...rest,
    ...report,
  ];
}

function isLiveCoordBit(line: Line) {
  if (line.kind === "think" || line.kind === "brief" || line.kind === "them") {
    return (
      (line.from ?? "coordinatore") === "coordinatore" && !!line.live
    );
  }
  if (line.kind === "trace") {
    return (line.from ?? "coordinatore") === "coordinatore" && !!line.live;
  }
  return false;
}

function coordHandoff(tail: Line[]) {
  return tail.some((l) => {
    if (l.kind === "brief" && !l.live && l.to !== "coordinatore") return true;
    if (l.kind === "think" && l.from && l.from !== "coordinatore") return true;
    if (l.kind === "trace" && l.from && l.from !== "coordinatore") return true;
    if (l.kind === "work" && l.from && l.from !== "coordinatore") return true;
    if (l.kind === "wrote" || l.kind === "clip") return true;
    if (l.kind === "ask" && l.who !== "coordinatore") return true;
    return false;
  });
}

function foldLiveCoord(list: Line[], ms?: number): Line[] {
  let cut = -1;
  for (let i = list.length - 1; i >= 0; i--) {
    if (list[i]!.kind === "you") {
      cut = i;
      break;
    }
  }
  if (cut < 0) return list;
  const tail = list.slice(cut + 1);
  if (tail.some(isLiveCoordBit)) return list;
  if (kitchenReady(tail)) return list;
  if (!coordHandoff(tail)) return list;
  const next = wrapCoordWork(tail, ms);
  if (next === tail) return list;
  return [...list.slice(0, cut + 1), ...next];
}

function kitchenReady(lines: Line[]) {
  const { kitchen } = splitCoordReport(lines);
  for (const line of kitchen) {
    if (line.kind === "work" && (line.from ?? "coder") === "coordinatore") {
      if (line.lines.some(isOtherCrewLine)) return false;
      continue;
    }
    if (line.kind !== "work" && isCoordOwnLine(line)) return false;
  }
  return true;
}

function kitchenWho(lines: Line[]): RoleId[] {
  const who: RoleId[] = [];
  const add = (id: RoleId) => {
    if (who.includes(id)) return;
    who.push(id);
  };
  for (const line of lines) {
    if (line.kind === "work") add(line.from ?? "coder");
    else if (line.kind === "brief") add(line.to);
    else if (line.kind === "wrote") add("coder");
    else if (line.kind === "clip") add(line.from ?? "coder");
    else if (line.kind === "them" && line.from) add(line.from);
    else if (line.kind === "think" && line.from) add(line.from);
    else if (line.kind === "ask" && line.who) add(line.who);
  }
  return who;
}

function crewWorkMs(lines: Line[]) {
  let ms = 0;
  for (const line of lines) {
    if (line.kind === "work" && line.from && line.from !== "coordinatore") {
      ms += line.ms;
    }
  }
  return ms;
}

function isCoderLine(line: Line) {
  if (line.kind === "wrote") return true;
  if (
    line.kind === "work" ||
    line.kind === "you" ||
    line.kind === "job" ||
    line.kind === "ask" ||
    line.kind === "switch" ||
    line.kind === "event"
  ) {
    return false;
  }
  return line.from === "coder";
}

function looseWorkRole(line: Line): RoleId | null {
  if (line.kind === "ask") {
    return line.who !== "coordinatore" ? line.who : null;
  }
  if (isCoderLine(line)) return "coder";
  if (line.kind === "clip") return null;
  if (line.kind === "them" && line.from && line.from !== "coordinatore") {
    return line.from;
  }
  if (line.kind === "think" && line.from && line.from !== "coordinatore") {
    return line.from;
  }
  if (line.kind === "trace" && line.from && line.from !== "coordinatore") {
    return line.from;
  }
  if (line.kind === "todo" && line.from && line.from !== "coordinatore") {
    return line.from;
  }
  return null;
}

function collapseThinkRuns(lines: Line[]): Line[] {
  // Lo streaming del thinking arriva a pezzi crescenti: il kitchen tenere
  // solo l'ultimo pezzo di una serie continua dello stesso ruolo (niente
  // "Coordinator thought" ripetuto 10 volte).
  const out: Line[] = [];
  for (const line of lines) {
    const prev = out.at(-1);
    if (
      line.kind === "think" &&
      prev?.kind === "think" &&
      (prev.from ?? "coordinatore") === (line.from ?? "coordinatore")
    ) {
      out[out.length - 1] = {
        ...line,
        live: false,
        text: line.text || (prev.kind === "think" ? prev.text : ""),
      };
    } else {
      out.push(line);
    }
  }
  return out;
}

function restoreKitchen(lines: Line[]): Line[] {
  let flat: Line[] = [];
  let heldMs = 0;
  const walk = (items: Line[]) => {
    for (const line of items) {
      if (line.kind === "work" && (line.from ?? "coder") === "coordinatore") {
        heldMs = Math.max(heldMs, line.ms);
        if (line.lines.some(isOtherCrewLine)) walk(line.lines);
        else flat.push(line);
      } else {
        if (line.kind === "work") {
          heldMs = Math.max(heldMs, line.ms);
          flat.push({
            ...line,
            lines: line.lines.filter((inner) => inner.kind !== "clip"),
          });
        } else {
          flat.push(line);
        }
      }
    }
  };
  walk(lines);
  flat = collapseThinkRuns(flat);
  const out: Line[] = [];
  const loose: Partial<Record<RoleId, Line[]>> = {};
  const flush = (id: RoleId) => {
    const chunk = loose[id];
    if (!chunk?.length) return;
    const ms = Math.max(1000, heldMs);
    out.push({
      kind: "work",
      from: id,
      ms,
      summary: `${crewName(id)} worked for ${formatWorked(ms)}`,
      lines: chunk,
    });
    loose[id] = [];
  };
  const tuck = (id: RoleId, line: Line) => {
    for (let i = out.length - 1; i >= 0; i--) {
      const cur = out[i]!;
      if (cur.kind === "work" && (cur.from ?? id) === id) {
        out[i] = { ...cur, lines: [...cur.lines, line] };
        return;
      }
    }
    (loose[id] ??= []).push(line);
  };
  for (const line of flat) {
    if (line.kind === "work" && line.from && line.from !== "coordinatore") {
      const extra = loose[line.from] ?? [];
      loose[line.from] = [];
      out.push(extra.length ? { ...line, lines: [...extra, ...line.lines] } : line);
    } else {
      const id = looseWorkRole(line);
      if (id) tuck(id, line);
      else {
        flush("coder");
        out.push(line);
      }
    }
  }
  flush("coder");
  return out;
}

function foldOneOrder(
  tail: Line[],
  totalMs?: number,
  coordMs?: number,
): Line[] {
  const existing = tail[0]?.kind === "job" ? tail[0] : null;
  if (existing && kitchenReady(existing.lines) && !totalMs && !coordMs) {
    return tail;
  }

  let kitchen: Line[];
  let keep: Line[];
  if (existing) {
    kitchen = restoreKitchen(existing.lines);
    keep = tail.slice(1);
  } else {
    keep = [];
    kitchen = [...tail];
    const split = splitCoordReport(kitchen);
    kitchen = split.kitchen;
    keep = split.report;
    if (!kitchen.length) return keep.length ? keep : tail;
    kitchen = restoreKitchen(kitchen);
  }

  kitchen = wrapCoordWork(kitchen, coordMs);
  const after = splitCoordReport(dropDupRoleSpeak(kitchen));
  kitchen = after.kitchen;
  keep = [...after.report, ...keep];
  const crew = isCrewKitchen(kitchen);
  const coordOnly = kitchen.some(
    (l) => l.kind === "work" && (l.from ?? "coder") === "coordinatore",
  );
  if (!crew && !coordOnly) return tail;
  if (!crew) return [...kitchen, ...keep];

  const ms = totalMs ?? existing?.ms ?? Math.max(1000, crewWorkMs(kitchen));
  if (
    existing &&
    kitchenReady(existing.lines) &&
    existing.ms === ms &&
    !coordMs
  ) {
    return tail;
  }
  return [
    {
      kind: "job",
      ms,
      who: kitchenWho(kitchen),
      lines: kitchen,
    },
    ...keep,
  ];
}

function foldOrderKitchen(
  list: Line[],
  totalMs?: number,
  coordMs?: number,
): Line[] {
  const out: Line[] = [];
  let i = 0;
  while (i < list.length) {
    const line = list[i]!;
    if (line.kind !== "you") {
      out.push(line);
      i += 1;
      continue;
    }
    out.push(line);
    i += 1;
    const start = i;
    while (i < list.length && list[i]!.kind !== "you") i += 1;
    const last = i >= list.length;
    out.push(
      ...foldOneOrder(
        list.slice(start, i),
        last ? totalMs : undefined,
        last ? coordMs : undefined,
      ),
    );
  }
  if (out.length === list.length && out.every((l, i) => l === list[i])) return list;
  return out;
}

function wrapJob(
  list: Line[],
  who: RoleId,
  started: number | null,
  heard: string | null,
): Line[] {
  if (who === "coordinatore") return list;
  let start = list.length;
  while (start > 0 && isRoleJobLine(list[start - 1]!, who)) start -= 1;
  if (start >= list.length) return list;
  const chunk = list.slice(start);
  const elapsed = started ? Math.max(1000, Date.now() - started) : 1000;
  const prev = start > 0 ? list[start - 1] : null;
  if (prev && prev.kind === "work" && (prev.from ?? who) === who) {
    return [
      ...list.slice(0, start - 1),
      {
        ...prev,
        ms: started ? Math.max(prev.ms, elapsed) : prev.ms,
        summary: heard?.trim() || prev.summary,
        lines: [...prev.lines, ...chunk],
      },
    ];
  }
  return [
    ...list.slice(0, start),
    {
      kind: "work",
      ms: elapsed,
      summary: heard?.trim() || "Worked",
      lines: chunk,
      from: who,
    },
  ];
}

function fitCompose(el: HTMLTextAreaElement) {
  const styles = getComputedStyle(el);
  const font = parseFloat(styles.fontSize) || 14;
  const line = parseFloat(styles.lineHeight);
  const linePx = Number.isFinite(line) && line >= font ? line : font * 1.4;
  const pad =
    (parseFloat(styles.paddingTop) || 0) +
    (parseFloat(styles.paddingBottom) || 0);
  const maxH = linePx * 5 + pad;
  el.style.height = "0px";
  el.style.height = `${Math.min(maxH, Math.max(linePx + pad, el.scrollHeight))}px`;
}

function foldRoleWork(
  list: Line[],
  who: RoleId,
  started: number | null,
  heard: string | null,
): Line[] {
  let next = freezeLiveSpeak(closeLiveThink(list, who), who);
  const note = heard?.trim() || null;
  if (note) {
    next = upsertSpeak(next, who, note, false);
  }
  return wrapJob(next, who, started, note);
}

export default function App() {
  const [draft, setDraft] = useState("");
  const [pending, setPending] = useState<PendingFile[]>([]);
  const [dragging, setDragging] = useState(false);
  const [busy, setBusy] = useState(false);
  const [talking, setTalking] = useState<RoleId | null>(null);
  const [woken, setWoken] = useState<RoleId[]>([]);
  const [threads, setThreads] = useState<Record<RoleId, Line[]>>(emptyThreads);
  const [feed, setFeed] = useState<Line[]>([]);
  const [crewTodos, setCrewTodos] = useState<Partial<Record<RoleId, TodoItem[]>>>({});
  const [packing, setPacking] = useState<RoleId | null>(null);
  const [packComune, setPackComune] = useState(false);
  const [recent, setRecent] = useState<RoleId[]>([]);
  const [clearAsk, setClearAsk] = useState(false);
  const [userAsk, setUserAsk] = useState<{ who: RoleId; question: string } | null>(null);
  const [askDraft, setAskDraft] = useState("");
  const [ready, setReady] = useState(false);
  const [workspace, setWorkspace] = useState<WorkspaceInfo | null>(null);
  const [cloud, setCloud] = useState<CloudStatus | null>(null);
  const [cloudEmail, setCloudEmail] = useState("");
  const [cloudBusy, setCloudBusy] = useState(false);
  const [cloudMsg, setCloudMsg] = useState<string | null>(null);
  const [cloudNotice, setCloudNotice] = useState<string | null>(null);
  const [cloudDevDismissed, setCloudDevDismissed] = useState(false);
  const threadRef = useRef<HTMLDivElement>(null);
  const stickBottom = useRef(true);
  const ignoreScroll = useRef(false);
  const thinkLive = useRef<Partial<Record<RoleId, boolean>>>({});
  const jobAt = useRef<Partial<Record<RoleId, number>>>({});
  const orderAt = useRef<number | null>(null);
  const coordStart = useRef<number | null>(null);
  const coordAcc = useRef(0);
  const fileRef = useRef<HTMLInputElement>(null);
  const composeRef = useRef<HTMLTextAreaElement>(null);
  const cliRef = useRef<((cmd: CliCmd) => Promise<void>) | undefined>(
    undefined,
  );
  cliRef.current = runCli;

  useEffect(() => {
    void invoke("cloud_status")
      .then((v) => {
        const s = v as CloudStatus;
        setCloud(s);
        // Il pulse del backend arriva prima del mount: lo ripropongo qui,
        // una volta per apertura, quando il workspace è connesso.
        if (s.configured && s.connected) {
          setCloudNotice("Workspace ready");
          window.setTimeout(() => setCloudNotice(null), 6000);
        }
      })
      .catch(() => setCloud(null));
  }, []);

  // Pulse del cloud: "Preparing workspace…", "Pushing on cloud…", login completato.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let t: number | undefined;
    void listen<CrewPulse & { text?: string }>("puck-crew", (ev) => {
      const pulse = ev.payload as { kind?: string; text?: string };
      if (pulse.kind === "cloud" && typeof pulse.text === "string") {
        setCloudNotice(pulse.text);
        if (t) window.clearTimeout(t);
        t = window.setTimeout(() => setCloudNotice(null), 6000);
      } else if (pulse.kind === "cloud_auth") {
        setCloud((prev) => (prev ? { ...prev, signed_in: true } : prev));
        void invoke("cloud_status")
          .then((v) => setCloud(v as CloudStatus))
          .catch(() => {});
      }
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      unlisten?.();
      if (t) window.clearTimeout(t);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Ritorno dal magic link (dev: http://localhost:1420/auth?code=…).
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const code = params.get("code");
    if (!code) return;
    window.history.replaceState({}, "", window.location.pathname);
    void invoke<{ email?: string }>("cloud_auth_callback", { code })
      .then((r) => {
        const email = r.email ?? "";
        setCloud((prev) => (prev ? { ...prev, signed_in: true, email } : prev));
        void invoke("cloud_status")
          .then((v) => setCloud(v as CloudStatus))
          .catch(() => {});
        const line = email
          ? `Puck Cloud collegato a ${email}. Preparo il workspace…`
          : "Puck Cloud collegato. Preparo il workspace…";
        setThreads((prev) => ({
          ...prev,
          coordinatore: [...prev.coordinatore, { kind: "event", text: line }],
        }));
        setFeed((prev) => [...prev, { kind: "event", text: line }]);
      })
      .catch((err) => {
        const msg =
          typeof err === "string" ? err : "Autenticazione fallita. Riprova dal pulsante.";
        setThreads((prev) => ({
          ...prev,
          coordinatore: [...prev.coordinatore, { kind: "event", text: msg }],
        }));
        setFeed((prev) => [...prev, { kind: "event", text: msg }]);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function connectCloud() {
    const email = cloudEmail.trim();
    if (!email || cloudBusy) return;
    setCloudBusy(true);
    setCloudMsg(null);
    try {
      await invoke("cloud_connect", { email });
      setCloudMsg(
        `We sent a sign-in link to ${email}. Open it to finish setting up your workspace.`,
      );
    } catch (err) {
      setCloudMsg(
        typeof err === "string" ? err : "Errore di connessione. Riprova tra poco.",
      );
    }
    setCloudBusy(false);
  }

  useEffect(() => {
    const el = composeRef.current;
    if (el) fitCompose(el);
  }, [draft]);

  function pauseCoord() {
    if (coordStart.current != null) {
      coordAcc.current += Date.now() - coordStart.current;
      coordStart.current = null;
    }
  }

  function resumeCoord() {
    if (coordStart.current == null) coordStart.current = Date.now();
  }

  function touchJob(who: RoleId | null | undefined) {
    if (!who || !isWorkRole(who)) return;
    pauseCoord();
    if (!jobAt.current[who]) jobAt.current[who] = Date.now();
  }

  function takeJob(who: RoleId) {
    const t = jobAt.current[who] ?? null;
    delete jobAt.current[who];
    return t;
  }

  function clearCrewTodo(who: RoleId) {
    setCrewTodos((prev) => {
      if (!prev[who]?.length) return prev;
      const next = { ...prev };
      delete next[who];
      return next;
    });
  }

  // Si scrive solo con workspace allineato (o in dev senza chiavi).
  const canWrite = cloud
    ? !cloud.configured || (cloud.signed_in && cloud.connected)
    : true;
  const lines = feed;
  const roster = useMemo(() => {
    const head = CREW.find((r) => r.id === "coordinatore")!;
    const rest = CREW.filter((r) => r.id !== "coordinatore");
    const rank = new Map(recent.map((id, i) => [id, i]));
    rest.sort((a, b) => (rank.get(a.id) ?? 1000) - (rank.get(b.id) ?? 1000));
    return [head, ...rest];
  }, [recent]);


  useEffect(() => {
    let gone = false;
    let unlistenWs: (() => void) | undefined;
    listen<WorkspaceInfo>("puck-workspace", (ev) => {
      if (!gone) setWorkspace(ev.payload);
    }).then((fn) => {
      if (gone) fn();
      else unlistenWs = fn;
    });
    invoke<WorkspaceInfo>("get_workspace")
      .then((info) => {
        if (!gone) setWorkspace(info);
      })
      .catch(() => {});
    invoke<unknown>("load_ui_session")
      .then((raw) => {
        if (gone) return;
        const session = readSession(raw);
        if (!session) return;
        if (session.woken) setWoken(asRoleList(session.woken));
        if (session.recent) setRecent(asRoleList(session.recent));
        const loadedThreads = session.threads
          ? asThreads(session.threads)
          : emptyThreads();
        setThreads(loadedThreads);
        const loadedFeed = asFeed(session.feed);
        setFeed(
          foldOrderKitchen(
            loadedFeed.length ? loadedFeed : feedFromThreads(loadedThreads),
          ),
        );
        const fromSession = asCrewTodos(session.crewTodos);
        const fromFeed = crewTodosFromLines(
          loadedFeed.length ? loadedFeed : feedFromThreads(loadedThreads),
        );
        const fromThreads = crewTodosFromLines(
          Object.values(loadedThreads).flat(),
        );
        setCrewTodos({ ...fromThreads, ...fromFeed, ...fromSession });
      })
      .catch(() => {})
      .finally(() => {
        if (!gone) setReady(true);
      });
    return () => {
      gone = true;
      unlistenWs?.();
    };
  }, []);

  useEffect(() => {
    if (!ready || busy) return;
    setFeed((prev) => foldOrderKitchen(prev));
  }, [ready, busy, KITCHEN_SHAPE]);

  useEffect(() => {
    if (!ready) return;
    const session = {
      version: 1,
      activeId: "coordinatore" as RoleId,
      woken,
      recent,
      threads,
      feed,
      crewTodos,
    };
    const save = () => {
      void invoke("save_ui_session", { session });
    };
    const timer = window.setTimeout(save, 400);
    const onHide = () => {
      if (document.visibilityState === "hidden") save();
    };
    document.addEventListener("visibilitychange", onHide);
    window.addEventListener("beforeunload", save);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", onHide);
      window.removeEventListener("beforeunload", save);
    };
  }, [ready, woken, recent, threads, feed, crewTodos]);

  useEffect(() => {
    const el = threadRef.current;
    if (!el || !stickBottom.current) return;
    ignoreScroll.current = true;
    el.scrollTop = el.scrollHeight;
  }, [lines, busy, talking, packing]);

  useEffect(() => {
    if (!busy && !talking && !packing) return;
    let raf = 0;
    const tick = () => {
      const el = threadRef.current;
      if (el && stickBottom.current) {
        ignoreScroll.current = true;
        el.scrollTop = el.scrollHeight;
      }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [busy, talking, packing]);

  useEffect(() => {
    let gone = false;
    let unlisten: (() => void) | undefined;
    listen<CrewPulse>("puck-crew", (ev) => {
      const pulse = ev.payload;
      const pack = (list: Line[]) =>
        foldLiveCoord(
          list,
          Math.max(
            1000,
            coordAcc.current +
              (coordStart.current != null
                ? Date.now() - coordStart.current
                : 0),
          ),
        );
      if (pulse.kind === "say" && pulse.text) {
        thinkLive.current.coordinatore = false;
        const said = pulse.text;
        setThreads((prev) => {
          const list = closeLiveThink(prev.coordinatore);
          if (feedHasThink(list, said)) {
            return list === prev.coordinatore ? prev : { ...prev, coordinatore: list };
          }
          const last = list.at(-1);
          if (last && last.kind === "them" && last.text === said) {
            return list === prev.coordinatore ? prev : { ...prev, coordinatore: list };
          }
          return {
            ...prev,
            coordinatore: [
              ...list,
              { kind: "them", text: said, from: "coordinatore" },
            ],
          };
        });
        setFeed((prev) => {
          const next = closeLiveThink(prev, "coordinatore");
          if (feedHasThink(next, said)) return pack(next);
          return pack(upsertSpeak(next, "coordinatore", said, false));
        });
        return;
      }
      if (pulse.kind === "speak") {
        const who = asRoleId(pulse.role) ?? "coordinatore";
        if (isWorkRole(who)) return;
        bumpRole(setRecent, who);
        setTalking(who);
        const live = pulse.brief !== "done";
        const text = pulse.text || "";
        setFeed((prev) =>
          pack(upsertSpeak(prev, who, text, live)),
        );
        return;
      }
      if (pulse.kind === "brief") {
        const to = asRoleId(pulse.role);
        const from = asRoleId(pulse.from) ?? "coordinatore";
        if (!to) return;
        bumpRole(setRecent, to);
        setTalking(from);
        setWoken((prev) => (prev.includes(to) ? prev : [...prev, to]));
        setFeed((prev) => {
          const live = pulse.brief !== "done";
          const next = closeLiveThink(
            upsertBrief(prev, from, to, pulse.text || "", live),
            "coordinatore",
          );
          return live ? next : pack(next);
        });
        return;
      }
      const role = asRoleId(pulse.role);
      if (pulse.kind === "busy" && role) {
        bumpRole(setRecent, role);
        setTalking(role);
        setWoken((prev) => (prev.includes(role) ? prev : [...prev, role]));
        touchJob(role);
        return;
      }
      if (pulse.kind === "talk" && role) {
        bumpRole(setRecent, role);
        setTalking(role);
        setWoken((prev) => (prev.includes(role) ? prev : [...prev, role]));
        touchJob(role);
        const from = asRoleId(pulse.from) ?? "coordinatore";
        setThreads((prev) => ({
          ...prev,
          [role]: upsertThink(
            [
              ...prev[role],
              {
                kind: "them",
                text: pulse.brief || pulse.text || "",
                from,
              },
            ],
            "",
            true,
            role,
          ),
        }));
        setFeed((prev) =>
          pack(
            upsertThink(
              closeLiveThink(
                upsertBrief(prev, from, role, pulse.brief || pulse.text || "", false),
                "coordinatore",
              ),
              "",
              true,
              role,
            ),
          ),
        );
        return;
      }
      if (pulse.kind === "think" && role) {
        bumpRole(setRecent, role);
        setTalking(role);
        touchJob(role);
        const live = pulse.brief !== "done";
        const text = pulse.text || "";
        if (live && !thinkLive.current[role]) {
          thinkLive.current[role] = true;
        }
        if (!live) thinkLive.current[role] = false;
        setThreads((prev) => ({
          ...prev,
          [role]: upsertThink(prev[role], text, live, role),
        }));
        setFeed((prev) => pack(upsertThink(prev, text, live, role)));
        return;
      }
      if (pulse.kind === "todo" && role && role !== "coordinatore") {
        const items = parseTodoItems(pulse.text);
        bumpRole(setRecent, role);
        setTalking(role);
        touchJob(role);
        setCrewTodos((prev) => {
          const merged = mergeCrewTodos(prev[role] ?? [], items);
          if (!merged.length) {
            if (!prev[role]?.length) return prev;
            const next = { ...prev };
            delete next[role];
            return next;
          }
          return { ...prev, [role]: merged };
        });
        return;
      }
      if (pulse.kind === "trace" && pulse.role === "coder" && pulse.text) {
        const who = pulse.role as RoleId;
        bumpRole(setRecent, who);
        setTalking(who);
        touchJob(who);
        setThreads((prev) => ({
          ...prev,
          [who]: upsertTrace(prev[who], who, pulse.text!, pulse.brief === "live"),
        }));
        setFeed((prev) => upsertTrace(prev, who, pulse.text!, pulse.brief === "live"));
        return;
      }
      if (pulse.kind === "comune" && role) {
        bumpRole(setRecent, role);
        setTalking(role);
        setFeed((prev) => upsertTrace(prev, role, "Updated owner memory", false));
        return;
      }
      if (pulse.kind === "project" && role) {
        bumpRole(setRecent, role);
        setTalking(role);
        setFeed((prev) => upsertTrace(prev, role, "Updated project memory", false));
        return;
      }
      if (pulse.kind === "workspace") {
        void invoke<WorkspaceInfo>("get_workspace")
          .then((w) => setWorkspace(w))
          .catch(() => {});
        return;
      }
      if (pulse.kind === "search" && role) {
        bumpRole(setRecent, role);
        setTalking(role);
        touchJob(role);
        const label = pulse.text ? `Searched: ${pulse.text}` : "Searched the web";
        setFeed((prev) => pack(upsertTrace(prev, role, label, false)));
        return;
      }
      if (pulse.kind === "read" && role) {
        bumpRole(setRecent, role);
        setTalking(role);
        touchJob(role);
        const label = pulse.text ? `Read: ${pulse.text}` : "Read a page";
        setFeed((prev) => pack(upsertTrace(prev, role, label, false)));
        return;
      }
      if (pulse.kind === "wrote" && pulse.path) {
        bumpRole(setRecent, "coder");
        setTalking("coder");
        touchJob("coder");
        const live = pulse.brief === "live";
        setThreads((prev) => ({
          ...prev,
          coder: upsertWrote(prev.coder, pulse.path!, pulse.text || "", live),
        }));
        setFeed((prev) => upsertWrote(prev, pulse.path!, pulse.text || "", live));
        return;
      }
      if (pulse.kind === "heard" && role && isWorkRole(role)) {
        bumpRole(setRecent, role);
        const started = takeJob(role);
        thinkLive.current[role] = false;
        const heard = pulse.text || null;
        clearCrewTodo(role);
        setThreads((prev) => {
          const next = foldRoleWork(prev[role], role, started, heard);
          return next === prev[role] ? prev : { ...prev, [role]: next };
        });
        setFeed((prev) => pack(foldRoleWork(prev, role, started, heard)));
        resumeCoord();
        return;
      }
      if (pulse.kind === "pack" && role) {
        setPacking(role);
        setPackComune(pulse.text === "comune" || pulse.text === "project");
        return;
      }
      if (pulse.kind === "packed") {
        setPacking(null);
        setPackComune(false);
        if (role) {
          const label =
            pulse.text === "comune"
              ? "Compacted owner memory."
              : pulse.text === "project"
                ? "Compacted project memory."
                : "Compacted.";
          setThreads((prev) => ({
            ...prev,
            [role]: [
              ...prev[role],
              {
                kind: "event",
                text: label,
              },
            ],
          }));
          setFeed((prev) => [
            ...prev,
            {
              kind: "event",
              text: label,
            },
          ]);
        }
        return;
      }
      if (pulse.kind === "idle") {
        if (role && isWorkRole(role)) {
          thinkLive.current[role] = false;
          const started = takeJob(role);
          clearCrewTodo(role);
          setThreads((prev) => {
            const next = foldRoleWork(prev[role], role, started, null);
            return next === prev[role] ? prev : { ...prev, [role]: next };
          });
          setFeed((prev) => pack(foldRoleWork(prev, role, started, null)));
          resumeCoord();
        } else if (role) {
          thinkLive.current[role] = false;
          setThreads((prev) => ({
            ...prev,
            [role]: closeLiveThink(prev[role]),
          }));
          setFeed((prev) => closeLiveThink(prev, role));
        }
        setTalking(asRoleId(pulse.from) ?? "coordinatore");
        return;
      }
    }).then((fn) => {
      if (gone) fn();
      else unlisten = fn;
    });
    return () => {
      gone = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    let gone = false;
    let unlisten: (() => void) | undefined;
    listen<{ who: string; question: string }>("puck-ask", (ev) => {
      const who = asRoleId(ev.payload.who) ?? "coordinatore";
      setTalking(who);
      setAskDraft("");
      setUserAsk({ who, question: ev.payload.question });
    }).then((fn) => {
      if (gone) fn();
      else unlisten = fn;
    });
    return () => {
      gone = true;
      unlisten?.();
    };
  }, []);

  // Dev-only terminal channel (cli.rs). One listener, current runCli
  // via ref so busy / box / pending are today's, not the mount's.
  useEffect(() => {
    let gone = false;
    let unlisten: (() => void) | undefined;
    listen<CliCmd>("puck-cli", (ev) => {
      void cliRef.current?.(ev.payload ?? {});
    }).then((fn) => {
      if (gone) fn();
      else unlisten = fn;
    });
    return () => {
      gone = true;
      unlisten?.();
    };
  }, []);

  async function addFiles(list: FileList | File[]) {
    const incoming = Array.from(list);
    if (!incoming.length) return;
    const extra: PendingFile[] = [];
    for (const file of incoming) {
      if (file.size > MAX_ATTACH_BYTES) continue;
      const data = await fileToBase64(file);
      extra.push({
        name: file.name,
        data,
        preview: file.type.startsWith("image/") ? URL.createObjectURL(file) : undefined,
      });
    }
    if (!extra.length) return;
    setPending((prev) => [...prev, ...extra].slice(0, MAX_ATTACH));
  }

  async function applyFolder(path: string) {
    const info = await invoke<WorkspaceInfo>("set_workspace", { path });
    const prev = workspace;
    setWorkspace(info);
    if (!prev || prev.path === info.path) return;
    const line: Line = {
      kind: "switch",
      fromPath: prev.path,
      fromName: prev.name,
      toPath: info.path,
      toName: info.name,
    };
    setFeed((feed) => [...feed, line]);
    setThreads((prevThreads) => {
      const next = { ...prevThreads };
      for (const role of CREW) {
        next[role.id] = [...prevThreads[role.id], line];
      }
      return next;
    });
  }

  async function addFilesFromPaths(paths: string[]) {
    const extra: PendingFile[] = [];
    for (const path of paths) {
      try {
        const file = await invoke<{
          name: string;
          data: string;
          image: boolean;
        }>("cli_load_file", { path });
        extra.push({
          name: file.name,
          data: file.data,
          preview: file.image
            ? `data:image/*;base64,${file.data}`
            : undefined,
        });
      } catch {
        /* skip a bad path, same as a file the drop would refuse */
      }
    }
    if (!extra.length) return 0;
    setPending((prev) => [...prev, ...extra].slice(0, MAX_ATTACH));
    return extra.length;
  }

  async function cliAck(ok: boolean, extra?: Record<string, unknown>) {
    try {
      await invoke("cli_ack", { payload: { ok, ...extra } });
    } catch {
      /* release build, or the window is not the debug channel */
    }
  }

  async function runCli(cmd: CliCmd) {
    const op = (cmd.op ?? "").trim() || "say";
    switch (op) {
      case "say": {
        if (busy) {
          await cliAck(false, { op, err: "busy" });
          return;
        }
        const text = (cmd.text ?? "").trim();
        if (!text && pending.length === 0) {
          await cliAck(false, { op, err: "empty" });
          return;
        }
        setDraft(text);
        await cliAck(true, { op });
        await submitOrder(text);
        return;
      }
      case "cloud-connect": {
        const email = (cmd.name ?? "").trim();
        if (!email) {
          await cliAck(false, { op, err: "missing-email" });
          return;
        }
        try {
          await invoke("cloud_connect", { email });
          await cliAck(true, { op });
        } catch (err) {
          await cliAck(false, { op, err: String(err) });
        }
        return;
      }
      case "answer": {
        if (!userAsk) {
          await cliAck(false, { op, err: "no-question" });
          return;
        }
        await replyAsk((cmd.text ?? "").trim());
        await cliAck(true, { op });
        return;
      }
      case "skip": {
        if (!userAsk) {
          await cliAck(false, { op, err: "no-question" });
          return;
        }
        await replyAsk("");
        await cliAck(true, { op });
        return;
      }
      case "clear": {
        if (busy) {
          await cliAck(false, { op, err: "busy" });
          return;
        }
        if (!hasScreen()) {
          await cliAck(false, { op, err: "empty" });
          return;
        }
        clearView();
        await cliAck(true, { op });
        return;
      }
      case "folder": {
        if (busy) {
          await cliAck(false, { op, err: "busy" });
          return;
        }
        const path = (cmd.path ?? "").trim();
        if (!path) {
          await cliAck(false, { op, err: "missing-path" });
          return;
        }
        try {
          await applyFolder(path);
          await cliAck(true, { op });
        } catch (err) {
          await cliAck(false, {
            op,
            err: typeof err === "string" ? err : "folder",
          });
        }
        return;
      }
      case "attach": {
        if (busy) {
          await cliAck(false, { op, err: "busy" });
          return;
        }
        const paths = cmd.paths?.length
          ? cmd.paths
          : cmd.path
            ? [cmd.path]
            : [];
        if (!paths.length) {
          await cliAck(false, { op, err: "missing-path" });
          return;
        }
        const n = await addFilesFromPaths(paths);
        if (!n) {
          await cliAck(false, { op, err: "no-files" });
          return;
        }
        await cliAck(true, { op, attached: n });
        return;
      }
      case "detach": {
        const name = (cmd.name ?? "").trim();
        if (!name) {
          await cliAck(false, { op, err: "missing-name" });
          return;
        }
        if (!pending.some((file) => file.name === name)) {
          await cliAck(false, { op, err: "not-attached" });
          return;
        }
        setPending((prev) => prev.filter((file) => file.name !== name));
        await cliAck(true, { op });
        return;
      }
      default:
        await cliAck(false, { op, err: "unknown" });
    }
  }

  async function send(e: FormEvent) {
    e.preventDefault();
    await submitOrder(draft);
  }

  // Core of send(), split out so a dev-only terminal channel (cli.rs,
  // "puck-cli" / op say) can submit an order through the exact same
  // path a typed-and-Enter order takes — same history, same session,
  // no bypass of the model call.
  async function submitOrder(rawText: string) {
    if (busy) return;
    if (cloud && cloud.configured && !(cloud.signed_in && cloud.connected)) {
      const line = cloud.signed_in
        ? "Preparing workspace… wait a moment, then retry (the button under the chat)."
        : "Sign in to Puck Cloud first: open the email we sent you and confirm access.";
      setThreads((prev) => ({
        ...prev,
        coordinatore: [...prev.coordinatore, { kind: "event", text: line }],
      }));
      setFeed((prev) => [...prev, { kind: "event", text: line }]);
      return;
    }
    const text = rawText.trim();
    if (!text && pending.length === 0) return;

    let order = text;
    let filesForBubble: { name: string; preview?: string }[] | undefined;
    if (pending.length) {
      try {
        const saved = await invoke<string[]>("save_workspace_files", {
          files: pending.map((file) => ({ name: file.name, data: file.data })),
        });
        const line = `Files in the workspace: ${saved.join(", ")}`;
        order = order ? `${order}\n\n${line}` : `Use these files.\n\n${line}`;
        filesForBubble = pending.map((file, i) => ({
          name: saved[i] ?? file.name,
          preview: file.preview,
        }));
        setPending([]);
      } catch (err) {
        const msg =
          typeof err === "string"
            ? err
            : err instanceof Error
              ? err.message
              : "Could not save the files.";
        setThreads((prev) => ({
          ...prev,
          coordinatore: [...prev.coordinatore, { kind: "event", text: msg }],
        }));
        setFeed((prev) => [...prev, { kind: "event", text: msg }]);
        return;
      }
    }

    const history: { role: string; content: string }[] = [];
    for (const l of threads.coordinatore) {
      if (l.kind === "switch") history.push({ role: "user", content: switchLineText(l) });
      else if (l.kind === "you") history.push({ role: "user", content: l.text });
      else if (l.kind === "ask") {
        history.push({ role: "assistant", content: askContextText(l) });
      } else if (l.kind === "them" && !l.live) {
        history.push({ role: "assistant", content: l.text });
      }
    }
    history.push({ role: "user", content: order });
    const awake = ["coordinatore", ...woken];

    const crew_threads: Record<string, { role: string; content: string }[]> = {};
    for (const role of CREW) {
      if (role.id === "coordinatore") continue;
      const turns = threadTurns(role.id, threads[role.id]);
      if (turns.length) crew_threads[role.id] = turns;
    }

    setDraft("");
    setBusy(true);
    setTalking("coordinatore");
    orderAt.current = Date.now();
    coordAcc.current = 0;
    coordStart.current = Date.now();
    thinkLive.current.coordinatore = true;
    setThreads((prev) => ({
      ...prev,
      coordinatore: upsertThink(
        [...prev.coordinatore, { kind: "you", text: order, files: filesForBubble }],
        "",
        true,
        "coordinatore",
      ),
    }));
    setFeed((prev) =>
      upsertThink(
        [...prev, { kind: "you", text: order, files: filesForBubble }],
        "",
        true,
        "coordinatore",
      ),
    );

    try {
      const reply = await invoke<AskReply>("ask_coordinatore", {
        history,
        awake,
        crew_threads,
      });
      // Il Coordinatore può avere aperto un progetto durante l'ordine:
      // aggiorna l'etichetta del workspace.
      void invoke<WorkspaceInfo>("get_workspace")
        .then((w) => setWorkspace(w))
        .catch(() => {});
      const woke = reply.woke
        .map((id) => asRoleId(id))
        .filter((id): id is RoleId => id !== null);
      const work: WorkPiece[] = reply.work.flatMap((piece) => {
        const role = asRoleId(piece.role);
        return role ? [{ role, brief: piece.brief, text: piece.text }] : [];
      });
      setWoken((prev) => {
        const next = [...prev];
        for (const id of woke) {
          if (!next.includes(id)) next.push(id);
        }
        return next;
      });
      setThreads((prev) => {
        const next: Record<RoleId, Line[]> = { ...prev };
        next.coordinatore = closeLiveThink(next.coordinatore);
        const copies = work.filter(userFacingCopy);
        const lastCopy = copies.at(-1);
        const spoken =
          reply.text &&
          !feedHasThink(sinceLastOrder(next.coordinatore), reply.text) &&
          !(lastCopy && copyEcho(reply.text, lastCopy.text))
            ? reply.text
            : "";
        if (lastCopy) {
          next.coordinatore = withCopyRecap(
            next.coordinatore,
            lastCopy.text,
            spoken,
          );
        } else if (spoken) {
          const already = sinceLastOrder(next.coordinatore).some(
            (l) => l.kind === "them" && l.text === spoken,
          );
          if (!already) {
            next.coordinatore = [
              ...next.coordinatore,
              { kind: "them", text: spoken, from: "coordinatore" },
            ];
          }
        }
        for (const piece of work) {
          const hasBrief = next[piece.role].some(
            (l) => l.kind === "them" && l.text === piece.brief,
          );
          const hasNote = next[piece.role].some(
            (l) => l.kind === "them" && l.text === piece.text,
          );
          const extra: Line[] = [];
          if (piece.brief && !hasBrief) {
            extra.push({
              kind: "them",
              text: piece.brief,
              from: asRoleId(piece.from) ?? "coordinatore",
            });
          }
          if (piece.text && !hasNote) {
            extra.push({ kind: "them", text: piece.text, from: piece.role });
          }
          if (extra.length) {
            next[piece.role] = [...closeLiveThink(next[piece.role]), ...extra];
          }
        }
        return next;
      });
      setFeed((prev) => {
        let next = closeLiveThink(prev);
        const copies = work.filter(userFacingCopy);
        const lastCopy = copies.at(-1);
        const spoken =
          reply.text &&
          !feedHasThink(sinceLastOrder(next), reply.text) &&
          !(lastCopy && copyEcho(reply.text, lastCopy.text))
            ? reply.text
            : "";
        if (spoken && !lastCopy) {
          if (!feedHasThem(sinceLastOrder(next), "coordinatore", spoken)) {
            next = upsertSpeak(next, "coordinatore", spoken, false);
          }
        }
        if (lastCopy) {
          next = withCopyRecap(next, lastCopy.text, spoken);
        }
        for (const piece of work) {
          const from = asRoleId(piece.from) ?? "coordinatore";
          if (piece.brief) {
            next = upsertBrief(next, from, piece.role, piece.brief, false);
          }
          if (piece.role === "coder") {
            continue;
          }
          if (piece.text && !feedHasThem(next, piece.role, piece.text)) {
            next = [...next, { kind: "them", text: piece.text, from: piece.role }];
          }
        }
        return next;
      });
    } catch (err) {
      const msg =
        typeof err === "string"
          ? err
          : err instanceof Error
            ? err.message
            : "Request failed.";
      setThreads((prev) => ({
        ...prev,
        coordinatore: [
          ...closeLiveThink(prev.coordinatore),
          { kind: "event", text: msg },
        ],
      }));
      setFeed((prev) => [
        ...closeLiveThink(prev),
        { kind: "event", text: msg },
      ]);
    } finally {
      void invoke("answer_user", { text: "" }).catch(() => {});
      setUserAsk(null);
      setAskDraft("");
      pauseCoord();
      const coordMs = Math.max(1000, coordAcc.current);
      const elapsed = orderAt.current
        ? Math.max(1000, Date.now() - orderAt.current)
        : undefined;
      orderAt.current = null;
      coordStart.current = null;
      coordAcc.current = 0;
      thinkLive.current = {};
      setThreads((prev) => {
        const next = { ...prev };
        for (const role of CREW) {
          next[role.id] = closeLiveThink(prev[role.id]);
        }
        return next;
      });
      setFeed((prev) =>
        foldOrderKitchen(closeLiveThink(prev), elapsed, coordMs),
      );
      setTalking(null);
      setBusy(false);
    }
  }

  async function replyAsk(text: string) {
    const prompt = userAsk;
    if (!prompt) return;
    try {
      await invoke("answer_user", { text });
    } catch {
      /* the job may have already moved on */
    }
    const answer = text.trim();
    const closed: Line = {
      kind: "ask",
      who: prompt.who,
      question: prompt.question,
      answer: answer || undefined,
      skipped: !answer,
    };
    setThreads((prev) => ({
      ...prev,
      [prompt.who]: [...prev[prompt.who], closed],
    }));
    setFeed((prev) => [...prev, closed]);
    setUserAsk(null);
    setAskDraft("");
  }

  function hasScreen() {
    if (draft.trim()) return true;
    if (pending.length) return true;
    return CREW.some((role) => threads[role.id].length > 0) || feed.length > 0;
  }

  function askClear() {
    if (busy || !hasScreen()) return;
    setClearAsk(true);
  }

  function clearView() {
    if (busy) return;
    thinkLive.current = {};
    jobAt.current = {};
    orderAt.current = null;
    coordStart.current = null;
    coordAcc.current = 0;
    setThreads(emptyThreads());
    setFeed([]);
    setCrewTodos({});
    setDraft("");
    setPending([]);
    setClearAsk(false);
    void invoke("save_ui_session", {
      session: {
        version: 1,
        activeId: "coordinatore",
        woken,
        recent,
        threads: emptyThreads(),
        feed: [],
        crewTodos: {},
      },
    });
  }

  useEffect(() => {
    if (!clearAsk) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setClearAsk(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [clearAsk]);

  const showPack = packing !== null;

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="drag" onMouseDown={dragWindow} />
        <div className="crew-list">
          {roster.map((role) => {
            const todos = crewTodos[role.id] ?? [];
            const hasTodo = todos.length > 0;
            return (
              <div
                key={role.id}
                className={hasTodo ? "crew-slot has-todo" : "crew-slot"}
                style={{ ["--dot" as string]: ROLE_COLOR[role.id] }}
              >
                <div
                  className={
                    talking === role.id || hasTodo ? "crew-item is-on" : "crew-item"
                  }
                >
                  <span className="mark">
                    <Formina id={role.id} />
                  </span>
                  <span className="crew-text">
                    <span className="crew-row">
                      <span className="crew-name">{role.name}</span>
                      <StatusDot id={role.id} talking={talking === role.id} />
                    </span>
                  </span>
                </div>
                {hasTodo ? <CrewTodo items={todos} /> : null}
              </div>
            );
          })}
        </div>
        <div className="sidebar-foot">
          <button className="clear-chat" type="button" onClick={askClear}>
            Clear chat
          </button>
          <span className="build">Puck 0.0.2</span>
        </div>
      </aside>

      <main className="chat">
        <header className="chat-head" onMouseDown={dragWindow}>
          Chat
        </header>
        {cloudNotice ? <div className="cloud-banner">{cloudNotice}</div> : null}

        <div
          className="thread"
          ref={threadRef}
          onWheel={(e) => {
            if (e.deltaY < 0) stickBottom.current = false;
          }}
          onScroll={() => {
            if (ignoreScroll.current) {
              ignoreScroll.current = false;
              return;
            }
            const el = threadRef.current;
            if (!el) return;
            stickBottom.current =
              el.scrollHeight - el.scrollTop - el.clientHeight < 48;
          }}
        >
          {lines.length === 0 && !busy && !showPack ? (
            <div className="thread-col">
              {cloud && cloud.signed_in && cloud.connected ? (
                <div className="empty">
                  <h1>What do you want to work on today?</h1>
                </div>
              ) : cloud && cloud.signed_in ? (
                <div className="empty">
                  <p style={{ margin: 0, fontSize: 13.5, color: "#6a6a6a" }}>
                    Preparing workspace…
                  </p>
                </div>
              ) : (
                <div className="empty">
                  <h1>What do you want to work on today?</h1>
                </div>
              )}
            </div>
          ) : (
            <div className="thread-col">
              {renderThread(lines)}
              {userAsk ? (
                <AskCard
                  who={userAsk.who}
                  question={userAsk.question}
                  live
                  draft={askDraft}
                  onDraft={setAskDraft}
                  onReply={(text) => void replyAsk(text)}
                />
              ) : null}
              {showPack ? (
                <div className="said">
                  <span className="said-mark">
                    <Formina id={packing ?? "coordinatore"} size={24} />
                  </span>
                  <p className="pack-label">
                    {packComune
                      ? "Memory is being compacted."
                      : "This chat is being compacted."}
                  </p>
                </div>
              ) : null}
            </div>
          )}
        </div>

        {canWrite ? (
          <form
            className={dragging ? "composer-wrap is-drop" : "composer-wrap"}
            onSubmit={send}
            onDragOver={(e) => {
              e.preventDefault();
              if (!busy) setDragging(true);
            }}
            onDragLeave={() => setDragging(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDragging(false);
              if (!busy) void addFiles(e.dataTransfer.files);
            }}
          >
            <input
              ref={fileRef}
              type="file"
              multiple
              hidden
              onChange={(e) => {
                if (e.target.files) void addFiles(e.target.files);
                e.target.value = "";
              }}
            />
            {pending.length ? (
              <div className="attach-row">
                {pending.map((file, i) => (
                  <button
                    key={`${file.name}-${i}`}
                    className="attach-chip"
                    type="button"
                    onClick={() =>
                      setPending((prev) => prev.filter((_, j) => j !== i))
                    }
                    title="Remove"
                  >
                    {file.preview ? (
                      <img src={file.preview} alt="" />
                    ) : null}
                    <span>{file.name}</span>
                  </button>
                ))}
              </div>
            ) : null}
            <div className="composer">
              <button
                className="ghost"
                type="button"
                disabled={busy || !canWrite}
                title="Attach a file"
                onClick={() => fileRef.current?.click()}
              >
                <Plus />
              </button>
              <textarea
                ref={composeRef}
                rows={1}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key !== "Enter" || e.shiftKey) return;
                  if (e.nativeEvent.isComposing) return;
                  e.preventDefault();
                  e.currentTarget.form?.requestSubmit();
                }}
                onPaste={(e) => {
                  const files = Array.from(e.clipboardData.files);
                  if (files.length) {
                    e.preventDefault();
                    void addFiles(files);
                  }
                }}
                placeholder={
                  busy
                    ? "Working…"
                    : !canWrite
                      ? "Connect to start…"
                      : "Message Coordinator"
                }
                disabled={busy || !canWrite}
              />
            </div>
            <div className="workspace-pick is-label" title={undefined}>
              <span>
                {busy
                  ? workspace?.chosen && workspace.exists
                    ? workspace.name
                    : "Working…"
                  : "Workspace"}
              </span>
              {cloud && cloud.signed_in && cloud.email ? (
                <>
                  <span className="sep">•</span>
                  <span className="mail">{cloud.email}</span>
                </>
              ) : null}
            </div>
          </form>
        ) : (
          <div className="composer composer-locked">
            <p>Sync was interrupted.</p>
            <button
              type="button"
              className="clear-chat"
              onClick={() => {
                void invoke<CloudStatus>("cloud_refresh")
                  .then((v) => setCloud(v))
                  .catch(() => {});
              }}
            >
              Retry
            </button>
          </div>
        )}
      </main>

      {clearAsk ? (
        <div className="clear-mask" onClick={() => setClearAsk(false)}>
          <div
            className="clear-card"
            onClick={(e) => e.stopPropagation()}
            role="dialog"
            aria-labelledby="clear-title"
          >
            <p id="clear-title">Clear chat?</p>
            <p className="clear-note">The staff keeps what it knows.</p>
            <div className="clear-actions">
              <button type="button" onClick={() => setClearAsk(false)}>
                Cancel
              </button>
              <button type="button" className="clear-go" onClick={clearView}>
                Clear
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {cloud &&
      ((cloud.configured && !cloud.signed_in) ||
        (!cloud.configured && !cloudDevDismissed)) ? (
        <div className="onboarding-mask">
          <div className="onboarding-card">
            <p className="onboarding-brand">PUCK</p>
            <h2>Create your Puck Cloud</h2>
            {cloud.configured ? (
              <>
                <p className="onboarding-body">
                  Enter your email and we will send you a sign-in link. Your
                  work follows you on any device, and nothing gets lost.
                </p>
                <input
                  type="email"
                  className="onboarding-input"
                  value={cloudEmail}
                  onChange={(e) => setCloudEmail(e.target.value)}
                  placeholder="you@email.com"
                  autoFocus
                />
                <button
                  type="button"
                  className="onboarding-cta"
                  disabled={cloudBusy || !cloudEmail.trim()}
                  onClick={connectCloud}
                >
                  {cloudBusy ? "Sending…" : "Connect"}
                </button>
                {cloudMsg ? (
                  <p className="onboarding-msg">{cloudMsg}</p>
                ) : null}
              </>
            ) : (
              <>
                <p className="onboarding-body">
                  Puck Cloud is not set up on this computer yet. Work will stay
                  on this device only.
                </p>
                <button
                  type="button"
                  className="onboarding-cta"
                  onClick={() => setCloudDevDismissed(true)}
                >
                  Continue locally
                </button>
              </>
            )}
          </div>
        </div>
      ) : null}
    </div>
  );
}

function nodeText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join("");
  if (typeof node === "object" && "props" in node) {
    return nodeText(
      (node as { props?: { children?: ReactNode } }).props?.children,
    );
  }
  return "";
}

function CopyPre({ children }: { children?: ReactNode }) {
  const [copied, setCopied] = useState(false);
  const text = nodeText(children).replace(/\n$/, "");

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const area = document.createElement("textarea");
      area.value = text;
      document.body.appendChild(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1200);
  }

  return (
    <div className="md-pre">
      <pre>{children}</pre>
      <button type="button" className="md-copy" onClick={copy}>
        {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}

function Md({ text }: { text: string }) {
  return (
    <div className="md">
      <Markdown
        remarkPlugins={[remarkGfm]}
        components={{
          pre({ children }) {
            return <CopyPre>{children}</CopyPre>;
          },
        }}
      >
        {text}
      </Markdown>
    </div>
  );
}

function renderThread(lines: Line[]) {
  const out: ReactNode[] = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    if (line.kind === "switch") continue;
    if (line.kind === "think") {
      const who = line.from ?? "coordinatore";
      const after: Line[] = [];
      let j = i + 1;
      while (j < lines.length && sameTurnFollow(lines[j]!, who)) {
        after.push(lines[j]!);
        j += 1;
      }
      out.push(<SaidTurn key={i} who={who} think={line} after={after} />);
      i = j - 1;
      continue;
    }
    if (line.kind === "brief" && (line.from ?? "coordinatore") === "coordinatore") {
      const after: Line[] = [line];
      let j = i + 1;
      while (j < lines.length && sameTurnFollow(lines[j]!, "coordinatore")) {
        after.push(lines[j]!);
        j += 1;
      }
      out.push(<SaidTurn key={i} who="coordinatore" after={after} />);
      i = j - 1;
      continue;
    }
    const who = turnWho(line);
    if (who && who !== "coordinatore" && isLooseTurnBit(line, who)) {
      const chunk: Line[] = [];
      let j = i;
      while (j < lines.length && isLooseTurnBit(lines[j]!, who)) {
        chunk.push(lines[j]!);
        j += 1;
      }
      out.push(<SaidTurn key={i} who={who} after={chunk} />);
      i = j - 1;
      continue;
    }
    out.push(<ChatLine key={i} line={line} />);
  }
  return out;
}

function SaidTurn({
  who,
  think,
  after,
}: {
  who: RoleId;
  think?: Extract<Line, { kind: "think" }>;
  after: Line[];
}) {
  return (
    <div className={who === "coordinatore" ? "said is-turn is-coord" : "said is-turn"}>
      <span className="said-mark">
        <Formina id={who} size={24} />
      </span>
      <div className="turn-body">
        {think ? (
          <ThinkBox
            text={think.text}
            live={think.live}
            who={think.from ?? who}
          />
        ) : null}
        {after.map((line, i) => (
          <ChatLine key={i} line={line} nested />
        ))}
      </div>
    </div>
  );
}

function CrewTodo({ items }: { items: TodoItem[] }) {
  return (
    <ul className="crew-todo">
      {items.map((item, i) => {
        const now = item.status === "in_progress";
        const done = item.status === "done";
        return (
          <li
            key={`${i}-${item.content}`}
            className={now ? "is-now" : done ? "is-done" : undefined}
          >
            <span
              className={
                done
                  ? "crew-todo-dot is-fill is-done"
                  : now
                    ? "crew-todo-dot is-fill"
                    : "crew-todo-dot"
              }
            />
            <span className="crew-todo-text">{item.content}</span>
          </li>
        );
      })}
    </ul>
  );
}

function ChatLine({ line, nested }: { line: Line; nested?: boolean }) {
  if (line.kind === "switch") return null;
  if (line.kind === "event") {
    return <div className="event">{line.text}</div>;
  }
  if (line.kind === "trace") {
    return <div className="trace">{line.text}</div>;
  }
  if (line.kind === "todo") {
    return null;
  }
  if (line.kind === "work") {
    const who = line.from ?? "coder";
    const box = <WorkBox line={line} />;
    if (nested) return box;
    return (
      <div className="said is-turn">
        <span className="said-mark">
          <Formina id={who} size={24} />
        </span>
        {box}
      </div>
    );
  }
  if (line.kind === "job") {
    return <JobBox line={line} />;
  }
  if (line.kind === "brief") {
    const box = (
      <BriefBox
        text={line.text}
        live={line.live}
        from={line.from}
        to={line.to}
      />
    );
    if (nested) return box;
    return (
      <div className="said">
        <span className="said-mark">
          <Formina id={line.from} size={24} />
        </span>
        {box}
      </div>
    );
  }
  if (line.kind === "think") {
    return (
      <ThinkBox
        text={line.text}
        live={line.live}
        who={line.from ?? "coordinatore"}
      />
    );
  }
  if (line.kind === "you") {
    return (
      <div className="bubble is-you">
        {line.files?.length ? (
          <div className="you-files">
            {line.files.map((file) =>
              file.preview ? (
                <img key={file.name} src={file.preview} alt={file.name} />
              ) : (
                <span key={file.name}>{file.name}</span>
              ),
            )}
          </div>
        ) : null}
        {line.text}
      </div>
    );
  }
  if (line.kind === "clip") {
    const label = isGenericClipTitle(line.title) ? "" : `**${line.title}**\n\n`;
    const bubble = (
      <div className="bubble is-them">
        <Md text={`${label}${fencePiece(line.text)}`} />
      </div>
    );
    if (nested) return bubble;
    return (
      <div className="said is-coord">
        <span className="said-mark">
          <Formina id="coordinatore" size={24} />
        </span>
        {bubble}
      </div>
    );
  }
  if (line.kind === "wrote") {
    return <WroteBox line={line} />;
  }
  if (line.kind === "ask") {
    return (
      <AskCard
        who={line.who}
        question={line.question}
        answer={line.answer}
        skipped={line.skipped}
        nested={nested}
      />
    );
  }
  const from = line.from ?? "coordinatore";
  const bubble = (
    <div className={line.live ? "bubble is-them is-live" : "bubble is-them"}>
      <Md text={line.text} />
    </div>
  );
  if (nested) return bubble;
  return (
    <div className={from === "coordinatore" ? "said is-coord" : "said"}>
      <span className="said-mark">
        <Formina id={from} size={24} />
      </span>
      {bubble}
    </div>
  );
}

function JobBox({
  line,
}: {
  line: Extract<Line, { kind: "job" }>;
}) {
  const [open, setOpen] = useState(false);
  const kitchen = useMemo(() => restoreKitchen(line.lines), [line]);
  return (
    <div className="job-box">
      <button
        className="clip-toggle job-head"
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
      >
        <span className="said-mark">
          <CrewSpin who={kitchenWho(kitchen)} />
        </span>
        <span className="clip-caret">{open ? "▾" : "▸"}</span>
        <span className="clip-preview">{jobPreview(line.ms)}</span>
      </button>
      {open ? <div className="job-body">{renderThread(kitchen)}</div> : null}
    </div>
  );
}

function WorkBox({
  line,
}: {
  line: Extract<Line, { kind: "work" }>;
}) {
  const [open, setOpen] = useState(false);
  const lines = useMemo(() => collapseThinkRuns(line.lines), [line]);
  return (
    <div className="work-box">
      <div className="think-bar">
        <button
          className="clip-toggle"
          type="button"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
        >
          <span className="clip-caret">{open ? "▾" : "▸"}</span>
          <span className="clip-preview">
            {crewName(line.from ?? "coder")} worked for {formatWorked(line.ms)}
          </span>
        </button>
      </div>
      {open ? (
        <div className="work-body">
          {lines.map((inner, i) => (
            <ChatLine key={i} line={inner} nested />
          ))}
        </div>
      ) : null}
    </div>
  );
}

function ThinkBox({
  text,
  live,
  who,
}: {
  text: string;
  live: boolean;
  who?: RoleId;
}) {
  const [open, setOpen] = useState(live);
  const bodyRef = useRef<HTMLDivElement>(null);
  const follow = useRef(true);

  useEffect(() => {
    setOpen(live);
    if (live) follow.current = true;
  }, [live]);

  useEffect(() => {
    if (!open) return;
    const el = bodyRef.current;
    if (!el || !follow.current) return;
    el.scrollTop = el.scrollHeight;
  }, [text, open, live]);

  const whoName = who ? crewName(who) : null;
  const preview = whoName
    ? live
      ? `${whoName} is thinking…`
      : `${whoName} thought`
    : live
      ? "Thinking…"
      : "Thought";
  return (
    <div className={live ? "think-box is-live" : "think-box"}>
      <div className="think-bar">
        <button
          className="clip-toggle"
          type="button"
          onClick={() => {
            follow.current = true;
            setOpen((v) => !v);
          }}
          aria-expanded={open}
        >
          <span className="clip-caret">{open ? "▾" : "▸"}</span>
          <span className="clip-preview">{preview}</span>
        </button>
      </div>
      {open && text.trim() ? (
        <div
          className="think-body"
          ref={bodyRef}
          onWheel={(e) => {
            if (e.deltaY < 0) follow.current = false;
          }}
          onScroll={() => {
            const el = bodyRef.current;
            if (!el) return;
            const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
            if (gap < 24) follow.current = true;
          }}
        >
          <Md text={text} />
        </div>
      ) : null}
    </div>
  );
}


function BriefBox({
  text,
  live,
  from,
  to,
}: {
  text: string;
  live: boolean;
  from: RoleId;
  to: RoleId;
}) {
  const [open, setOpen] = useState(live);
  const bodyRef = useRef<HTMLDivElement>(null);
  const follow = useRef(true);

  useEffect(() => {
    setOpen(live);
    if (live) follow.current = true;
  }, [live]);

  useEffect(() => {
    if (!open) return;
    const el = bodyRef.current;
    if (!el || !follow.current) return;
    el.scrollTop = el.scrollHeight;
  }, [text, open, live]);

  const preview = live
    ? `Brief for ${crewName(to)}…`
    : `Brief for ${crewName(to)}`;
  return (
    <div className={live ? "think-box is-live" : "think-box"}>
      <div className="think-bar">
        <button
          className="clip-toggle"
          type="button"
          onClick={() => {
            follow.current = true;
            setOpen((v) => !v);
          }}
          aria-expanded={open}
        >
          <span className="clip-caret">{open ? "▾" : "▸"}</span>
          <span className="clip-preview">{preview}</span>
        </button>
      </div>
      {open && text.trim() ? (
        <div
          className="think-body"
          ref={bodyRef}
          onWheel={(e) => {
            if (e.deltaY < 0) follow.current = false;
          }}
          onScroll={() => {
            const el = bodyRef.current;
            if (!el) return;
            const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
            if (gap < 24) follow.current = true;
          }}
        >
          <p className="brief-from">From {crewName(from)}</p>
          <Md text={text} />
        </div>
      ) : null}
    </div>
  );
}

function WroteBox({
  line,
}: {
  line: Extract<Line, { kind: "wrote" }>;
}) {
  const live = !!line.live;
  const [open, setOpen] = useState(live);
  const bodyRef = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  const name = shortPath(line.path);

  useEffect(() => {
    setOpen(live);
    if (live) follow.current = true;
  }, [live]);

  useEffect(() => {
    if (!open) return;
    const el = bodyRef.current;
    if (!el || !follow.current) return;
    el.scrollTop = el.scrollHeight;
  }, [line.diff, open, live]);

  return (
    <div className={live ? "think-box is-write is-live" : "think-box is-write"}>
      <div className="think-bar">
        <button
          className="clip-toggle"
          type="button"
          onClick={() => {
            follow.current = true;
            setOpen((v) => !v);
          }}
          aria-expanded={open}
        >
          <span className="clip-caret">{open ? "▾" : "▸"}</span>
          <span className="clip-preview">
            {live ? `Writing ${name}…` : `Wrote ${name}`}
          </span>
        </button>
      </div>
      {open && line.diff.trim() ? (
        <div
          className="think-body is-diff"
          ref={bodyRef}
          onWheel={(e) => {
            if (e.deltaY < 0) follow.current = false;
          }}
        >
          <pre>{line.diff}</pre>
        </div>
      ) : null}
    </div>
  );
}

function AskCard({
  who,
  question,
  answer,
  skipped,
  live,
  nested,
  draft,
  onDraft,
  onReply,
}: {
  who: RoleId;
  question: string;
  answer?: string;
  skipped?: boolean;
  live?: boolean;
  nested?: boolean;
  draft?: string;
  onDraft?: (text: string) => void;
  onReply?: (text: string) => void;
}) {
  const boxRef = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    if (!live) return;
    const el = boxRef.current;
    if (el) fitCompose(el);
  }, [draft, live]);
  const card = (
      <form
        className="ask-card"
        onSubmit={(e) => {
          e.preventDefault();
          if (!live || !onReply) return;
          const text = (draft ?? "").trim();
          if (!text) return;
          onReply(text);
        }}
      >
        <p className="ask-q">{question}</p>
        {live ? (
          <>
            <textarea
              ref={boxRef}
              className="ask-box"
              value={draft ?? ""}
              onChange={(e) => onDraft?.(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && e.shiftKey) return;
                if (e.key === "Enter") {
                  e.preventDefault();
                  const text = (draft ?? "").trim();
                  if (text) onReply?.(text);
                }
              }}
              rows={1}
              autoFocus
            />
            <div className="clear-actions">
              <button type="button" onClick={() => onReply?.("")}>
                Skip
              </button>
              <button
                type="submit"
                className="clear-go"
                disabled={!(draft ?? "").trim()}
              >
                Reply
              </button>
            </div>
          </>
        ) : skipped ? (
          <p className="ask-a is-skip">Skipped</p>
        ) : (
          <p className="ask-a">{answer}</p>
        )}
      </form>
  );
  if (nested) return card;
  return (
    <div className="said is-ask">
      <span className="said-mark">
        <Formina id={who} size={24} />
      </span>
      {card}
    </div>
  );
}

function StatusDot({
  id,
  talking,
}: {
  id: RoleId;
  talking: boolean;
}) {
  const color = ROLE_COLOR[id];
  return (
    <span
      className={talking ? "status-dot is-talk" : "status-dot"}
      style={{ ["--dot" as string]: color }}
      title={talking ? "Talking" : "Off"}
    />
  );
}

function Plus() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden>
      <path d="M8 3.5v9M3.5 8h9" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
    </svg>
  );
}



