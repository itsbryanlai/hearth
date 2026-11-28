import type { AgentEvent, ApprovalDecision } from "@hearth/protocol";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { App } from "./App";
import type { InstalledAgent, Ipc, ThreadEventPayload, ThreadModels, ThreadSnapshot } from "./ipc/contract";
import { createMockIpc } from "./ipc/mock";
import { createAppStore, nextFrame } from "./state/store";

const THREAD = { id: "th", projectId: "p", title: "Thread one", agent: "claude-code", model: "opus", branch: null, archived: false };

beforeEach(() => {
  // jsdom has no layout: give every element a size so the virtualizer has a viewport.
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({
    width: 800, height: 40, top: 0, left: 0, right: 800, bottom: 40, x: 0, y: 0, toJSON: () => ({}),
  });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, value: 800 });
  Object.defineProperty(HTMLElement.prototype, "clientWidth", { configurable: true, value: 800 });
  HTMLElement.prototype.scrollTo = function (this: HTMLElement, opts?: ScrollToOptions | number) {
    this.scrollTop = typeof opts === "object" ? (opts.top ?? 0) : 0;
    this.dispatchEvent(new Event("scroll"));
  };
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** A manual animation-frame queue. */
function frames() {
  const queue: (() => void)[] = [];
  return {
    raf: (cb: () => void) => void queue.push(cb),
    pending: () => queue.length,
    run: () => act(() => queue.splice(0).forEach((cb) => cb())),
  };
}

const QWEN: InstalledAgent = {
  id: "qwen-code",
  label: "Qwen Code",
  status: { kind: "ready" },
  remembersApprovals: true,
};
const CLAUDE: InstalledAgent = {
  id: "claude-code",
  label: "Claude Code",
  status: { kind: "ready" },
  remembersApprovals: false,
};

function fakeIpc(snapshot: ThreadSnapshot = { thread: THREAD, turns: [], lastSeq: 0 }) {
  let emit: (p: ThreadEventPayload) => void = () => {};
  const ipc = {
    listAgents: vi.fn(async (): Promise<InstalledAgent[]> => [{ ...CLAUDE, remembersApprovals: true }]),
    usageSummary: vi.fn<Ipc["usageSummary"]>(async () => []),
    listProjects: vi.fn(async () => [{ id: "p", name: "proj", path: "/work/proj" }]),
    addProject: vi.fn(),
    listThreads: vi.fn<Ipc["listThreads"]>(async () => [THREAD]),
    worktreeInfo: vi.fn<Ipc["worktreeInfo"]>(async () => ({ isGit: false, base: null, reason: null })),
    createThread: vi.fn(),
    loadThread: vi.fn(async () => snapshot),
    reviewTurns: vi.fn<Ipc["reviewTurns"]>(async () => []),
    startWorktreeReview: vi.fn<Ipc["startWorktreeReview"]>(async () => "review"),
    worktreeDiffFiles: vi.fn<Ipc["worktreeDiffFiles"]>(async () => null),
    worktreeDiffFile: vi.fn<Ipc["worktreeDiffFile"]>(async () => null),
    releaseWorktreeReview: vi.fn<Ipc["releaseWorktreeReview"]>(async () => {}),
    turnDiffFiles: vi.fn<Ipc["turnDiffFiles"]>(async () => null),
    turnDiffFile: vi.fn<Ipc["turnDiffFile"]>(async () => null),
    archiveThread: vi.fn(async () => ({ ...THREAD, archived: true })),
    restoreThread: vi.fn(async () => THREAD),
    cleanupPath: vi.fn<Ipc["cleanupPath"]>(async () => null),
    removeThreadWorktree: vi.fn(async () => {}),
    deleteThreadBranch: vi.fn(async () => {}),
    listThreadModels: vi.fn(async (): Promise<ThreadModels> => ({ choices: [], selected: null, current: null })),
    setThreadModel: vi.fn<Ipc["setThreadModel"]>(async () => THREAD),
    draftHandoff: vi.fn<Ipc["draftHandoff"]>(async (threadId, target) => ({ threadId, sourceAgent: "claude-code", targetAgent: target, text: `Draft for ${target}` })),
    startHandoff: vi.fn<Ipc["startHandoff"]>(async () => ({ ...THREAD, agent: "qwen-code" })),
    sendMessage: vi.fn(async () => THREAD),
    answerApproval: vi.fn(async () => {}),
    cancelTurn: vi.fn(async () => {}),
    onThreadEvent: (h: (p: ThreadEventPayload) => void) => {
      emit = h;
      return () => {};
    },
  } satisfies Record<keyof Ipc, unknown>;
  const send = (event: AgentEvent, seq = 1, model?: string) =>
    act(() => emit({ threadId: "th", turnId: "t1", seq, event, model }));
  return { ipc: ipc as Ipc & typeof ipc, send };
}

async function openThread(ipc: Ipc, f = frames()) {
  const store = createAppStore(ipc, { raf: f.raf });
  render(<App store={store} />);
  fireEvent.click(await screen.findByRole("button", { name: /^Thread one/ }));
  await screen.findByLabelText("Message");
  await vi.waitFor(() => expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(false));
  return { store, f };
}

// Fails if rejection leaves a stale catalog, a change loses an alias or relabels old replies,
// or Send is enabled before the new selection is ready.
test("model changes keep historical labels and accept only the acknowledged selection", async () => {
  const { ipc, send } = fakeIpc({
    thread: THREAD,
    lastSeq: 2,
    turns: [{
      turnId: "prior",
      agent: "claude-code",
      model: "prior-model",
      input: { kind: "text", text: "Earlier request" },
      events: [
        { kind: "text.delta", text: "Earlier reply" },
        { kind: "turn.end", stopReason: "endTurn" },
      ],
    }],
  });
  ipc.listThreadModels.mockResolvedValue({
    selected: null,
    current: "opus",
    choices: [
      { id: "default", label: "Default", resolvedId: "opus" },
      { id: "opus", label: "Opus", resolvedId: "opus" },
      { id: "sonnet", label: "Sonnet", resolvedId: "sonnet" },
    ],
  });
  const { f } = await openThread(ipc);
  fireEvent.click(screen.getByRole("button", { name: "Choose model" }));
  const picker = await screen.findByRole("combobox", { name: "Model" });

  let refresh: () => void = () => {};
  ipc.listThreadModels.mockImplementationOnce(() => new Promise((resolve) => {
    refresh = () => resolve({
      selected: null,
      current: "haiku",
      choices: [
        { id: "default", label: "Default", resolvedId: "opus" },
        { id: "opus", label: "Opus", resolvedId: "opus" },
        { id: "haiku", label: "Haiku", resolvedId: "haiku" },
      ],
    });
  }));
  ipc.setThreadModel.mockRejectedValueOnce(new Error("The model change was rejected."));
  fireEvent.change(picker, { target: { value: "sonnet" } });
  expect((await screen.findByRole("alert")).textContent).toBe("The model change was rejected.");
  expect(screen.getByRole("button", { name: "Send" }).hasAttribute("disabled")).toBe(true);
  await act(async () => refresh());

  expect(picker).toBeInstanceOf(HTMLSelectElement);
  if (!(picker instanceof HTMLSelectElement)) throw new Error("Expected a model select");
  expect(picker.value).toBe("opus");
  expect(within(picker).queryByRole("option", { name: "Sonnet" })).toBeNull();
  expect(within(picker).getByRole("option", { name: "Haiku" })).toBeDefined();

  let acknowledge: () => void = () => {};
  ipc.setThreadModel.mockImplementationOnce(() => new Promise((resolve) => {
    acknowledge = () => resolve(THREAD);
  }));
  fireEvent.change(picker, { target: { value: "default" } });
  expect(screen.getByRole("button", { name: "Send" }).hasAttribute("disabled")).toBe(true);
  await act(async () => acknowledge());

  expect(ipc.setThreadModel).toHaveBeenLastCalledWith("th", "default");
  expect(picker.value).toBe("default");
  expect(screen.getByText("Reply model: prior-model")).toBeDefined();
  expect(screen.getByRole("button", { name: "Send" }).hasAttribute("disabled")).toBe(false);

  let finishSend: () => void = () => {};
  ipc.sendMessage.mockImplementationOnce(() => new Promise((resolve) => {
    finishSend = () => resolve({ ...THREAD, title: "Updated thread title", model: "old-model" });
  }));
  fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Another request" } });
  fireEvent.click(screen.getByRole("button", { name: "Send" }));
  await send({ kind: "text.delta", text: "Live reply" }, 3, "observed-model");
  f.run();
  await act(async () => finishSend());
  expect(screen.getByRole("button", { name: "Updated thread title" })).toBeDefined();
  expect(screen.getByText(/claude-code · observed-model/)).toBeDefined();
  expect(screen.getByText("Reply model: observed-model")).toBeDefined();
});

// Fails if each delta is rendered on arrival, or if deltas become separate blocks.
test("deltas arriving within one frame show as one merged block after the frame", async () => {
  const { ipc, send } = fakeIpc();
  const { f } = await openThread(ipc);
  for (const text of ["Hel", "lo ", "world"]) await send({ kind: "text.delta", text });
  expect(screen.queryByText(/Hel/)).toBeNull();
  expect(f.pending()).toBe(1);
  f.run();
  const blocks = document.querySelectorAll(".block.text");
  expect(blocks).toHaveLength(1);
  expect(blocks[0]?.textContent).toBe("Hello world");
});

// Fails if a button sends the wrong decision, or none, to the backend.
test.each([
  ["Allow once", "allowOnce"],
  ["Allow for this thread", "allowForThread"],
  ["Deny", "deny"],
] as [string, ApprovalDecision][])("approval button %s answers %s", async (label, decision) => {
  const { ipc, send } = fakeIpc();
  const { f } = await openThread(ipc);
  await send({
    kind: "approval.request",
    request: { id: "a9", tool: "Bash", title: "Run tests", detail: "cargo test", options: [] },
  });
  f.run();
  let accept = () => {};
  ipc.answerApproval.mockImplementationOnce(() => new Promise<void>((resolve) => { accept = resolve; }));
  const button = await screen.findByRole("button", { name: label });
  fireEvent.click(button);
  fireEvent.click(button);
  expect(ipc.answerApproval).toHaveBeenCalledExactlyOnceWith("th", "a9", decision);
  expect(screen.getByRole("button", { name: label })).toBe(button);
  await act(async () => accept());
  expect(screen.queryByRole("button", { name: label })).toBeNull();
});

// Fails if the shortcut does not send, does not clear the box, or plain Enter sends too.
test("Cmd+Enter sends the message and clears the composer; plain Enter does not send", async () => {
  const { ipc } = fakeIpc();
  await openThread(ipc);
  const box = screen.getByLabelText("Message") as HTMLTextAreaElement;
  fireEvent.change(box, { target: { value: "fix the test" } });
  fireEvent.keyDown(box, { key: "Enter" });
  expect(ipc.sendMessage).not.toHaveBeenCalled();
  fireEvent.keyDown(box, { key: "Enter", metaKey: true });
  expect(ipc.sendMessage).toHaveBeenCalledWith("th", { kind: "text", text: "fix the test" });
  expect(box.value).toBe("");
});

// Fails if the header keeps the empty model a new thread starts with, instead of the
// model the agent reported when the turn started.
test("the header shows the model the agent reported after the first message", async () => {
  const fresh = { ...THREAD, model: "" };
  const { ipc } = fakeIpc({ thread: fresh, turns: [], lastSeq: 0 });
  ipc.listThreads.mockImplementation(async () => [fresh]);
  ipc.sendMessage.mockImplementation(async () => ({ ...fresh, model: "qwen4-max" }));
  await openThread(ipc);
  expect(screen.queryByText(/qwen4-max/)).toBeNull();
  const box = screen.getByLabelText("Message");
  fireEvent.change(box, { target: { value: "hi" } });
  fireEvent.keyDown(box, { key: "Enter", metaKey: true });
  expect(await screen.findByText(/qwen4-max/)).toBeTruthy();
});

// Fails if Cmd+. does not cancel the running turn, or cancels when nothing runs.
test("Cmd+. cancels the running turn only", async () => {
  const { ipc } = fakeIpc();
  await openThread(ipc);
  const box = screen.getByLabelText("Message") as HTMLTextAreaElement;
  fireEvent.keyDown(box, { key: ".", metaKey: true });
  expect(ipc.cancelTurn).not.toHaveBeenCalled();
  fireEvent.change(box, { target: { value: "go" } });
  fireEvent.keyDown(box, { key: "Enter", metaKey: true });
  fireEvent.keyDown(box, { key: ".", metaKey: true });
  expect(ipc.cancelTurn).toHaveBeenCalledWith("th");
});

// Fails if the transcript mounts a row per block instead of only the visible ones.
test("a 10,000-event thread keeps fewer than 100 rows in the DOM", async () => {
  const events: AgentEvent[] = Array.from({ length: 10_000 }, (_, i) => ({ kind: "error", message: `e${i}` }));
  const { ipc } = fakeIpc({
    thread: THREAD,
    turns: [{ turnId: "t1", agent: "claude-code", model: "opus", input: { kind: "text", text: "hi" }, events }],
    lastSeq: events.length,
  });
  await openThread(ipc);
  const rows = document.querySelectorAll(".transcript .row");
  expect(rows.length).toBeGreaterThan(0);
  expect(rows.length).toBeLessThan(100);
});

// Fails if the send -> stream -> approval -> turn end flow breaks anywhere between UI, store and IPC.
test("the mock backend drives a thread from send to the end of the turn", async () => {
  const steps: (() => void)[] = [];
  const f = frames();
  const ipc = createMockIpc({ schedule: (s) => void steps.push(s) });
  const store = createAppStore(ipc, { raf: f.raf });
  render(<App store={store} />);
  fireEvent.click(await screen.findByRole("button", { name: "Fix flaky auth test" }));
  await screen.findByText(/timing race in the session refresh/);

  const box = screen.getByLabelText("Message");
  fireEvent.change(box, { target: { value: "run it again" } });
  fireEvent.keyDown(box, { key: "Enter", metaKey: true });

  const drain = () => {
    while (steps.length > 0) act(() => steps.shift()?.());
    f.run();
  };
  await vi.waitFor(() => expect(steps.length).toBeGreaterThan(0));
  drain(); // runs up to the approval request
  const card = await screen.findByRole("group", { name: /Run the auth tests/ });
  expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
  fireEvent.click(within(card).getByRole("button", { name: "Allow once" }));
  await vi.waitFor(() => expect(steps.length).toBeGreaterThan(0));
  drain();
  expect(await screen.findByText(/passes ten runs in a row/)).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
});

// Fails if events wait for an animation frame that a hidden window never gets,
// or if the callback runs twice when both the frame and the timer fire.
test("the frame scheduler falls back to a timer and runs once", () => {
  vi.useFakeTimers();
  const frameCbs: FrameRequestCallback[] = [];
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => frameCbs.push(cb));
  const cb = vi.fn();
  nextFrame(cb);
  expect(cb).not.toHaveBeenCalled();
  vi.advanceTimersByTime(100);
  expect(cb).toHaveBeenCalledTimes(1);
  frameCbs.forEach((f) => f(0));
  expect(cb).toHaveBeenCalledTimes(1);
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

// Fails if the selector ignores availability or sends the default agent instead of the chosen one.
test.each([
  { status: { kind: "ready" }, expected: CLAUDE.id },
  { status: { kind: "unsupported", version: "1.0.0" }, expected: QWEN.id },
] satisfies { status: InstalledAgent["status"]; expected: InstalledAgent["id"] }[])(
  "new thread routes to an available agent when Claude is $status.kind",
  async ({ status, expected }) => {
    const { ipc } = fakeIpc();
    ipc.listAgents.mockResolvedValue([QWEN, { ...CLAUDE, status }]);
    ipc.createThread.mockResolvedValue({ ...THREAD, agent: expected });
    const store = createAppStore(ipc);
    render(<App store={store} />);
    const button = await screen.findByRole("button", { name: "+ Thread" });
    await vi.waitFor(() => expect(button).not.toHaveProperty("disabled", true));
    fireEvent.click(button);
    const selector = screen.getByRole("combobox", { name: "Agent" });
    if (status.kind === "ready") {
      fireEvent.change(selector, { target: { value: CLAUDE.id } });
    } else {
      expect(within(selector).getByRole("option", { name: /Claude Code.*unsupported version 1.0.0/ }))
        .toHaveProperty("disabled", true);
    }
    await vi.waitFor(() => expect(screen.getByRole("button", { name: "Create thread" })).not.toHaveProperty("disabled", true));
    fireEvent.click(screen.getByRole("button", { name: "Create thread" }));
    await vi.waitFor(() => expect(ipc.createThread).toHaveBeenCalledWith("p", expected, null, null));
  },
);

// Fails if a single ready agent bypasses the directory choice or the selected base.
test.each([
  { mode: "worktree", expected: "topic" },
  { mode: "repo", expected: null },
])("new thread uses the selected $mode directory", async ({ mode, expected }) => {
  const { ipc } = fakeIpc();
  ipc.worktreeInfo.mockResolvedValue({ isGit: true, base: "main", reason: null });
  ipc.createThread.mockResolvedValue(THREAD);
  render(<App store={createAppStore(ipc)} />);
  fireEvent.click(await screen.findByRole("button", { name: "+ Thread" }));
  const directory = await screen.findByRole("combobox", { name: "Working directory" });
  expect(directory).toHaveProperty("value", "worktree");
  fireEvent.change(screen.getByRole("textbox", { name: "Base" }), { target: { value: "topic" } });
  fireEvent.change(screen.getByRole("textbox", { name: "Worktree parent" }), { target: { value: "/tmp/trees" } });
  if (mode === "repo") {
    fireEvent.change(directory, { target: { value: "repo" } });
    expect(screen.queryByRole("textbox", { name: "Worktree parent" })).toBeNull();
  }
  fireEvent.click(screen.getByRole("button", { name: "Create thread" }));
  await vi.waitFor(() => expect(ipc.createThread).toHaveBeenCalledWith("p", CLAUDE.id, expected, mode === "worktree" ? "/tmp/trees" : null));
});

// Fails if a rejected creation loses the chosen inputs or allows duplicate submissions.
test("worktree creation retains inputs after failure and closes after retry", async () => {
  const { ipc } = fakeIpc();
  ipc.worktreeInfo.mockResolvedValue({ isGit: true, base: "main", reason: null });
  let rejectCreation: (error: Error) => void = () => {};
  ipc.createThread.mockImplementationOnce(() => new Promise((_, reject) => { rejectCreation = reject; }));
  ipc.createThread.mockResolvedValueOnce(THREAD);
  render(<App store={createAppStore(ipc)} />);
  fireEvent.click(await screen.findByRole("button", { name: "+ Thread" }));
  const base = await screen.findByRole("textbox", { name: "Base" });
  const parent = screen.getByRole("textbox", { name: "Worktree parent" });
  fireEvent.change(base, { target: { value: "topic" } });
  fireEvent.change(parent, { target: { value: "/tmp/missing" } });
  const submit = screen.getByRole("button", { name: "Create thread" });
  fireEvent.click(submit);
  expect(submit).toHaveProperty("disabled", true);
  fireEvent.click(submit);
  expect(ipc.createThread).toHaveBeenCalledTimes(1);
  await act(async () => rejectCreation(new Error("Folder does not exist")));
  expect((await screen.findByRole("alert")).textContent).toContain("Folder does not exist");
  expect(base).toHaveProperty("value", "topic");
  expect(parent).toHaveProperty("value", "/tmp/missing");
  fireEvent.change(parent, { target: { value: "/tmp/existing" } });
  fireEvent.click(submit);
  await vi.waitFor(() => expect(screen.queryByRole("button", { name: "Create thread" })).toBeNull());
  expect(ipc.createThread).toHaveBeenLastCalledWith("p", CLAUDE.id, "topic", "/tmp/existing");
});

// Fails if an agent without a known approval scope offers a remembered approval.
test("an unscoped approval only offers one-shot choices", async () => {
  const { ipc, send } = fakeIpc();
  ipc.listAgents.mockResolvedValue([CLAUDE]);
  const { f } = await openThread(ipc);
  await send({
    kind: "approval.request",
    request: { id: "a", tool: "Read", title: "Read file", detail: "", options: [] },
  });
  f.run();
  expect(screen.getByRole("button", { name: "Allow once" })).toBeTruthy();
  expect(screen.getByRole("button", { name: "Deny" })).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Allow for this thread" })).toBeNull();
});

// Fails if the latest turn or selected file cannot be reviewed and closing loses the composer.
test("opens changes and selects a historical turn and file", async () => {
  const { ipc } = fakeIpc();
  ipc.reviewTurns.mockResolvedValue([{ turnId: "old", label: "Earlier", available: true }, { turnId: "new", label: "Latest", available: false }]);
  ipc.turnDiffFiles.mockImplementation(async (_thread, turn) => [{ id: turn, path: `${turn}.txt`, oldPath: null, status: "modified" }]);
  ipc.turnDiffFile.mockImplementation(async (_thread, _turn, file) => ({ binary: false, hunks: [{ header: "@@", rows: [{ kind: "add", text: `${file} content`, oldLine: null, newLine: 1 }], splitRows: [{ left: null, right: 0, notice: null }] }] }));
  await openThread(ipc);
  fireEvent.change(screen.getByLabelText("Message"), { target: { value: "draft" } });
  fireEvent.click(screen.getByRole("button", { name: "Changes" }));
  expect(await screen.findByText("old content", { exact: false })).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Turn"), { target: { value: "new" } });
  expect(await screen.findByText("new content", { exact: false })).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Close changes" }));
  expect((screen.getByLabelText("Message") as HTMLTextAreaElement).value).toBe("draft");
});

// Fails if changing the view hides line numbers or the changed text.
test("switches between unified and side by side diffs", async () => {
  const { ipc } = fakeIpc();
  ipc.reviewTurns.mockResolvedValue([{ turnId: "t", label: "Turn", available: true }]);
  ipc.turnDiffFiles.mockResolvedValue([{ id: "f", path: "file", oldPath: null, status: "modified" }]);
  ipc.turnDiffFile.mockResolvedValue({ binary: false, hunks: [{ header: "@@", rows: [{ kind: "delete", text: "before", oldLine: 4, newLine: null }, { kind: "add", text: "after", oldLine: null, newLine: 5 }], splitRows: [{ left: 0, right: 1, notice: null }] }] });
  await openThread(ipc);
  fireEvent.click(screen.getByRole("button", { name: "Changes" }));
  expect(await screen.findByText(/4.*before/)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Side by side" }));
  expect(screen.getByText(/4.*before/)).toBeTruthy();
  expect(screen.getByText(/5.*after/)).toBeTruthy();
  expect(screen.getByText(/4.*before/).parentElement).toBe(screen.getByText(/5.*after/).parentElement);
});

// Fails if a late response replaces the currently selected turn's empty result.
test("keeps unavailable and empty turn results scoped to the selection", async () => {
  const { ipc } = fakeIpc();
  let resolveOld: (value: Awaited<ReturnType<Ipc["turnDiffFiles"]>>) => void = () => {};
  ipc.reviewTurns.mockResolvedValue([{ turnId: "old", label: "Earlier", available: true }, { turnId: "new", label: "Latest", available: true }]);
  ipc.turnDiffFiles.mockImplementation((_thread, turn) => turn === "old" ? new Promise((resolve) => { resolveOld = resolve; }) : Promise.resolve([]));
  await openThread(ipc);
  fireEvent.click(screen.getByRole("button", { name: "Changes" }));
  expect(await screen.findByText("No files changed in this turn.")).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Turn"), { target: { value: "old" } });
  fireEvent.change(screen.getByLabelText("Turn"), { target: { value: "new" } });
  await act(async () => resolveOld(null));
  expect(await screen.findByText("No files changed in this turn.")).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Turn"), { target: { value: "old" } });
  await act(async () => resolveOld(null));
  expect(await screen.findByText("Snapshot unavailable for this turn.")).toBeTruthy();
});

// Fails if refresh or switching scope leaves a review allocated or loses the historical diff.
test("refreshes and releases worktree reviews while preserving turn changes", async () => {
  const thread = { ...THREAD, branch: "feature" };
  const { ipc } = fakeIpc({ thread, lastSeq: 0, turns: [] });
  ipc.listThreads.mockResolvedValue([thread]);
  ipc.reviewTurns.mockResolvedValue([{ turnId: "t", label: "Turn", available: true }]);
  ipc.turnDiffFiles.mockResolvedValue([{ id: "f", path: "turn.txt", oldPath: null, status: "modified" }]);
  ipc.turnDiffFile.mockResolvedValue({ binary: false, hunks: [{ header: "@@", rows: [{ kind: "add", text: "historical", oldLine: null, newLine: 1 }], splitRows: [] }] });
  ipc.startWorktreeReview.mockResolvedValueOnce("one").mockResolvedValueOnce("two");
  ipc.worktreeDiffFiles.mockImplementation(async (_thread, id) => ({ id, files: [{ id: "wf", path: "worktree.txt", oldPath: null, status: "modified" }] }));
  ipc.worktreeDiffFile.mockImplementation(async (_thread, id) => ({ binary: false, hunks: [{ header: "@@", rows: [{ kind: "add", text: `worktree ${id}`, oldLine: null, newLine: 1 }], splitRows: [] }] }));
  await openThread(ipc);
  fireEvent.click(screen.getByRole("button", { name: "Changes" }));
  expect(await screen.findByText(/historical/)).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Scope"), { target: { value: "worktree" } });
  expect(await screen.findByText(/worktree one/)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  await vi.waitFor(() => expect(ipc.releaseWorktreeReview).toHaveBeenCalledWith("th", "one"));
  expect(await screen.findByText(/worktree two/)).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Scope"), { target: { value: "turn" } });
  await vi.waitFor(() => expect(ipc.releaseWorktreeReview).toHaveBeenCalledWith("th", "two"));
  expect(await screen.findByText(/historical/)).toBeTruthy();
});

// Fails if a reservation arriving after close is captured or left allocated.
test("releases a late worktree reservation after closing changes", async () => {
  const thread = { ...THREAD, branch: "feature" };
  const { ipc } = fakeIpc({ thread, lastSeq: 0, turns: [] });
  ipc.listThreads.mockResolvedValue([thread]);
  let resolveStart: (id: string) => void = () => {};
  ipc.startWorktreeReview.mockImplementationOnce(() => new Promise((resolve) => { resolveStart = resolve; }));
  ipc.worktreeDiffFiles.mockResolvedValue({ id: "review", files: [] });
  await openThread(ipc);
  fireEvent.click(screen.getByRole("button", { name: "Changes" }));
  fireEvent.change(screen.getByLabelText("Scope"), { target: { value: "worktree" } });
  await vi.waitFor(() => expect(ipc.startWorktreeReview).toHaveBeenCalledTimes(1));
  fireEvent.click(screen.getByRole("button", { name: "Close changes" }));
  fireEvent.click(screen.getByRole("button", { name: "Changes" }));
  fireEvent.change(screen.getByLabelText("Scope"), { target: { value: "worktree" } });
  expect(ipc.startWorktreeReview).toHaveBeenCalledTimes(1);
  await act(async () => resolveStart("late"));
  expect(ipc.releaseWorktreeReview).toHaveBeenCalledWith("th", "late");
  expect(await screen.findByText("No files changed in this worktree.")).toBeTruthy();
  expect(ipc.startWorktreeReview).toHaveBeenCalledTimes(2);
  expect(ipc.worktreeDiffFiles).toHaveBeenCalledOnce();
  expect(ipc.worktreeDiffFiles).toHaveBeenCalledWith("th", "review");
});

// Fails if archived history disappears or allows sending before it is restored.
test("archive hides a thread while keeping its history available for restore", async () => {
  const ipc = createMockIpc();
  const store = createAppStore(ipc);
  render(<App store={store} />);
  fireEvent.click(await screen.findByRole("button", { name: "Fix flaky auth test" }));
  await screen.findByText("It is a timing race in the session refresh.");
  fireEvent.click(screen.getByRole("button", { name: "Archive" }));
  await vi.waitFor(() => expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(true));
  expect(screen.queryByRole("button", { name: "Fix flaky auth test (archived)" })).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Show archived" }));
  expect(screen.getByRole("button", { name: "Fix flaky auth test (archived)" })).toBeTruthy();
  expect(screen.getByText("It is a timing race in the session refresh.")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Restore" }));
  await vi.waitFor(() => expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(false));
});

// Fails if worktree removal starts before the user confirms the exact path.
test("archived worktree cleanup requires explicit confirmation", async () => {
  const { ipc } = fakeIpc();
  ipc.cleanupPath.mockResolvedValue("/tmp/example/tree");
  await openThread(ipc);
  fireEvent.click(screen.getByRole("button", { name: "Archive" }));
  await screen.findByRole("button", { name: "Worktree cleanup" });
  fireEvent.click(screen.getByRole("button", { name: "Worktree cleanup" }));
  await screen.findByText("Worktree: /tmp/example/tree");
  fireEvent.click(screen.getByRole("button", { name: "Remove worktree" }));
  expect(screen.getByText("Remove the clean worktree at /tmp/example/tree?")).toBeTruthy();
  expect(ipc.removeThreadWorktree).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Confirm removal" }));
  await vi.waitFor(() => expect(ipc.removeThreadWorktree).toHaveBeenCalledWith("th"));
});

// Fails if pending archive accepts a message or a late history load revives the thread.
test("pending archive blocks sending and survives a late thread load", async () => {
  const { ipc } = fakeIpc();
  let finishLoad: (snapshot: ThreadSnapshot) => void = () => {};
  let finishArchive: (thread: typeof THREAD) => void = () => {};
  ipc.loadThread.mockImplementation(() => new Promise((resolve) => { finishLoad = resolve; }));
  ipc.archiveThread.mockImplementation(() => new Promise((resolve) => { finishArchive = resolve; }));
  const store = createAppStore(ipc);
  render(<App store={store} />);
  fireEvent.click(await screen.findByRole("button", { name: "Thread one" }));
  fireEvent.click(screen.getByRole("button", { name: "Archive" }));
  expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(true);
  await act(async () => { await store.getState().send("must not appear"); });
  expect(ipc.sendMessage).not.toHaveBeenCalled();
  await act(async () => { finishArchive({ ...THREAD, archived: true }); });
  await act(async () => { finishLoad({ thread: THREAD, turns: [], lastSeq: 0 }); });
  expect(screen.getByRole("button", { name: "Restore" })).toBeTruthy();
  expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(true);
});

// Fails if Start does not send the edited draft to the chosen target.
test("hand-off Start sends the edited draft to the chosen agent and the header follows", async () => {
  const { ipc } = fakeIpc();
  ipc.listAgents.mockResolvedValue([CLAUDE, QWEN]);
  await openThread(ipc);
  await vi.waitFor(() => expect(screen.getByRole("button", { name: "Hand off" }).hasAttribute("disabled")).toBe(false));
  fireEvent.click(screen.getByRole("button", { name: "Hand off" }));
  const box = (await screen.findByLabelText("Hand-off message")) as HTMLTextAreaElement;
  await vi.waitFor(() => expect(box.value).toBe("Draft for qwen-code"));
  fireEvent.change(box, { target: { value: "Edited preamble" } });
  fireEvent.click(screen.getByRole("button", { name: "Start" }));
  await vi.waitFor(() => expect(ipc.startHandoff).toHaveBeenCalledWith("th", "qwen-code", "Edited preamble"));
  expect(await screen.findByText(/qwen-code ·/)).toBeDefined();
  expect(screen.queryByLabelText("Hand-off message")).toBeNull();
});

// Fails if Cancel calls the backend to start the hand-off.
test("hand-off Cancel closes the panel without starting anything", async () => {
  const { ipc } = fakeIpc();
  ipc.listAgents.mockResolvedValue([CLAUDE, QWEN]);
  await openThread(ipc);
  await vi.waitFor(() => expect(screen.getByRole("button", { name: "Hand off" }).hasAttribute("disabled")).toBe(false));
  fireEvent.click(screen.getByRole("button", { name: "Hand off" }));
  await screen.findByLabelText("Hand-off message");
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(screen.queryByLabelText("Hand-off message")).toBeNull();
  expect(ipc.startHandoff).not.toHaveBeenCalled();
  expect(ipc.sendMessage).not.toHaveBeenCalled();
});

// Fails if the usage panel does not show the backend's rows, or does not refetch each time it opens.
test("usage panel lists the backend rows and refreshes on every open", async () => {
  const { ipc } = fakeIpc();
  ipc.usageSummary.mockResolvedValueOnce([
    { agent: "claude-code", model: "opus", turns: 3, inputTokens: 1200, outputTokens: 340, cacheReadTokens: 50, cacheWriteTokens: 7 },
  ]);
  render(<App store={createAppStore(ipc, { raf: frames().raf })} />);
  fireEvent.click(await screen.findByRole("button", { name: "Usage" }));
  const row = (await screen.findByText("opus")).closest("tr")!;
  expect(within(row).getAllByRole("cell").map((c) => c.textContent)).toEqual(["claude-code", "opus", "3", "1200", "340", "50", "7"]);
  expect(screen.getByText("Computed from this Mac's Hearth data. Never sent anywhere.")).toBeDefined();
  fireEvent.click(screen.getByRole("button", { name: "Close usage" }));
  fireEvent.click(screen.getByRole("button", { name: "Usage" }));
  await vi.waitFor(() => expect(ipc.usageSummary).toHaveBeenCalledTimes(2));
});
