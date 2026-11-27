import type { AgentId, ApprovalDecision } from "@hearth/protocol";
import { createStore, type StoreApi } from "zustand/vanilla";
import type {
  Ipc,
  Project,
  ThreadEventPayload,
  ThreadSummary,
} from "../ipc/contract";
import {
  addUserMessage,
  answerApprovalBlock,
  applyEvent,
  emptyTranscript,
  transcriptFromTurns,
  type Transcript,
} from "./transcript";

export type AppState = {
  projects: Project[];
  threads: Record<string, ThreadSummary[]>;
  transcripts: Record<string, Transcript>;
  activeThreadId: string | null;
  error: string | null;
  init(): Promise<void>;
  addProject(path: string): Promise<void>;
  createThread(projectId: string, agent?: AgentId): Promise<void>;
  selectThread(threadId: string): Promise<void>;
  send(text: string): Promise<void>;
  answerApproval(approvalId: string, decision: ApprovalDecision): Promise<void>;
  cancel(): Promise<void>;
  /** Applies every buffered event now. The frame callback calls this; tests may too. */
  flush(): void;
  dispose(): void;
};

export type AppStore = StoreApi<AppState>;

type Options = {
  /** Schedules one callback for the next animation frame. */
  raf?: (cb: () => void) => void;
  defaultAgent?: AgentId;
};

/**
 * Runs `cb` once, on the next animation frame or after 100 ms, whichever comes
 * first. WebKit pauses animation frames while the window is hidden or covered,
 * and events must still be applied then.
 */
export function nextFrame(cb: () => void): void {
  let done = false;
  const run = () => {
    if (done) return;
    done = true;
    cb();
  };
  requestAnimationFrame(run);
  setTimeout(run, 100);
}

export function createAppStore(ipc: Ipc, options: Options = {}): AppStore {
  const raf = options.raf ?? nextFrame;
  const defaultAgent = options.defaultAgent ?? "qwen-code";

  // Events wait here and are applied together, once per frame, however many arrive.
  let buffer: ThreadEventPayload[] = [];
  let frameScheduled = false;
  const loading = new Set<string>();
  let unsubscribe: (() => void) | undefined;

  return createStore<AppState>()((set, get) => {
    const setTranscript = (threadId: string, f: (t: Transcript) => Transcript) =>
      set((s) => ({
        transcripts: { ...s.transcripts, [threadId]: f(s.transcripts[threadId] ?? emptyTranscript) },
      }));
    const fail = (e: unknown) => set({ error: e instanceof Error ? e.message : String(e) });

    const flush = () => {
      frameScheduled = false;
      if (buffer.length === 0) return;
      const batch = buffer;
      buffer = [];
      set((s) => {
        const transcripts = { ...s.transcripts };
        for (const p of batch) {
          // Events for a thread that is not loaded yet are covered by its snapshot.
          const t = transcripts[p.threadId];
          if (t) transcripts[p.threadId] = applyEvent(t, p.event);
        }
        return { transcripts };
      });
    };

    const enqueue = (p: ThreadEventPayload) => {
      buffer.push(p);
      if (!frameScheduled) {
        frameScheduled = true;
        raf(flush);
      }
    };

    return {
      projects: [],
      threads: {},
      transcripts: {},
      activeThreadId: null,
      error: null,
      flush,
      dispose() {
        unsubscribe?.();
        unsubscribe = undefined;
      },
      async init() {
        unsubscribe ??= ipc.onThreadEvent(enqueue);
        try {
          const projects = await ipc.listProjects();
          const lists = await Promise.all(projects.map((p) => ipc.listThreads(p.id)));
          set({
            projects,
            threads: Object.fromEntries(projects.map((p, i) => [p.id, lists[i] ?? []])),
          });
        } catch (e) {
          fail(e);
        }
      },
      async addProject(path) {
        try {
          const project = await ipc.addProject(path);
          set((s) => ({
            projects: [...s.projects, project],
            threads: { ...s.threads, [project.id]: [] },
          }));
        } catch (e) {
          fail(e);
        }
      },
      async createThread(projectId, agent = defaultAgent) {
        try {
          const thread = await ipc.createThread(projectId, agent);
          set((s) => ({
            threads: { ...s.threads, [projectId]: [...(s.threads[projectId] ?? []), thread] },
            transcripts: { ...s.transcripts, [thread.id]: emptyTranscript },
            activeThreadId: thread.id,
          }));
        } catch (e) {
          fail(e);
        }
      },
      async selectThread(threadId) {
        set({ activeThreadId: threadId });
        if (get().transcripts[threadId] || loading.has(threadId)) return;
        loading.add(threadId);
        try {
          const snapshot = await ipc.loadThread(threadId);
          // Events that arrived while loading are already in the snapshot.
          buffer = buffer.filter((p) => p.threadId !== threadId);
          set((s) => ({
            transcripts: { ...s.transcripts, [threadId]: transcriptFromTurns(snapshot.turns) },
          }));
        } catch (e) {
          fail(e);
        } finally {
          loading.delete(threadId);
        }
      },
      async send(text) {
        const threadId = get().activeThreadId;
        const body = text.trim();
        if (!threadId || body === "" || get().transcripts[threadId]?.running) return;
        setTranscript(threadId, (t) => addUserMessage(t, body));
        try {
          await ipc.sendMessage(threadId, { kind: "text", text: body });
        } catch (e) {
          setTranscript(threadId, (t) => ({ ...t, running: false }));
          fail(e);
        }
      },
      async answerApproval(approvalId, decision) {
        const threadId = get().activeThreadId;
        if (!threadId) return;
        setTranscript(threadId, (t) => answerApprovalBlock(t, approvalId, decision));
        try {
          await ipc.answerApproval(threadId, approvalId, decision);
        } catch (e) {
          fail(e);
        }
      },
      async cancel() {
        const threadId = get().activeThreadId;
        if (!threadId || !get().transcripts[threadId]?.running) return;
        try {
          await ipc.cancelTurn(threadId);
        } catch (e) {
          fail(e);
        }
      },
    };
  });
}
