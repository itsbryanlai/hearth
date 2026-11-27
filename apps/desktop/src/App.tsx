import { useEffect } from "react";
import { StoreContext } from "./state/context";
import type { AppStore } from "./state/store";
import { Sidebar } from "./ui/Sidebar";
import { ThreadView } from "./ui/ThreadView";

export function App({ store }: { store: AppStore }) {
  useEffect(() => {
    void store.getState().init();
    return () => store.getState().dispose();
  }, [store]);

  return (
    <StoreContext.Provider value={store}>
      <div className="app">
        <Sidebar />
        <ThreadView />
      </div>
    </StoreContext.Provider>
  );
}
