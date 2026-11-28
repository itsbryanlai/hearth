import { useEffect, useMemo, useState } from "react";
import { Changes } from "./Changes";
import type { InstalledAgent, ThreadSummary } from "../ipc/contract";
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

function HandoffPanel({ thread, targets, onClose }: { thread: ThreadSummary; targets: InstalledAgent[]; onClose: () => void }) {
  const draftHandoff = useApp((s) => s.draftHandoff);
  const startHandoff = useApp((s) => s.startHandoff);
  const [target, setTarget] = useState(targets[0]?.id ?? "");
  const [drafts, setDrafts] = useState<Record<string, { text: string; error: string | null }>>({});
  const [starting, setStarting] = useState(false);
  const draft = drafts[target];
  const ready = draft !== undefined && draft.error === null;
  const text = draft?.text ?? "";

  useEffect(() => {
    if (!target) return;
    let current = true;
    draftHandoff(thread.id, target).then(
      (result) => current && setDrafts((d) => ({ ...d, [target]: { text: result.text, error: null } })),
      (e) => current && setDrafts((d) => ({ ...d, [target]: { text: "", error: e instanceof Error ? e.message : String(e) } })),
    );
    return () => {
      current = false;
    };
  }, [draftHandoff, thread.id, target]);

  const start = async () => {
    setStarting(true);
    const started = await startHandoff(target, text);
    setStarting(false);
    if (started) onClose();
  };

  return (
    <section className="handoff-panel" aria-label="Hand off">
      <label>
        Hand off to
        <select aria-label="Target agent" value={target} onChange={(event) => setTarget(event.target.value)}>
          {targets.map((agent) => <option key={agent.id} value={agent.id}>{agent.label}</option>)}
        </select>
      </label>
      <textarea
        aria-label="Hand-off message"
        value={text}
        disabled={!ready || starting}
        onChange={(event) => setDrafts((d) => ({ ...d, [target]: { text: event.target.value, error: null } }))}
      />
      {draft?.error ? <span role="alert">{draft.error}</span> : null}
      <button type="button" disabled={!ready || starting || text.trim() === ""} onClick={() => void start()}>Start</button>
      <button type="button" onClick={onClose}>Cancel</button>
    </section>
  );
}

export function ThreadView() {
  const [changesOpen, setChangesOpen] = useState(false);
  const [handoffOpen, setHandoffOpen] = useState(false);
  const [cleanup, setCleanup] = useState<{ threadId: string; path: string; action: "remove" | "branch" | null } | null>(null);
  const [cleanupError, setCleanupError] = useState<string | null>(null);
  const [cleanupMessage, setCleanupMessage] = useState<string | null>(null);
  const [cleanupBusy, setCleanupBusy] = useState(false);
  const threadId = useApp((s) => s.activeThreadId);
  const thread = useApp((s) =>
    Object.values(s.threads)
      .flat()
      .find((t) => t.id === s.activeThreadId),
  );
  const running = useApp((s) => (s.activeThreadId ? (s.transcripts[s.activeThreadId]?.running ?? false) : false));
  const error = useApp((s) => s.error);
  const archivePending = useApp((s) => threadId !== null && !!s.archivePending[threadId]);
  const setArchived = useApp((s) => s.setArchived);
  const agents = useApp((s) => s.agents);
  const targets = useMemo(
    () => agents.filter((agent) => agent.status.kind === "ready" && agent.id !== thread?.agent),
    [agents, thread?.agent],
  );
  const cleanupPath = useApp((s) => s.cleanupPath);
  const removeThreadWorktree = useApp((s) => s.removeThreadWorktree);
  const deleteThreadBranch = useApp((s) => s.deleteThreadBranch);
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
        {thread ? <ModelPicker thread={thread} running={running || thread.archived || archivePending} /> : null}
        {thread ? <button onClick={() => setChangesOpen((open) => !open)}>Changes</button> : null}
        {thread ? (
          <button disabled={running || thread.archived || archivePending || targets.length === 0} onClick={() => setHandoffOpen((open) => !open)}>Hand off</button>
        ) : null}
        {thread ? <button type="button" disabled={running || archivePending} onClick={() => void setArchived(thread.id, !thread.archived)}>{thread.archived ? "Restore" : "Archive"}</button> : null}
        {thread?.archived ? <button type="button" onClick={() => {
          setCleanupError(null);
          setCleanupMessage(null);
          void cleanupPath(thread.id).then((path) => path ? setCleanup({ threadId: thread.id, path, action: null }) : setCleanupError("This thread has no worktree."), (error: unknown) => setCleanupError(String(error)));
        }}>Worktree cleanup</button> : null}
        {thread ? <span className="muted">{thread.archived ? "archived" : running ? "running" : "idle"}</span> : null}
      </header>
      {cleanup && cleanup.threadId === threadId ? <section aria-label="Worktree cleanup">
        <p>Worktree: {cleanup.path}</p>
        {cleanup.action === null ? <><button type="button" onClick={() => setCleanup({ ...cleanup, action: "remove" })}>Remove worktree</button><button type="button" onClick={() => setCleanup({ ...cleanup, action: "branch" })}>Delete branch</button></> : <>
          <p>{cleanup.action === "remove" ? `Remove the clean worktree at ${cleanup.path}?` : `Delete branch ${thread?.branch ?? ""}?`}</p>
          <button type="button" disabled={cleanupBusy} onClick={() => {
            setCleanupBusy(true); setCleanupError(null); setCleanupMessage(null);
            const operation = cleanup.action === "remove" ? removeThreadWorktree(cleanup.threadId) : deleteThreadBranch(cleanup.threadId);
            void operation.then(() => { setCleanupMessage(cleanup.action === "remove" ? "Worktree removed. The branch was kept." : "Branch deleted."); setCleanup({ ...cleanup, action: null }); }, (error: unknown) => setCleanupError(String(error))).finally(() => setCleanupBusy(false));
          }}>Confirm {cleanup.action === "remove" ? "removal" : "branch deletion"}</button>
          <button type="button" disabled={cleanupBusy} onClick={() => setCleanup({ ...cleanup, action: null })}>Cancel</button>
        </>}
      </section> : null}
      {cleanupError ? <div role="alert">{cleanupError}</div> : null}
      {cleanupMessage ? <div role="status">{cleanupMessage}</div> : null}
      {error ? (
        <div className="banner" role="alert">
          {error}
        </div>
      ) : null}
      {thread && handoffOpen && !thread.archived && targets.length > 0 ? (
        <HandoffPanel key={thread.id + thread.agent} thread={thread} targets={targets} onClose={() => setHandoffOpen(false)} />
      ) : null}
      <div className="thread-content">
        {threadId ? <TranscriptView key={threadId} threadId={threadId} remembersApprovals={remembersApprovals} /> : <div className="transcript empty" />}
        {threadId && changesOpen ? <Changes key={threadId} threadId={threadId} worktree={thread?.branch != null} onClose={() => setChangesOpen(false)} /> : null}
      </div>
      <Composer />
    </main>
  );
}
