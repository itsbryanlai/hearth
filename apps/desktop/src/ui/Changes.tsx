import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useMemo, useRef, useState } from "react";
import type { DiffFile, DiffRow, FileDiff, ReviewTurn } from "../ipc/contract";
import { useApp } from "../state/context";

type Load<T> = { kind: "loading" } | { kind: "error" } | { kind: "ready"; value: T };

export function Changes({ threadId, onClose }: { threadId: string; onClose: () => void }) {
  const reviewTurns = useApp((s) => s.reviewTurns);
  const [turns, setTurns] = useState<Load<ReviewTurn[]>>({ kind: "loading" });
  const [turnId, setTurnId] = useState("");
  useEffect(() => {
    let live = true;
    void reviewTurns(threadId).then((value) => {
      if (live) {
        setTurns({ kind: "ready", value });
        setTurnId([...value].reverse().find((turn) => turn.available)?.turnId ?? value.at(-1)?.turnId ?? "");
      }
    }, () => { if (live) setTurns({ kind: "error" }); });
    return () => { live = false; };
  }, [threadId, reviewTurns]);

  let content;
  if (turns.kind === "loading") content = <p>Loading turns…</p>;
  else if (turns.kind === "error") content = <p role="alert">Could not load turns.</p>;
  else if (turns.value.length === 0) content = <p>No turns to review.</p>;
  else content = (
    <>
      <label>
        Turn
        <select aria-label="Turn" value={turnId} onChange={(e) => setTurnId(e.target.value)}>
          {turns.value.map((turn) => <option key={turn.turnId} value={turn.turnId}>{turn.label}</option>)}
        </select>
      </label>
      {turnId ? <TurnChanges key={turnId} threadId={threadId} turnId={turnId} /> : null}
    </>
  );
  return (
    <section className="changes" aria-label="Changes">
      <header className="changes-header">
        <strong>Changes</strong>
        <button onClick={onClose}>Close changes</button>
      </header>
      {content}
    </section>
  );
}

function TurnChanges({ threadId, turnId }: { threadId: string; turnId: string }) {
  const load = useApp((s) => s.turnDiffFiles);
  const [files, setFiles] = useState<Load<DiffFile[] | null>>({ kind: "loading" });
  const [fileId, setFileId] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const list = files.kind === "ready" ? files.value ?? [] : [];
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: list.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 34,
    overscan: 6,
  });
  useEffect(() => {
    let live = true;
    void load(threadId, turnId).then((value) => {
      if (live) {
        setFiles({ kind: "ready", value });
        setFileId(value?.[0]?.id ?? "");
      }
    }, () => { if (live) setFiles({ kind: "error" }); });
    return () => { live = false; };
  }, [threadId, turnId, load]);
  if (files.kind === "loading") return <p>Loading files…</p>;
  if (files.kind === "error") return <p role="alert">Could not load changes.</p>;
  if (files.value === null) return <p>Snapshot unavailable for this turn.</p>;
  if (files.value.length === 0) return <p>No files changed in this turn.</p>;
  return (
    <div className="changes-body">
      <nav className="changes-files" aria-label="Changed files" ref={scrollRef}>
        <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
          {virtualizer.getVirtualItems().map((row) => {
            const file = list[row.index];
            if (!file) return null;
            return (
              <button
                key={file.id}
                className="changes-file"
                aria-current={file.id === fileId ? "true" : undefined}
                style={{ transform: `translateY(${row.start}px)` }}
                onClick={() => setFileId(file.id)}
              >
                {file.status} {file.oldPath ? `${file.oldPath} → ` : ""}{file.path}
              </button>
            );
          })}
        </div>
      </nav>
      {fileId ? <FileChanges key={fileId} threadId={threadId} turnId={turnId} fileId={fileId} /> : null}
    </div>
  );
}

type DisplayRow =
  | { kind: "header"; text: string }
  | { kind: "unified"; row: DiffRow }
  | { kind: "split"; left: DiffRow | undefined; right: DiffRow | undefined; notice: DiffRow | undefined };

function RowContent({ row }: { row: DisplayRow }) {
  if (row.kind === "header") return row.text;
  if (row.kind === "split") {
    if (row.notice) return <span className="diff-notice">{row.notice.text}</span>;
    return (
      <>
        <span className={`diff-cell ${row.left?.kind ?? ""}`}>
          {row.left ? `${row.left.oldLine ?? ""} ${row.left.kind === "delete" ? "−" : " "} ${row.left.text}` : ""}
        </span>
        <span className={`diff-cell ${row.right?.kind ?? ""}`}>
          {row.right ? `${row.right.newLine ?? ""} ${row.right.kind === "add" ? "+" : " "} ${row.right.text}` : ""}
        </span>
      </>
    );
  }
  let marker = " ";
  if (row.row.kind === "add") marker = "+";
  else if (row.row.kind === "delete") marker = "−";
  return <span>{row.row.oldLine ?? ""} {row.row.newLine ?? ""} {marker} {row.row.text}</span>;
}

function FileChanges({ threadId, turnId, fileId }: { threadId: string; turnId: string; fileId: string }) {
  const load = useApp((s) => s.turnDiffFile);
  const [diff, setDiff] = useState<Load<FileDiff | null>>({ kind: "loading" });
  const [split, setSplit] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const rows = useMemo<DisplayRow[]>(() => {
    if (diff.kind !== "ready" || !diff.value) return [];
    return diff.value.hunks.flatMap((h): DisplayRow[] => {
      const header: DisplayRow = { kind: "header", text: h.header };
      if (!split) return [header, ...h.rows.map((row): DisplayRow => ({ kind: "unified", row }))];
      return [header, ...h.splitRows.map((pair): DisplayRow => ({
        kind: "split",
        left: pair.left === null ? undefined : h.rows[pair.left],
        right: pair.right === null ? undefined : h.rows[pair.right],
        notice: pair.notice === null ? undefined : h.rows[pair.notice],
      }))];
    });
  }, [diff, split]);
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 24,
    overscan: 10,
  });
  useEffect(() => {
    let live = true;
    void load(threadId, turnId, fileId).then(
      (value) => { if (live) setDiff({ kind: "ready", value }); },
      () => { if (live) setDiff({ kind: "error" }); },
    );
    return () => { live = false; };
  }, [threadId, turnId, fileId, load]);

  let content;
  if (diff.kind === "loading") content = <p>Loading diff…</p>;
  else if (diff.kind === "error") content = <p role="alert">Could not load diff.</p>;
  else if (diff.value === null) content = <p>Snapshot unavailable for this turn.</p>;
  else if (diff.value.binary) content = <p>Binary file; diff unavailable.</p>;
  else if (rows.length === 0) content = <p>No textual changes.</p>;
  else content = (
    <div className="diff-scroll" ref={scrollRef} tabIndex={0} aria-label="File diff">
      <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
        {virtualizer.getVirtualItems().map((item) => {
          const row = rows[item.index];
          if (!row) return null;
          const rowClass = row.kind === "unified" ? row.row.kind : row.kind === "split" ? row.left?.kind ?? row.right?.kind ?? "notice" : "header";
          return (
            <div key={item.key} className={`diff-row ${rowClass} ${row.kind}`} style={{ transform: `translateY(${item.start}px)` }}>
              <RowContent row={row} />
            </div>
          );
        })}
      </div>
    </div>
  );
  return (
    <div className="changes-diff">
      <div className="changes-view">
        <button aria-pressed={!split} onClick={() => setSplit(false)}>Unified</button>
        <button aria-pressed={split} onClick={() => setSplit(true)}>Side by side</button>
      </div>
      {content}
    </div>
  );
}
