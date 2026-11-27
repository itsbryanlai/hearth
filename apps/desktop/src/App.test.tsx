import type { AgentEvent, ApprovalDecision } from "@hearth/protocol";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { App } from "./App";
import type { Ipc, ThreadEventPayload, ThreadSnapshot } from "./ipc/contract";
import { createMockIpc } from "./ipc/mock";
import { createAppStore, nextFrame } from "./state/store";

const THREAD = { id: "th", projectId: "p", title: "Thread one", agent: "claude-code", model: "opus" };

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

function fakeIpc(snapshot: ThreadSnapshot = { thread: THREAD, turns: [], lastSeq: 0 }) {
  let emit: (p: ThreadEventPayload) => void = () => {};
  const ipc = {
    listProjects: vi.fn(async () => [{ id: "p", name: "proj", path: "/work/proj" }]),
    addProject: vi.fn(),
    listThreads: vi.fn(async () => [THREAD]),
    createThread: vi.fn(),
    loadThread: vi.fn(async () => snapshot),
    sendMessage: vi.fn(async () => THREAD),
    answerApproval: vi.fn(async () => {}),
    cancelTurn: vi.fn(async () => {}),
    onThreadEvent: (h: (p: ThreadEventPayload) => void) => {
      emit = h;
      return () => {};
    },
  } satisfies Record<keyof Ipc, unknown>;
  const send = (event: AgentEvent, seq = 1) =>
    act(() => emit({ threadId: "th", turnId: "t1", seq, event }));
  return { ipc: ipc as Ipc & typeof ipc, send };
}

async function openThread(ipc: Ipc, f = frames()) {
  const store = createAppStore(ipc, { raf: f.raf });
  render(<App store={store} />);
  fireEvent.click(await screen.findByRole("button", { name: "Thread one" }));
  await screen.findByLabelText("Message");
  await vi.waitFor(() => expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(false));
  return { store, f };
}

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
  fireEvent.click(await screen.findByRole("button", { name: label }));
  expect(ipc.answerApproval).toHaveBeenCalledWith("th", "a9", decision);
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
