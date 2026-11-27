import { useState } from "react";
import type { AgentStatus } from "../ipc/contract";
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
  const addProject = useApp((s) => s.addProject);
  const [adding, setAdding] = useState(false);
  const [path, setPath] = useState("");

  return (
    <nav className="sidebar" aria-label="Projects">
      {projects.map((p) => (
        <section key={p.id} className="project">
          <div className="project-head">
            <span className="project-name" title={p.path}>
              {p.name}
            </span>
            <button
              type="button"
              className="quiet"
              disabled={agents.length === 0}
              onClick={() => {
                if (agents.length === 1 && ready) {
                  void createThread(p.id, ready.id);
                } else {
                  setAgentId(ready?.id ?? "");
                  setAgentProject(p.id);
                }
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
                if (selected?.status.kind !== "ready") return;
                setAgentProject(null);
                void createThread(p.id, selected.id);
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
              <button type="submit" disabled={!ready}>Create thread</button>
              <button type="button" className="quiet" onClick={() => setAgentProject(null)}>Cancel</button>
            </form>
          ) : null}
          <ul>
            {(threads[p.id] ?? []).map((t) => (
              <li key={t.id}>
                <button
                  type="button"
                  className={t.id === active ? "thread active" : "thread"}
                  aria-current={t.id === active}
                  onClick={() => void select(t.id)}
                >
                  {t.title}
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
    </nav>
  );
}
