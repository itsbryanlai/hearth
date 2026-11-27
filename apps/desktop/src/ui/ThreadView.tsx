import { useApp } from "../state/context";
import { Composer } from "./Composer";
import { TranscriptView } from "./Transcript";

export function ThreadView() {
  const threadId = useApp((s) => s.activeThreadId);
  const thread = useApp((s) =>
    Object.values(s.threads)
      .flat()
      .find((t) => t.id === s.activeThreadId),
  );
  const running = useApp((s) => (s.activeThreadId ? (s.transcripts[s.activeThreadId]?.running ?? false) : false));
  const error = useApp((s) => s.error);

  return (
    <main className="thread-view">
      <header className="thread-header">
        <span className="thread-title">{thread?.title ?? "No thread selected"}</span>
        {thread ? (
          <span className="agent-label">
            {thread.agent} · {thread.model}
          </span>
        ) : null}
        {thread ? <span className="muted">{running ? "running" : "idle"}</span> : null}
      </header>
      {error ? (
        <div className="banner" role="alert">
          {error}
        </div>
      ) : null}
      {threadId ? <TranscriptView key={threadId} threadId={threadId} /> : <div className="transcript empty" />}
      <Composer />
    </main>
  );
}
