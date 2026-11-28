import type { AgentEvent, ApprovalDecision } from "@hearth/protocol";
import type {
  Ipc,
  ModelChoice,
  Project,
  ThreadEventPayload,
  ThreadSnapshot,
  ThreadSummary,
  TurnSnapshot,
} from "./contract";

type Options = {
  /** Runs `step` later. Default: a short real timer; tests pass a manual queue. */
  schedule?: (step: () => void) => void;
};

const REPLY =
  "The flaky test races the token refresh against the session clock. " +
  "I moved the refresh behind the same lock the clock uses and added a regression test that " +
  "advances time by one second between the two calls. The suite now passes ten runs in a row.";

function words(text: string): string[] {
  return text.match(/\S+\s*/g) ?? [];
}

/** Events before the approval, then (after it) the events that follow. */
function scriptedTurn(): { before: AgentEvent[]; after: (d: ApprovalDecision) => AgentEvent[] } {
  const before: AgentEvent[] = [
    ...words("Looking at how the refresh path is called before changing anything.").map(
      (text): AgentEvent => ({ kind: "reasoning.delta", text }),
    ),
    {
      kind: "tool.start",
      toolCallId: "t1",
      tool: "Read",
      toolKind: "read",
      title: "Read src/auth/session.rs",
    },
    { kind: "tool.output", toolCallId: "t1", text: "pub fn refresh(&mut self) {\n" },
    { kind: "tool.output", toolCallId: "t1", text: "    self.token = self.mint();\n}\n" },
    { kind: "tool.end", toolCallId: "t1", status: "completed" },
    { kind: "file.edit", toolCallId: "t2", path: "src/auth/session.rs", editKind: "modify" },
    {
      kind: "approval.request",
      request: {
        id: "a1",
        tool: "Bash",
        title: "Run the auth tests",
        detail: "cargo test -p auth session",
        options: [
          { optionId: "allow", label: "Allow", kind: "allowOnce" },
          { optionId: "reject", label: "Reject", kind: "rejectOnce" },
        ],
      },
    },
  ];
  const after = (decision: ApprovalDecision): AgentEvent[] => [
    ...(decision === "deny"
      ? ([{ kind: "error", message: "Command was denied; not running the tests." }] as AgentEvent[])
      : ([
          { kind: "command.run", toolCallId: "t3", command: "cargo test -p auth session", exitCode: 0 },
        ] as AgentEvent[])),
    ...words(REPLY).map((text): AgentEvent => ({ kind: "text.delta", text })),
    { kind: "usage", usage: { inputTokens: 1200, outputTokens: 180 } },
    { kind: "turn.end", stopReason: "endTurn" },
  ];
  return { before, after };
}

/** In-memory backend for `pnpm dev` in a browser and for tests. */
export function createMockIpc(options: Options = {}): Ipc {
  const schedule = options.schedule ?? ((step) => void setTimeout(step, 15));
  let nextId = 1;
  const id = (prefix: string) => `${prefix}${nextId++}`;

  const projects: Project[] = [{ id: "p1", name: "demo", path: "/work/demo" }];
  const threads: ThreadSummary[] = [
    { id: "th1", projectId: "p1", title: "Fix flaky auth test", agent: "claude-code", model: "opus", branch: null },
    { id: "th2", projectId: "p1", title: "Add retry to the sync job", agent: "qwen-code", model: "qwen3-coder", branch: null },
  ];
  const selections = new Map<string, string>();
  const turns = new Map<string, TurnSnapshot[]>([
    [
      "th1",
      [
        {
          turnId: "turn0",
          agent: "claude-code",
          model: "opus",
          input: { kind: "text", text: "The auth test fails about one run in five. Can you find out why?" },
          events: [
            { kind: "text.delta", text: "It is a timing race in the session refresh." },
            { kind: "turn.end", stopReason: "endTurn" },
          ],
        },
      ],
    ],
  ]);
  const listeners = new Set<(p: ThreadEventPayload) => void>();
  const seqs = new Map<string, number>();
  const running = new Map<string, { cancelled: boolean; resume?: (d: ApprovalDecision) => void }>();

  function emit(threadId: string, turnId: string, event: AgentEvent) {
    const seq = (seqs.get(threadId) ?? 0) + 1;
    seqs.set(threadId, seq);
    const turn = turns.get(threadId)?.find((t) => t.turnId === turnId);
    turn?.events.push(event);
    const model = turn?.events.length === 1 ? turn.model : undefined;
    for (const l of listeners) l({ threadId, turnId, seq, model, event });
  }

  function play(threadId: string, turnId: string, queue: AgentEvent[], done: () => void) {
    const run = running.get(threadId);
    const step = () => {
      if (!run || run.cancelled) return;
      const event = queue.shift();
      if (!event) return done();
      emit(threadId, turnId, event);
      if (event.kind === "approval.request") return; // wait for the answer
      if (event.kind === "turn.end") return void running.delete(threadId);
      schedule(step);
    };
    schedule(step);
  }

  function choicesFor(thread: ThreadSummary): ModelChoice[] {
    const models = thread.agent === "claude-code" ? ["opus", "sonnet"] : ["qwen3-coder", "qwen3"];
    return models.map((id) => ({ id, label: id, resolvedId: null }));
  }

  return {
    async listAgents() {
      return [{ id: "qwen-code", label: "Qwen Code", status: { kind: "ready" }, remembersApprovals: true }];
    },
    listProjects: async () => [...projects],
    async addProject(path) {
      const name = path.split("/").filter(Boolean).pop() ?? path;
      const project = { id: id("p"), name, path };
      projects.push(project);
      return project;
    },
    listThreads: async (projectId) => threads.filter((t) => t.projectId === projectId),
    worktreeInfo: async () => ({ isGit: false, base: null, reason: null }),
    async createThread(projectId, agent) {
      const thread: ThreadSummary = {
        id: id("th"),
        projectId,
        title: "New thread",
        agent,
        model: agent === "qwen-code" ? "qwen3-coder" : "opus",
        branch: null,
      };
      threads.push(thread);
      return thread;
    },
    async loadThread(threadId): Promise<ThreadSnapshot> {
      const thread = threads.find((t) => t.id === threadId);
      if (!thread) throw new Error(`unknown thread ${threadId}`);
      return {
        thread,
        turns: (turns.get(threadId) ?? []).map((t) => ({ ...t, events: [...t.events] })),
        lastSeq: seqs.get(threadId) ?? 0,
      };
    },
    reviewTurns: async (threadId) => (turns.get(threadId) ?? []).map((t) => ({ turnId: t.turnId, label: t.input.kind === "text" ? t.input.text : "Turn", available: false })),
    startWorktreeReview: async () => "mock-review",
    worktreeDiffFiles: async () => null,
    worktreeDiffFile: async () => null,
    releaseWorktreeReview: async () => {},
    turnDiffFiles: async () => null,
    turnDiffFile: async () => null,
    async listThreadModels(threadId) {
      const thread = threads.find((t) => t.id === threadId);
      if (!thread) throw new Error(`unknown thread ${threadId}`);
      return { choices: choicesFor(thread), selected: selections.get(threadId) ?? null, current: thread.model };
    },
    async setThreadModel(threadId, modelId) {
      const index = threads.findIndex((t) => t.id === threadId);
      const thread = threads[index];
      if (!thread) throw new Error(`unknown thread ${threadId}`);
      if (running.has(threadId)) throw new Error("Wait for the current turn to finish.");
      if (!choicesFor(thread).some((choice) => choice.id === modelId)) throw new Error("This model is no longer available.");
      const updated = { ...thread, model: modelId };
      threads[index] = updated;
      selections.set(threadId, modelId);
      return updated;
    },
    async sendMessage(threadId, input) {
      const thread = threads.find((t) => t.id === threadId);
      if (!thread) throw new Error(`unknown thread ${threadId}`);
      if (running.has(threadId)) throw new Error("a turn is already running");
      const turnId = id("turn");
      const list = turns.get(threadId) ?? [];
      list.push({ turnId, agent: thread.agent, model: thread.model, input, events: [] });
      turns.set(threadId, list);
      const script = scriptedTurn();
      const run: { cancelled: boolean; resume?: (d: ApprovalDecision) => void } = { cancelled: false };
      running.set(threadId, run);
      run.resume = (decision) => play(threadId, turnId, script.after(decision), () => undefined);
      play(threadId, turnId, script.before, () => undefined);
      return thread;
    },
    async answerApproval(threadId, _approvalId, decision) {
      const run = running.get(threadId);
      const resume = run?.resume;
      if (run) run.resume = undefined;
      resume?.(decision);
    },
    async cancelTurn(threadId) {
      const run = running.get(threadId);
      if (!run) return;
      run.cancelled = true;
      running.delete(threadId);
      const turnId = turns.get(threadId)?.at(-1)?.turnId;
      if (turnId) emit(threadId, turnId, { kind: "turn.end", stopReason: "cancelled" });
    },
    onThreadEvent(handler) {
      listeners.add(handler);
      return () => void listeners.delete(handler);
    },
  };
}
