import type { ThreadSummary } from "../ipc/contract";
import { useApp } from "../state/context";
import { Composer } from "./Composer";
import { TranscriptView } from "./Transcript";

function ModelPicker({ thread, running }: { thread: ThreadSummary; running: boolean }) {
  const picker = useApp((s) => s.modelPickers[thread.id]);
  const loadModels = useApp((s) => s.loadModels);
  const setModel = useApp((s) => s.setModel);

  if (picker?.kind === "ready" || picker?.kind === "switching") {
    const { choices, selected: saved, current } = picker.models;
    const actual = thread.model || current || "";
    const selected = saved
      ?? choices.find((choice) => choice.id === actual)?.id
      ?? choices.find((choice) => choice.resolvedId === actual)?.id
      ?? actual;
    const unavailable = selected !== "" && !choices.some((choice) => choice.id === selected);
    return (
      <select
        aria-label="Model"
        disabled={running || picker.kind === "switching" || choices.length === 0}
        value={selected}
        onChange={(event) => void setModel(thread.id, event.target.value)}
      >
        {selected === "" ? <option value="" disabled>{choices.length ? "Choose model" : "No models available"}</option> : null}
        {unavailable ? <option value={selected} disabled>Unavailable: {selected}</option> : null}
        {choices.map((choice) => <option key={choice.id} value={choice.id}>{choice.label}</option>)}
      </select>
    );
  }

  return (
    <span className="model-picker">
      <button
        type="button"
        className="quiet"
        disabled={running || picker?.kind === "loading"}
        onClick={() => void loadModels(thread.id)}
      >
        {picker?.kind === "loading" ? "Loading models…" : "Choose model"}
      </button>
      {picker?.kind === "failed" ? <span role="alert">{picker.message}</span> : null}
    </span>
  );
}

export function ThreadView() {
  const threadId = useApp((s) => s.activeThreadId);
  const thread = useApp((s) =>
    Object.values(s.threads)
      .flat()
      .find((t) => t.id === s.activeThreadId),
  );
  const running = useApp((s) => (s.activeThreadId ? (s.transcripts[s.activeThreadId]?.running ?? false) : false));
  const error = useApp((s) => s.error);
  const remembersApprovals = useApp((s) => s.agents.some((agent) =>
    agent.id === thread?.agent && agent.remembersApprovals,
  ));

  return (
    <main className="thread-view">
      <header className="thread-header">
        <span className="thread-title">{thread?.title ?? "No thread selected"}</span>
        {thread ? (
          <span className="agent-label">
            {thread.agent} · {thread.model}
          </span>
        ) : null}
        {thread ? <ModelPicker thread={thread} running={running} /> : null}
        {thread ? <span className="muted">{running ? "running" : "idle"}</span> : null}
      </header>
      {error ? (
        <div className="banner" role="alert">
          {error}
        </div>
      ) : null}
      {threadId ? <TranscriptView key={threadId} threadId={threadId} remembersApprovals={remembersApprovals} /> : <div className="transcript empty" />}
      <Composer />
    </main>
  );
}
