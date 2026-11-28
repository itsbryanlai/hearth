import { useEffect, useRef, useState } from "react";
import type { AgentStatus, UsageRow } from "../ipc/contract";
import { useApp } from "../state/context";

function availability(status: AgentStatus): string {
  switch (status.kind) {
    case "ready":
      return "";
    case "missing":
      return " (not installed)";
    case "unsupported":
      return ` (unsupported version ${status.version})`;
  }
}

function UsagePanel({ onClose }: { onClose: () => void }) {
  const usageSummary = useApp((s) => s.usageSummary);
  const [rows, setRows] = useState<UsageRow[] | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    let current = true;
    void usageSummary().then((value) => { if (current) setRows(value); }, (e: unknown) => { if (current) setError(String(e)); });
    return () => { current = false; };
  }, [usageSummary]);
  return (
    <section className="usage" aria-label="Usage">
      <p>Computed from this Mac's Hearth data. Never sent anywhere.</p>
      {error ? <p role="alert">{error}</p> : rows === null ? <p>Loading…</p> : rows.length === 0 ? <p>No usage yet.</p> : (
        <table>
          <thead><tr><th>Agent</th><th>Model</th><th>Turns</th><th>Input</th><th>Output</th><th>Cache read</th><th>Cache write</th></tr></thead>
          <tbody>
            {rows.map((r) => (
              <tr key={`${r.agent}/${r.model}`}>
                <td>{r.agent}</td><td>{r.model}</td><td>{r.turns}</td><td>{r.inputTokens}</td><td>{r.outputTokens}</td><td>{r.cacheReadTokens}</td><td>{r.cacheWriteTokens}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <button type="button" className="quiet" onClick={onClose}>Close usage</button>
    </section>
  );
}

export function Sidebar() {
  const projects = useApp((s) => s.projects);
  const agents = useApp((s) => s.agents);
  const ready = agents.find((agent) => agent.status.kind === "ready");
  const [agentProject, setAgentProject] = useState<string | null>(null);
  const [agentId, setAgentId] = useState("");
  const threads = useApp((s) => s.threads);
  const active = useApp((s) => s.activeThreadId);
  const select = useApp((s) => s.selectThread);
  const createThread = useApp((s) => s.createThread);
  const worktreeInfo = useApp((s) => s.worktreeInfo);
  const request = useRef(0);
  const creating = useRef(false);
  const [pending, setPending] = useState(false);
  const [createError, setCreateError] = useState("");
  const [info, setInfo] = useState<{ project: string; isGit: boolean; base: string | null; reason: string | null } | null>(null);
  const [infoError, setInfoError] = useState("");
  const [mode, setMode] = useState<"repo" | "worktree">("repo");
  const [base, setBase] = useState("");
  const [parent, setParent] = useState("");
  const addProject = useApp((s) => s.addProject);
  const [adding, setAdding] = useState(false);
  const [path, setPath] = useState("");
  const [showArchived, setShowArchived] = useState(false);
  const [showUsage, setShowUsage] = useState(false);

  return (
    <nav className="sidebar" aria-label="Projects">
      <button type="button" className="quiet" aria-pressed={showArchived} onClick={() => setShowArchived(!showArchived)}>{showArchived ? "Hide archived" : "Show archived"}</button>
      {projects.map((p) => (
        <section key={p.id} className="project">
          <div className="project-head">
            <span className="project-name" title={p.path}>
              {p.name}
            </span>
            <button
              type="button"
              className="quiet"
              disabled={agents.length === 0 || pending}
              onClick={() => {
                setAgentId(ready?.id ?? "");
                setAgentProject(p.id);
                setInfo(null);
                setParent("");
                setInfoError("");
                setCreateError("");
                const current = ++request.current;
                void worktreeInfo(p.id).then((result) => {
                  if (request.current !== current) return;
                  setInfo({ project: p.id, ...result });
                  setBase(result.base ?? "");
                  setMode(result.base ? "worktree" : "repo");
                }).catch((error: unknown) => { if (request.current === current) setInfoError(String(error)); });
              }}
            >
              + Thread
            </button>
          </div>
          {agentProject === p.id ? (
            <form
              className="new-thread-agent"
              onSubmit={(event) => {
                event.preventDefault();
                const selected = agents.find((agent) => agent.id === agentId);
                if (creating.current || selected?.status.kind !== "ready" || info?.project !== p.id) return;
                creating.current = true;
                setPending(true);
                setCreateError("");
                void createThread(p.id, selected.id, mode === "worktree" ? base : null, mode === "worktree" ? parent.trim() || null : null).then((error) => {
                  creating.current = false;
                  setPending(false);
                  if (error !== null) setCreateError(error);
                  else { request.current++; setAgentProject(null); }
                });
              }}
            >
              <select aria-label="Agent" value={agentId} onChange={(event) => setAgentId(event.target.value)}>
                {!ready ? <option value="">No agent available</option> : null}
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id} disabled={agent.status.kind !== "ready"}>
                    {agent.label}{availability(agent.status)}
                  </option>
                ))}
              </select>
              {info?.project === p.id ? (
                <>
                  <label>Working directory
                    <select aria-label="Working directory" value={mode} onChange={(event) => setMode(event.target.value === "worktree" ? "worktree" : "repo")}>
                      <option value="repo">Work in repo</option>
                      <option value="worktree" disabled={!info.isGit}>Work in new worktree</option>
                    </select>
                  </label>
                  {mode === "worktree" ? <> <label>Base <input aria-label="Base" value={base} onChange={(event) => setBase(event.target.value)} /></label><label>Worktree parent (existing folder) <input aria-label="Worktree parent" placeholder="Default: sibling .hearth-worktrees" value={parent} onChange={(event) => setParent(event.target.value)} /></label></> : null}
                  {info.reason ? <p>{info.reason}</p> : null}
                  {info.isGit && !info.base ? <p>No usable current base commit. Choose a base for a worktree.</p> : null}
                </>
              ) : infoError ? <p role="alert">{infoError}</p> : <p>Checking project…</p>}
              {createError ? <p role="alert">{createError}</p> : null}
              <button type="submit" disabled={pending || !ready || info?.project !== p.id || (mode === "worktree" && !base.trim())}>Create thread</button>
              <button type="button" className="quiet" disabled={pending} onClick={() => { request.current++; setAgentProject(null); }}>Cancel</button>
            </form>
          ) : null}
          <ul>
            {(threads[p.id] ?? []).filter((t) => showArchived || !t.archived).map((t) => (
              <li key={t.id}>
                <button
                  type="button"
                  className={t.id === active ? "thread active" : "thread"}
                  aria-current={t.id === active}
                  onClick={() => void select(t.id)}
                >
                  {t.title}{t.archived ? " (archived)" : ""}{t.branch ? ` · ${t.branch}` : null}
                </button>
              </li>
            ))}
          </ul>
        </section>
      ))}
      {adding ? (
        <form
          className="add-project"
          onSubmit={(e) => {
            e.preventDefault();
            if (path.trim() === "") return;
            void addProject(path.trim());
            setPath("");
            setAdding(false);
          }}
        >
          <input
            aria-label="Project path"
            placeholder="/path/to/repo"
            value={path}
            autoFocus
            onChange={(e) => setPath(e.target.value)}
          />
          <button type="submit" className="primary">
            Add
          </button>
        </form>
      ) : (
        <button type="button" className="quiet add" onClick={() => setAdding(true)}>
          + Project
        </button>
      )}
      {showUsage ? (
        <UsagePanel onClose={() => setShowUsage(false)} />
      ) : (
        <button type="button" className="quiet add" onClick={() => setShowUsage(true)}>
          Usage
        </button>
      )}
    </nav>
  );
}
