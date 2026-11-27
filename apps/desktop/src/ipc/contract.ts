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

export type Project = { id: ProjectId; name: string; path: string };

export type ThreadSummary = {
  id: ThreadId;
  projectId: ProjectId;
  title: string;
  agent: AgentId;
  model: string;
};

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

/**
 * The backend as the UI sees it. Each method is one Tauri command of the same
 * name in snake_case (`listProjects` is `list_projects`).
 */
export interface Ipc {
  listAgents(): Promise<InstalledAgent[]>;
  listProjects(): Promise<Project[]>;
  addProject(path: string): Promise<Project>;
  listThreads(projectId: ProjectId): Promise<ThreadSummary[]>;
  createThread(projectId: ProjectId, agent: AgentId): Promise<ThreadSummary>;
  loadThread(threadId: ThreadId): Promise<ThreadSnapshot>;
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
