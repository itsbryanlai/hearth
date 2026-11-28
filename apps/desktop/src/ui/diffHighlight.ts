import { refractor } from "refractor/core";
import javascript from "refractor/javascript";
import typescript from "refractor/typescript";
import jsx from "refractor/jsx";
import tsx from "refractor/tsx";
import rust from "refractor/rust";
import python from "refractor/python";
import json from "refractor/json";
import bash from "refractor/bash";
type Node = ReturnType<typeof refractor.highlight>["children"][number];
type Element = Extract<Node, { type: "element" }>;
type Text = Extract<Node, { type: "text" }>;
import { diffLanguage } from "./diffLanguage";
import type { FileDiff } from "../ipc/contract";

for (const grammar of [javascript, typescript, jsx, tsx, rust, python, json, bash]) refractor.register(grammar);

export type TokenKind = "plain" | "comment" | "string" | "keyword" | "number" | "function";
export type Span = { text: string; kind: TokenKind };
export type HighlightedHunk = { old: Span[][]; new: Span[][] };


function category(node: Element, parent: TokenKind): TokenKind {
  const classes = node.properties.className;
  if (!Array.isArray(classes)) return parent;
  if (classes.includes("comment")) return "comment";
  if (classes.includes("string") || classes.includes("char")) return "string";
  if (classes.includes("keyword") || classes.includes("boolean")) return "keyword";
  if (classes.includes("number")) return "number";
  if (classes.includes("function") || classes.includes("class-name")) return "function";
  return parent;
}

function tokenize(lines: string[], syntax: string): Span[][] {
  const result: Span[][] = lines.map(() => []);
  let line = 0;
  function visit(node: Element | Text, kind: TokenKind) {
    if (node.type === "element") {
      const next = category(node, kind);
      for (const child of node.children) if (child.type === "element" || child.type === "text") visit(child, next);
    } else {
      const parts = node.value.split("\n");
      parts.forEach((part, index) => {
        if (index > 0) line++;
        if (part) result[line]?.push({ text: part, kind });
      });
    }
  }
  for (const node of refractor.highlight(lines.join("\n"), syntax).children) {
    if (node.type === "element" || node.type === "text") visit(node, "plain");
  }
  return result;
}

export function highlightDiff(path: string, diff: FileDiff): HighlightedHunk[] | null {
  const syntax = diffLanguage(path);
  if (!syntax || diff.binary) return null;
  const selectedSyntax = syntax;
  return diff.hunks.map((hunk) => {
    function side(excluded: "add" | "delete"): Span[][] {
      const indices = hunk.rows.flatMap((row, index) => row.kind !== excluded && row.kind !== "notice" ? [index] : []);
      const tokens = tokenize(indices.map((index) => hunk.rows[index]?.text ?? ""), selectedSyntax);
      const result: Span[][] = hunk.rows.map(() => []);
      indices.forEach((index, position) => { result[index] = tokens[position] ?? []; });
      return result;
    }
    return { old: side("add"), new: side("delete") };
  });
}
