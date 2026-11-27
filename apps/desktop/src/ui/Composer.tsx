import { useState } from "react";
import { useApp } from "../state/context";
import { modelPickerBusy } from "../state/store";

export function Composer() {
  const threadId = useApp((s) => s.activeThreadId);
  const running = useApp((s) => (s.activeThreadId ? (s.transcripts[s.activeThreadId]?.running ?? false) : false));
  const changingModel = useApp((s) => threadId !== null && modelPickerBusy(s.modelPickers[threadId]));
  const send = useApp((s) => s.send);
  const cancel = useApp((s) => s.cancel);
  const [text, setText] = useState("");
  const disabled = threadId === null;

  const submit = () => {
    if (text.trim() === "" || running || changingModel) return;
    void send(text);
    setText("");
  };

  return (
    <div className="composer">
      <textarea
        aria-label="Message"
        placeholder={disabled ? "Select or create a thread" : "Message…  (⌘↵ to send, ⌘. to stop)"}
        value={text}
        disabled={disabled}
        rows={3}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          const mod = e.metaKey || e.ctrlKey;
          if (mod && e.key === "Enter") {
            e.preventDefault();
            submit();
          } else if (mod && e.key === ".") {
            e.preventDefault();
            void cancel();
          }
        }}
      />
      <div className="composer-actions">
        {running ? (
          <button type="button" onClick={() => void cancel()}>
            Stop
          </button>
        ) : null}
        <button type="button" className="primary" disabled={disabled || running || changingModel} onClick={submit}>
          Send
        </button>
      </div>
    </div>
  );
}
