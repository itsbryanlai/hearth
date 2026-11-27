import type { Ipc } from "./contract";
import { createMockIpc } from "./mock";
import { createTauriIpc } from "./tauri";

/** The Tauri backend inside the app, the in-memory mock in a plain browser. */
export function createDefaultIpc(): Ipc {
  return "__TAURI_INTERNALS__" in window ? createTauriIpc() : createMockIpc();
}
