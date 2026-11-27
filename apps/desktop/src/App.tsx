import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

export function App() {
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    invoke<string>("app_version").then((v) => {
      if (live) setVersion(v);
    });
    return () => {
      live = false;
    };
  }, []);

  return (
    <main>
      <h1>Hearth</h1>
      <p>{version === null ? "" : `v${version}`}</p>
    </main>
  );
}
