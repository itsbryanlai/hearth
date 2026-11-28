import type {
  AgentEvent,
  AgentId,
  ApprovalDecision,
  ApprovalId,
  ProjectId,
  ThreadId,
  TurnId,
  UserInput,
} from "@hearth/protocol";

export type AgentStatus =
  | { kind: "ready" }
  | { kind: "missing" }
  | { kind: "unsupported"; version: string };

export type InstalledAgent = {
  id: AgentId;
  label: string;
  status: AgentStatus;
  remembersApprovals: boolean;
};

export type HandoffDraft = {
  threadId: ThreadId;
  sourceAgent: AgentId;
  targetAgent: AgentId;
  text: string;
};

export type Project = { id: ProjectId; name: string; path: string };

export type ThreadSummary = {
  id: ThreadId;
  projectId: ProjectId;
  title: string;
  agent: AgentId;
  model: string;
  branch: string | null;
  archived: boolean;
};

export type ModelChoice = { id: string; label: string; resolvedId: string | null };

export type ThreadModels = { choices: ModelChoice[]; selected: string | null; current: string | null };

/** One user message and the agent events it produced, in order. */
export type TurnSnapshot = {
  turnId: TurnId;
  agent: AgentId;
  model: string;
  input: UserInput;
  events: AgentEvent[];
};

export type ThreadSnapshot = {
  thread: ThreadSummary;
  turns: TurnSnapshot[];
  /** The `seq` of the newest event in `turns`: live events at or below it are already shown. */
  lastSeq: number;
};

/** Payload of the `thread-event` channel. `seq` increases per thread. */
export type ThreadEventPayload = {
  threadId: ThreadId;
  turnId: TurnId;
  seq: number;
  model?: string;
  event: AgentEvent;
};

export const THREAD_EVENT_CHANNEL = "thread-event";

export type ReviewTurn = { turnId: TurnId; label: string; available: boolean };
export type WorktreeReview = { id: string; files: DiffFile[] };
export type DiffFile = { id: string; path: string; oldPath: string | null; status: string };
export type DiffRow = { kind: "context" | "delete" | "add" | "notice"; text: string; oldLine: number | null; newLine: number | null };
export type FileDiff = { binary: boolean; hunks: { header: string; rows: DiffRow[]; splitRows: { left: number | null; right: number | null; notice: number | null }[] }[] };

/**
 * The backend as the UI sees it. Each method is one Tauri command of the same
 * name in snake_case (`listProjects` is `list_projects`).
 */
export interface Ipc {
  listAgents(): Promise<InstalledAgent[]>;
  listProjects(): Promise<Project[]>;
  addProject(path: string): Promise<Project>;
  listThreads(projectId: ProjectId): Promise<ThreadSummary[]>;
  worktreeInfo(projectId: ProjectId): Promise<{ isGit: boolean; base: string | null; reason: string | null }>;
  createThread(projectId: ProjectId, agent: AgentId, base: string | null, parent?: string | null): Promise<ThreadSummary>;
  loadThread(threadId: ThreadId): Promise<ThreadSnapshot>;
  reviewTurns(threadId: ThreadId): Promise<ReviewTurn[]>;
  startWorktreeReview(threadId: ThreadId): Promise<string>;
  worktreeDiffFiles(threadId: ThreadId, reviewId: string): Promise<WorktreeReview | null>;
  worktreeDiffFile(threadId: ThreadId, reviewId: string, fileId: string): Promise<FileDiff | null>;
  releaseWorktreeReview(threadId: ThreadId, reviewId: string): Promise<void>;
  turnDiffFiles(threadId: ThreadId, turnId: TurnId): Promise<DiffFile[] | null>;
  turnDiffFile(threadId: ThreadId, turnId: TurnId, fileId: string): Promise<FileDiff | null>;
  archiveThread(threadId: ThreadId): Promise<ThreadSummary>;
  restoreThread(threadId: ThreadId): Promise<ThreadSummary>;
  listThreadModels(threadId: ThreadId): Promise<ThreadModels>;
  setThreadModel(threadId: ThreadId, modelId: string): Promise<ThreadSummary>;
  /** Builds an editable preamble locally; starts nothing. */
  draftHandoff(threadId: ThreadId, target: AgentId): Promise<HandoffDraft>;
  /** Switches the thread to `target` and sends `text` as its first turn there. */
  startHandoff(threadId: ThreadId, target: AgentId, text: string): Promise<ThreadSummary>;
  /** Resolves with the thread as it is once the turn started (title, model). */
  sendMessage(threadId: ThreadId, input: UserInput): Promise<ThreadSummary>;
  answerApproval(
    threadId: ThreadId,
    approvalId: ApprovalId,
    decision: ApprovalDecision,
  ): Promise<void>;
  cancelTurn(threadId: ThreadId): Promise<void>;
  /** Subscribe to the `thread-event` channel; returns the unsubscribe function. */
  onThreadEvent(handler: (payload: ThreadEventPayload) => void): () => void;
}
