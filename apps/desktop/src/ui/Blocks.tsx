import type { ApprovalDecision } from "@hearth/protocol";
import { useState } from "react";
import type { Block } from "../state/transcript";

function Collapsible({
  summary,
  children,
  className,
}: {
  summary: string;
  children: React.ReactNode;
  className: string;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className={`block ${className}`}>
      <button
        type="button"
        className="disclosure"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <span aria-hidden="true">{open ? "▾" : "▸"}</span> {summary}
      </button>
      {open ? children : null}
    </div>
  );
}

function ApprovalCard({
  block,
  remembersApprovals,
  onAnswer,
}: {
  block: Extract<Block, { kind: "approval" }>;
  remembersApprovals: boolean;
  onAnswer: (approvalId: string, decision: ApprovalDecision) => void;
}) {
  const { request, answer } = block;
  const choices: [string, ApprovalDecision][] = [
    ["Allow once", "allowOnce"],
    ["Allow for this thread", "allowForThread"],
    ["Deny", "deny"],
  ];
  return (
    <div className="block approval" role="group" aria-label={`Approval: ${request.title}`}>
      <div className="approval-head">
        <strong>{request.tool}</strong> {request.title}
      </div>
      <pre className="mono">{request.detail}</pre>
      {answer === undefined ? (
        <div className="approval-actions">
          {choices.filter(([, decision]) => remembersApprovals || decision !== "allowForThread").map(([label, decision]) => (
            <button
              key={decision}
              type="button"
              className={decision === "deny" ? "danger" : decision === "allowOnce" ? "primary" : ""}
              onClick={() => onAnswer(request.id, decision)}
            >
              {label}
            </button>
          ))}
        </div>
      ) : (
        <div className="muted">Answered: {choices.find(([, d]) => d === answer)?.[0]}</div>
      )}
    </div>
  );
}

export function BlockView({
  block,
  remembersApprovals,
  onAnswer,
}: {
  block: Block;
  remembersApprovals: boolean;
  onAnswer: (approvalId: string, decision: ApprovalDecision) => void;
}) {
  switch (block.kind) {
    case "user":
      return <div className="block user">{block.text}</div>;
    case "text":
      return <div className="block text">{block.text}</div>;
    case "reasoning":
      return (
        <Collapsible summary="Reasoning" className="reasoning">
          <div className="reasoning-body">{block.text}</div>
        </Collapsible>
      );
    case "tool":
      return (
        <Collapsible
          summary={`${block.title} · ${block.status}`}
          className={`tool status-${block.status}`}
        >
          <pre className="mono">{block.output === "" ? "(no output)" : block.output}</pre>
        </Collapsible>
      );
    case "fileEdit":
      return (
        <div className="block file-edit">
          <span className="chip">{block.editKind}</span> <span className="mono">{block.path}</span>
        </div>
      );
    case "command":
      return (
        <div className="block command">
          <span className="mono">$ {block.command}</span>
          {block.exitCode !== undefined ? <span className="muted"> exit {block.exitCode}</span> : null}
        </div>
      );
    case "approval":
      return <ApprovalCard block={block} remembersApprovals={remembersApprovals} onAnswer={onAnswer} />;
    case "error":
      return (
        <div className="block error" role="alert">
          {block.message}
        </div>
      );
  }
}
