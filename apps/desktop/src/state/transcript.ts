import type {
  AgentEvent,
  ApprovalDecision,
  ApprovalRequest,
  FileEditKind,
  ToolKind,
} from "@hearth/protocol";
import type { TurnSnapshot } from "../ipc/contract";

export type Block =
  | { id: number; kind: "user"; text: string; replyModel: string }
  | { id: number; kind: "text"; text: string }
  | { id: number; kind: "reasoning"; text: string }
  | {
      id: number;
      kind: "tool";
      toolCallId: string;
      tool: string;
      toolKind: ToolKind;
      title: string;
      status: "running" | "completed" | "failed";
      output: string;
    }
  | { id: number; kind: "fileEdit"; path: string; editKind: FileEditKind }
  | { id: number; kind: "command"; command: string; exitCode?: number }
  | { id: number; kind: "approval"; request: ApprovalRequest; answer?: ApprovalDecision }
  | { id: number; kind: "error"; message: string };

export type Transcript = { blocks: Block[]; nextId: number; running: boolean; lastUserIndex: number };

export const emptyTranscript: Transcript = { blocks: [], nextId: 1, running: false, lastUserIndex: -1 };

function append(t: Transcript, make: (id: number) => Block): Transcript {
  return { ...t, blocks: [...t.blocks, make(t.nextId)], nextId: t.nextId + 1 };
}

function replaceLast(t: Transcript, block: Block): Transcript {
  const blocks = t.blocks.slice();
  blocks[blocks.length - 1] = block;
  return { ...t, blocks };
}

function replaceAt(t: Transcript, index: number, block: Block): Transcript {
  const blocks = t.blocks.slice();
  blocks[index] = block;
  return { ...t, blocks };
}

function findTool(t: Transcript, toolCallId: string): number {
  for (let i = t.blocks.length - 1; i >= 0; i--) {
    const b = t.blocks[i];
    if (b?.kind === "tool" && b.toolCallId === toolCallId) return i;
  }
  return -1;
}

export function addUserMessage(t: Transcript, text: string, replyModel = ""): Transcript {
  return {
    ...append(t, (id) => ({ id, kind: "user", text, replyModel })),
    running: true,
    lastUserIndex: t.blocks.length,
  };
}

export function labelActiveReply(t: Transcript, replyModel: string): Transcript {
  const user = t.blocks[t.lastUserIndex];
  return user?.kind === "user" && user.replyModel !== replyModel
    ? replaceAt(t, t.lastUserIndex, { ...user, replyModel })
    : t;
}

/** Folds one agent event into the transcript. Consecutive deltas of one kind extend the last block. */
export function applyEvent(t: Transcript, event: AgentEvent): Transcript {
  switch (event.kind) {
    case "text.delta":
    case "reasoning.delta": {
      const kind = event.kind === "text.delta" ? "text" : "reasoning";
      const last = t.blocks[t.blocks.length - 1];
      if (last && last.kind === kind) return replaceLast(t, { ...last, text: last.text + event.text });
      return append(t, (id) => ({ id, kind, text: event.text }));
    }
    case "tool.start":
      return append(t, (id) => ({
        id,
        kind: "tool",
        toolCallId: event.toolCallId,
        tool: event.tool,
        toolKind: event.toolKind,
        title: event.title,
        status: "running",
        output: "",
      }));
    case "tool.output": {
      const i = findTool(t, event.toolCallId);
      const b = t.blocks[i];
      return b?.kind === "tool" ? replaceAt(t, i, { ...b, output: b.output + event.text }) : t;
    }
    case "tool.end": {
      const i = findTool(t, event.toolCallId);
      const b = t.blocks[i];
      return b?.kind === "tool" ? replaceAt(t, i, { ...b, status: event.status }) : t;
    }
    case "file.edit":
      return append(t, (id) => ({ id, kind: "fileEdit", path: event.path, editKind: event.editKind }));
    case "command.run":
      return append(t, (id) => ({
        id,
        kind: "command",
        command: event.command,
        ...(event.exitCode === undefined ? {} : { exitCode: event.exitCode }),
      }));
    case "approval.request":
      return append(t, (id) => ({ id, kind: "approval", request: event.request }));
    case "error":
      return append(t, (id) => ({ id, kind: "error", message: event.message }));
    case "turn.end":
      return { ...t, running: false };
    case "usage":
      return t;
  }
}

export function transcriptFromTurns(turns: TurnSnapshot[]): Transcript {
  let t = emptyTranscript;
  for (const turn of turns) {
    t = addUserMessage(t, turn.input.text, turn.model);
    // A turn without a turn.end is still running (or was interrupted); the live events decide.
    for (const event of turn.events) t = applyEvent(t, event);
  }
  return t;
}

export function answerApprovalBlock(t: Transcript, approvalId: string, decision: ApprovalDecision): Transcript {
  const i = t.blocks.findIndex((b) => b.kind === "approval" && b.request.id === approvalId);
  const b = t.blocks[i];
  return b?.kind === "approval" ? replaceAt(t, i, { ...b, answer: decision }) : t;
}
