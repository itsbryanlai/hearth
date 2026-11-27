import type { AgentId, ApprovalDecision } from "@hearth/protocol";
import { createStore, type StoreApi } from "zustand/vanilla";
import type {
  InstalledAgent,
  Ipc,
  Project,
  ThreadEventPayload,
  ThreadModels,
  ThreadSummary,
} from "../ipc/contract";
import {
  addUserMessage,
  answerApprovalBlock,
  applyEvent,
  emptyTranscript,
  labelActiveReply,
  transcriptFromTurns,
  type Transcript,
} from "./transcript";

type ModelPickerState =
  | { kind: "loading" }
  | { kind: "failed"; message: string }
  | { kind: "ready" | "switching"; models: ThreadModels };

export function modelPickerBusy(state: ModelPickerState | undefined): boolean {
  return state?.kind === "loading" || state?.kind === "switching";
}

export type AppState = {
  agents: InstalledAgent[];
  projects: Project[];
  threads: Record<string, ThreadSummary[]>;
  transcripts: Record<string, Transcript>;
  modelPickers: Record<string, ModelPickerState>;
  activeThreadId: string | null;
  error: string | null;
  init(): Promise<void>;
  addProject(path: string): Promise<void>;
  createThread(projectId: string, agent?: AgentId): Promise<void>;
  selectThread(threadId: string): Promise<void>;
  loadModels(threadId: string): Promise<void>;
  setModel(threadId: string, modelId: string): Promise<void>;
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
  const pendingAnswers = new Set<string>();
  const pendingSends = new Map<string, { modelUpdated: boolean }>();
  let unsubscribe: (() => void) | undefined;

  return createStore<AppState>()((set, get) => {
    const setTranscript = (threadId: string, f: (t: Transcript) => Transcript) =>
      set((s) => ({
        transcripts: { ...s.transcripts, [threadId]: f(s.transcripts[threadId] ?? emptyTranscript) },
      }));
    const fail = (e: unknown) => set({ error: e instanceof Error ? e.message : String(e) });

    const refreshModelChoices = async (threadId: string) => {
      let picker: ModelPickerState;
      try {
        const models = await ipc.listThreadModels(threadId);
        picker = { kind: "ready", models };
      } catch (e) {
        picker = { kind: "failed", message: e instanceof Error ? e.message : String(e) };
      }
      set((s) => ({ modelPickers: { ...s.modelPickers, [threadId]: picker } }));
    };

    const flush = () => {
      frameScheduled = false;
      if (buffer.length === 0) return;
      const batch = buffer;
      buffer = [];
      set((s) => {
        const transcripts = { ...s.transcripts };
        let models: Map<string, string> | undefined;
        for (const p of batch) {
          // Events for a thread that is not loaded yet are covered by its snapshot.
          if (p.model !== undefined) {
            (models ??= new Map()).set(p.threadId, p.model);
            const pending = pendingSends.get(p.threadId);
            if (pending) pending.modelUpdated = true;
          }
          const t = transcripts[p.threadId];
          if (t) {
            const labeled = p.model === undefined ? t : labelActiveReply(t, p.model);
            transcripts[p.threadId] = applyEvent(labeled, p.event);
          }
        }
        const updatedModels = models;
        if (!updatedModels) return { transcripts };
        const threads = Object.fromEntries(
          Object.entries(s.threads).map(([project, list]) => [
            project,
            list.map((thread) => {
              const model = updatedModels.get(thread.id);
              return model === undefined || model === thread.model ? thread : { ...thread, model };
            }),
          ]),
        );
        return { transcripts, threads };
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
      agents: [],
      projects: [],
      threads: {},
      transcripts: {},
      modelPickers: {},
      activeThreadId: null,
      error: null,
      flush,
      dispose() {
        unsubscribe?.();
        unsubscribe = undefined;
      },
      async init() {
        unsubscribe ??= ipc.onThreadEvent(enqueue);
        const detection = ipc.listAgents().then((agents) => set({ agents }), fail);
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
        await detection;
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
            threads: {
              ...s.threads,
              [snapshot.thread.projectId]: (s.threads[snapshot.thread.projectId] ?? []).map(
                (thread) => thread.id === threadId ? snapshot.thread : thread,
              ),
            },
          }));
        } catch (e) {
          fail(e);
        } finally {
          loading.delete(threadId);
        }
      },
      async loadModels(threadId) {
        const state = get();
        if (state.transcripts[threadId]?.running || modelPickerBusy(state.modelPickers[threadId])) return;
        set((s) => ({ modelPickers: { ...s.modelPickers, [threadId]: { kind: "loading" } } }));
        await refreshModelChoices(threadId);
      },
      async setModel(threadId, modelId) {
        const state = get();
        const picker = state.modelPickers[threadId];
        if (picker?.kind !== "ready" || state.transcripts[threadId]?.running) return;
        const { models } = picker;
        set((s) => ({
          error: null,
          modelPickers: { ...s.modelPickers, [threadId]: { kind: "switching", models } },
        }));
        try {
          const summary = await ipc.setThreadModel(threadId, modelId);
          const pending = pendingSends.get(threadId);
          if (pending) pending.modelUpdated = true;
          set((s) => ({
            modelPickers: {
              ...s.modelPickers,
              [threadId]: { kind: "ready", models: { ...models, selected: modelId } },
            },
            threads: {
              ...s.threads,
              [summary.projectId]: (s.threads[summary.projectId] ?? []).map(
                (thread) => (thread.id === summary.id ? summary : thread),
              ),
            },
          }));
        } catch (e) {
          fail(e);
          await refreshModelChoices(threadId);
        }
      },
      async send(text) {
        const threadId = get().activeThreadId;
        const body = text.trim();
        if (!threadId || body === "" || get().transcripts[threadId]?.running || modelPickerBusy(get().modelPickers[threadId])) return;
        const pending = { modelUpdated: false };
        pendingSends.set(threadId, pending);
        setTranscript(threadId, (t) => addUserMessage(t, body));
        try {
          const summary = await ipc.sendMessage(threadId, { kind: "text", text: body });
          if (pendingSends.get(threadId) !== pending) return;
          set((s) => ({
            threads: Object.fromEntries(
              Object.entries(s.threads).map(([project, list]) => [
                project,
                list.map((t) =>
                  t.id === summary.id ? { ...summary, model: pending.modelUpdated ? t.model : summary.model || t.model } : t,
                ),
              ]),
            ),
          }));
        } catch (e) {
          if (pendingSends.get(threadId) !== pending) return;
          setTranscript(threadId, (t) => ({ ...t, running: false }));
          fail(e);
        } finally {
          if (pendingSends.get(threadId) === pending) pendingSends.delete(threadId);
        }
      },
      async answerApproval(approvalId, decision) {
        const threadId = get().activeThreadId;
        if (!threadId) return;
        const pending = JSON.stringify([threadId, approvalId]);
        if (pendingAnswers.has(pending)) return;
        pendingAnswers.add(pending);
        try {
          await ipc.answerApproval(threadId, approvalId, decision);
          setTranscript(threadId, (t) => answerApprovalBlock(t, approvalId, decision));
        } catch (e) {
          fail(e);
        } finally {
          pendingAnswers.delete(pending);
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
