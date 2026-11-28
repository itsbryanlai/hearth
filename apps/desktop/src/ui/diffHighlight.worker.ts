import { highlightDiff } from "./diffHighlight";
import type { FileDiff } from "../ipc/contract";

self.onmessage = (event: MessageEvent<{ path: string; diff: FileDiff }>) => {
  try {
    self.postMessage(highlightDiff(event.data.path, event.data.diff));
  } catch {
    self.postMessage(null);
  }
};
