import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { createDefaultIpc } from "./ipc";
import { createAppStore } from "./state/store";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("missing #root");
const store = createAppStore(createDefaultIpc());
createRoot(root).render(
  <StrictMode>
    <App store={store} />
  </StrictMode>,
);
