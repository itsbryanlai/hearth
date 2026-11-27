import { useState } from "react";
import { useApp } from "../state/context";

export function Sidebar() {
  const projects = useApp((s) => s.projects);
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
            <button type="button" className="quiet" onClick={() => void createThread(p.id)}>
              + Thread
            </button>
          </div>
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
