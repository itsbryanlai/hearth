import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  worker: {
    plugins: () => [{
      name: "worker-character-decoder",
      enforce: "pre",
      async resolveId(source, importer) {
        if (source !== "decode-named-character-reference") return;
        // The browser export uses document; the sibling is the package’s worker export.
        const resolved = await this.resolve(source, importer, { skipSelf: true });
        if (!resolved || !resolved.id.endsWith("/index.dom.js")) {
          throw new Error("Unexpected character decoder browser export");
        }
        return resolved.id.slice(0, -"index.dom.js".length) + "index.js";
      },
    }],
  },
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  test: { environment: "jsdom" },
});
