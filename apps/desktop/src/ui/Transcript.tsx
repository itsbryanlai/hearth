import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useRef } from "react";
import { useApp } from "../state/context";
import { emptyTranscript } from "../state/transcript";
import { BlockView } from "./Blocks";

export function TranscriptView({ threadId, remembersApprovals }: {
  threadId: string;
  remembersApprovals: boolean;
}) {
  const transcript = useApp((s) => s.transcripts[threadId] ?? emptyTranscript);
  const answer = useApp((s) => s.answerApproval);
  const { blocks } = transcript;
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  // The compiler skips this component; the virtualizer returns unstable functions by design.
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: blocks.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 64,
    overscan: 6,
    getItemKey: (i) => blocks[i]?.id ?? i,
  });

  const last = blocks[blocks.length - 1];
  const lastLength = last && "text" in last ? last.text.length : 0;
  useEffect(() => {
    if (stickToBottom.current && blocks.length > 0) {
      virtualizer.scrollToIndex(blocks.length - 1, { align: "end" });
    }
  }, [blocks.length, lastLength, virtualizer]);

  return (
    <div
      ref={scrollRef}
      className="transcript"
      data-testid="transcript"
      onScroll={(e) => {
        const el = e.currentTarget;
        stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      }}
    >
      <div className="transcript-inner" style={{ height: virtualizer.getTotalSize() }}>
        {virtualizer.getVirtualItems().map((row) => {
          const block = blocks[row.index];
          if (!block) return null;
          return (
            <div
              key={row.key}
              ref={virtualizer.measureElement}
              data-index={row.index}
              className="row"
              style={{ transform: `translateY(${row.start}px)` }}
            >
              <BlockView block={block} remembersApprovals={remembersApprovals} onAnswer={(id, d) => void answer(id, d)} />
            </div>
          );
        })}
      </div>
    </div>
  );
}
