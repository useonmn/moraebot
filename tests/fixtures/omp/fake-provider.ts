// Test-only offline model provider for the opt-in real-omp tests in tests/real_omp.rs.
// It registers provider `fakeprov` with one model `fake-model` and never opens a socket.
// Each request is appended as one JSON line to MORAEBOT_TEST_PROVIDER_LOG, and the reply is
// chosen from the last message that is not a `developer` message (omp adds a hook's
// additionalContext as such a message after the tool results). The MORAEBOT_TEST_* variables
// are read per request, so loading this file where they are unset is harmless.
import * as fs from "node:fs";
import * as path from "node:path";
import { AssistantMessageEventStream } from "@oh-my-pi/pi-ai";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

let calls = 0;

function reply(content: any[], stopReason: string) {
  return {
    role: "assistant",
    content,
    api: "fake-api",
    provider: "fakeprov",
    model: "fake-model",
    usage: {
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      totalTokens: 0,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason,
    timestamp: Date.now(),
  };
}

function textOf(message: any): string {
  if (!message) return "";
  if (typeof message.content === "string") return message.content;
  if (!Array.isArray(message.content)) return "";
  return message.content.map((block: any) => (typeof block?.text === "string" ? block.text : "")).join("");
}

function toolCall(name: string, args: unknown) {
  calls += 1;
  return reply([{ type: "toolCall", id: `call_fake_${calls}`, name, arguments: args }], "toolUse");
}

function answer(messages: any[]) {
  const last = [...messages].reverse().find((m: any) => m?.role !== "developer");
  const text = textOf(last);
  if (last?.role === "toolResult") return reply([{ type: "text", text: "done" }], "stop");
  if (last?.role === "user") {
    if (text.includes("cue-write-memory")) {
      return toolCall("write", { path: process.env.MORAEBOT_TEST_MEMORY_TARGET, content: "blocked-111\n" });
    }
    if (text.includes("cue-read-project")) {
      const outbox = process.env.MORAEBOT_TEST_OUTBOX ?? "";
      fs.writeFileSync(path.join(outbox, "99999999999999999999.md"), "mid-turn-marker-111\n", { mode: 0o600 });
      return toolCall("read", { path: "proj-111/a.txt" });
    }
    if (text.includes("cue-ast-edit-memory")) {
      return toolCall("ast_edit", {
        ops: [{ pat: "project notes", out: "edited-111" }],
        paths: [process.env.MORAEBOT_TEST_EDIT_TARGET],
      });
    }
    if (text.includes("cue-edit-memory")) {
      return toolCall("edit", {
        path: process.env.MORAEBOT_TEST_EDIT_TARGET,
        old_string: "project notes",
        new_string: "edited-111",
      });
    }
  }
  return reply([{ type: "text", text: "ok" }], "stop");
}

export default function (pi: ExtensionAPI) {
  pi.registerProvider("fakeprov", {
    baseUrl: "http://127.0.0.1:9/",
    apiKey: "unused",
    api: "fake-api",
    streamSimple(_model: any, context: any, _options: any) {
      const stream = new AssistantMessageEventStream();
      const messages = context.messages ?? [];
      const system = Array.isArray(context.systemPrompt)
        ? context.systemPrompt.join("\n")
        : String(context.systemPrompt ?? "");
      const log = process.env.MORAEBOT_TEST_PROVIDER_LOG;
      if (log) fs.appendFileSync(log, JSON.stringify({ system, messages }) + "\n");
      queueMicrotask(() => {
        const m: any = answer(messages);
        stream.push({ type: "start", partial: { ...m, content: [] } });
        m.content.forEach((block: any, i: number) => {
          if (block.type === "toolCall") {
            stream.push({ type: "toolcall_start", contentIndex: i, partial: m });
            stream.push({ type: "toolcall_end", contentIndex: i, toolCall: block, partial: m });
          } else {
            stream.push({ type: "text_start", contentIndex: i, partial: m });
            stream.push({ type: "text_end", contentIndex: i, content: block.text, partial: m });
          }
        });
        stream.push({ type: "done", reason: m.stopReason, message: m });
      });
      return stream;
    },
    models: [
      {
        id: "fake-model",
        name: "fake-model",
        reasoning: false,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 128000,
        maxTokens: 4096,
      },
    ],
  } as any);
}
