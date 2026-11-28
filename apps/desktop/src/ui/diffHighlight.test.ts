import { describe, expect, it } from "vitest";
import { highlightDiff } from "./diffHighlight";
import type { FileDiff } from "../ipc/contract";

const diff: FileDiff = {
  binary: false,
  hunks: [{ header: "@@", splitRows: [], rows: [
    { kind: "context", text: "/* comment", oldLine: 1, newLine: 1 },
    { kind: "delete", text: "old */ const n = 1;", oldLine: 2, newLine: null },
    { kind: "add", text: "new */ const n = 2;", oldLine: null, newLine: 2 },
  ] }],
};

describe("diff syntax highlighting", () => {
  it("fails if highlighted hunk sides change source text or lose multiline syntax", () => {
    const result = highlightDiff("source.ts", diff);
    expect(result?.[0]?.old[1]?.map((span) => span.text).join("")).toBe("old */ const n = 1;");
    expect(result?.[0]?.new[2]?.map((span) => span.text).join("")).toBe("new */ const n = 2;");
    expect(result?.[0]?.old[1]?.[0]).toEqual({ text: "old */", kind: "comment" });
    expect(result?.[0]?.new[2]?.some((span) => span.kind === "keyword" && span.text === "const")).toBe(true);
  });

  it("fails if unknown or binary files receive syntax highlighting", () => {
    for (const [path, value] of [["file.unknown", diff], ["file.ts", { ...diff, binary: true }]] satisfies [string, FileDiff][]) {
      expect(highlightDiff(path, value)).toBeNull();
    }
    expect(highlightDiff("file.ts", diff)).not.toBeNull();
  });
});
