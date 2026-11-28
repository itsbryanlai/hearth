import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  THREAD_EVENT_CHANNEL,
  type Ipc,
  type ThreadEventPayload,
} from "./contract";

export function createTauriIpc(): Ipc {
  return {
    listAgents: () => invoke("list_agents"),
    listProjects: () => invoke("list_projects"),
    addProject: (path) => invoke("add_project", { path }),
    listThreads: (projectId) => invoke("list_threads", { projectId }),
    worktreeInfo: (projectId) => invoke("worktree_info", { projectId }),
    createThread: (projectId, agent, base, parent) =>
      invoke("create_thread", { projectId, agent, base, parent }),
    loadThread: (threadId) => invoke("load_thread", { threadId }),
    reviewTurns: (threadId) => invoke("review_turns", { threadId }),
    startWorktreeReview: (threadId) => invoke("start_worktree_review", { threadId }),
    worktreeDiffFiles: (threadId, reviewId) => invoke("worktree_diff_files", { threadId, reviewId }),
    worktreeDiffFile: (threadId, reviewId, fileId) => invoke("worktree_diff_file", { threadId, reviewId, fileId }),
    releaseWorktreeReview: (threadId, reviewId) => invoke("release_worktree_review", { threadId, reviewId }),
    turnDiffFiles: (threadId, turnId) => invoke("turn_diff_files", { threadId, turnId }),
    turnDiffFile: (threadId, turnId, fileId) => invoke("turn_diff_file", { threadId, turnId, fileId }),
    archiveThread: (threadId) => invoke("archive_thread", { threadId }),
    restoreThread: (threadId) => invoke("restore_thread", { threadId }),
    cleanupPath: (threadId) => invoke("cleanup_path", { threadId }),
    removeThreadWorktree: (threadId) => invoke("remove_thread_worktree", { threadId }),
    deleteThreadBranch: (threadId) => invoke("delete_thread_branch", { threadId }),
    listThreadModels: (threadId) => invoke("list_thread_models", { threadId }),
    setThreadModel: (threadId, modelId) => invoke("set_thread_model", { threadId, modelId }),
    draftHandoff: (threadId, target) => invoke("draft_handoff", { threadId, target }),
    startHandoff: (threadId, target, text) => invoke("start_handoff", { threadId, target, text }),
    sendMessage: (threadId, input) =>
      invoke("send_message", { threadId, input }),
    answerApproval: (threadId, approvalId, decision) =>
      invoke("answer_approval", { threadId, approvalId, decision }),
    cancelTurn: (threadId) => invoke("cancel_turn", { threadId }),
    onThreadEvent(handler) {
      let live = true;
      let unlisten: (() => void) | undefined;
      void listen<ThreadEventPayload>(THREAD_EVENT_CHANNEL, (e) =>
        handler(e.payload),
      ).then((fn) => {
        if (live) unlisten = fn;
        else fn();
      });
      return () => {
        live = false;
        unlisten?.();
      };
    },
  };
}
