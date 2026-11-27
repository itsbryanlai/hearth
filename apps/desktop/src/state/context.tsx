import { createContext, useContext } from "react";
import { useStore } from "zustand";
import type { AppState, AppStore } from "./store";

export const StoreContext = createContext<AppStore | null>(null);

export function useApp<T>(selector: (s: AppState) => T): T {
  const store = useContext(StoreContext);
  if (!store) throw new Error("StoreContext is missing");
  return useStore(store, selector);
}
