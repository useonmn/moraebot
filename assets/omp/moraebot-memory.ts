// moraebot's memory extension for the lead's omp process. moraebot writes this file to
// <home>/omp at every lead start and loads it with `-e`. It refreshes the memory block of
// the system prompt from a file, delivers queued memory notices as a hidden message,
// refuses file tools aimed at the memory folder, and points at project memory.
import * as fs from "node:fs";
import * as path from "node:path";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

const VERSION = "moraebot-memory-1";
const HOME_ENV = "MORAEBOT_EXT_HOME";
const ACK_FILE = "moraebot-memory.ack.json";
const BLOCK_FILE = "memory-block.md";
const OUTBOX_DIR = "memory-outbox";
const MESSAGE_TYPE = "moraebot-memory";
const BEGIN = "<!-- moraebot-memory:begin -->";
const END = "<!-- moraebot-memory:end -->";
const DIRECT_EDIT = "E_MEMORY_DIRECT_EDIT";
const MEMORY_DIR = "memory";
const OMP_DIR = "omp";
const PROJECTS = "notes/projects";

const EDIT_TOOLS = ["write", "edit", "ast_edit"];
const HINT_TOOLS = ["read", "write", "edit", "ast_edit", "grep", "glob"];
const PROJECT_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

// The real path of `p`: the nearest existing ancestor resolved, the rest appended.
function realOf(p: string): string {
  let head = path.resolve(p);
  const rest: string[] = [];
  for (;;) {
    try {
      return path.join(fs.realpathSync(head), ...rest);
    } catch {
      const parent = path.dirname(head);
      if (parent === head) return path.join(head, ...rest);
      rest.unshift(path.basename(head));
      head = parent;
    }
  }
}

function inside(target: string, root: string): boolean {
  const rel = path.relative(root, target);
  return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
}

function targetsOf(input: any): string[] {
  const out: string[] = [];
  if (typeof input?.path === "string") out.push(input.path);
  if (Array.isArray(input?.paths)) {
    for (const p of input.paths) if (typeof p === "string") out.push(p);
  }
  return out;
}

function writePrivate(dir: string, name: string, text: string): void {
  const tmp = path.join(dir, `.${name}.${process.pid}.tmp`);
  fs.writeFileSync(tmp, text, { mode: 0o600 });
  fs.renameSync(tmp, path.join(dir, name));
}

export default function (pi: ExtensionAPI) {
  const home = process.env[HOME_ENV];
  if (!home || !path.isAbsolute(home)) return;
  const memoryRoot = path.join(home, MEMORY_DIR);
  const ompDir = path.join(home, OMP_DIR);
  const hinted = new Set<string>();
  let pending: { prompt: string; text: string } | undefined;

  const isMain = (ctx: any) => ctx?.agent?.kind !== "sub";

  const drain = (): string => {
    const outbox = path.join(ompDir, OUTBOX_DIR);
    let names: string[];
    try {
      names = fs.readdirSync(outbox);
    } catch {
      return "";
    }
    let text = "";
    for (const name of names.filter((n) => n.endsWith(".md") && !n.startsWith(".")).sort()) {
      const file = path.join(outbox, name);
      try {
        text += fs.readFileSync(file, "utf8");
        fs.unlinkSync(file);
      } catch {
        // An entry that cannot be read stays for the next drain.
      }
    }
    return text;
  };

  pi.on("session_start", async () => {
    try {
      writePrivate(ompDir, ACK_FILE, JSON.stringify({ pid: process.pid, version: VERSION }));
    } catch {
      return undefined;
    }
  });

  pi.on("before_agent_start", async (event: any, ctx: any) => {
    try {
      if (event.reason !== "prompt") return undefined;
      const result: any = {};
      let block: string | undefined;
      try {
        block = fs.readFileSync(path.join(ompDir, BLOCK_FILE), "utf8");
      } catch {
        block = undefined;
      }
      if (block !== undefined && block.startsWith(BEGIN) && block.endsWith(END + "\n")) {
        const fresh = block.slice(0, -1);
        const parts: string[] = [...event.systemPrompt];
        for (let i = 0; i < parts.length; i++) {
          const start = parts[i].indexOf(BEGIN);
          if (start < 0) continue;
          const end = parts[i].indexOf(END, start);
          if (end < 0) continue;
          const stop = end + END.length;
          if (parts[i].slice(start, stop) !== fresh) {
            parts[i] = parts[i].slice(0, start) + fresh + parts[i].slice(stop);
            result.systemPrompt = parts;
          }
          break;
        }
      }
      if (isMain(ctx)) {
        if (pending && pending.prompt !== event.prompt) pending = undefined;
        const text = (pending?.text ?? "") + drain();
        if (text !== "") {
          pending = { prompt: event.prompt, text };
          result.message = { customType: MESSAGE_TYPE, content: text, display: false };
        }
      }
      return Object.keys(result).length > 0 ? result : undefined;
    } catch {
      return undefined;
    }
  });

  pi.on("turn_end", async (_event: any, ctx: any) => {
    try {
      if (isMain(ctx)) pending = undefined;
    } catch {
      return undefined;
    }
  });

  pi.on("tool_call", async (event: any, ctx: any) => {
    try {
      if (EDIT_TOOLS.includes(event.toolName)) {
        const roots = [realOf(memoryRoot), realOf(ompDir)];
        for (const target of targetsOf(event.input)) {
          const real = realOf(path.resolve(ctx.cwd, target));
          if (roots.some((root) => inside(real, root))) {
            return {
              block: true,
              reason: DIRECT_EDIT + ": " + target + ": change memory with the memory tools",
            };
          }
        }
        return undefined;
      }
      if (event.toolName === "bash") {
        const command = event.input?.command;
        if (typeof command === "string" && (command.includes(memoryRoot) || command.includes(ompDir))) {
          return {
            additionalContext:
              "moraebot: files under the memory folder change only through the memory tools; a shell edit there is not used and holds memory writes until the owner accepts it.",
          };
        }
      }
      return undefined;
    } catch {
      return undefined;
    }
  });

  pi.on("tool_result", async (event: any, ctx: any) => {
    try {
      if (!isMain(ctx)) return undefined;
      const lines: string[] = [];
      const queued = drain();
      if (queued !== "") lines.push(queued);
      if (HINT_TOOLS.includes(event.toolName) && !event.isError) {
        const cwd = realOf(ctx.cwd);
        for (const target of targetsOf(event.input)) {
          const rel = path.relative(cwd, realOf(path.resolve(ctx.cwd, target)));
          if (rel.startsWith("..") || path.isAbsolute(rel)) continue;
          const parts = rel.split(path.sep);
          if (parts.length < 2) continue;
          const name = parts[0];
          if (!PROJECT_NAME.test(name) || hinted.has(name)) continue;
          const note = path.join(memoryRoot, PROJECTS, name + ".md");
          if (!fs.statSync(note, { throwIfNoEntry: false })?.isFile()) continue;
          hinted.add(name);
          lines.push(
            `moraebot: project memory for ${name} is in ${PROJECTS}/${name}.md (read it with memory_read when it matters).`,
          );
        }
      }
      return lines.length > 0 ? { additionalContext: lines.join("\n") } : undefined;
    } catch {
      return undefined;
    }
  });
}
