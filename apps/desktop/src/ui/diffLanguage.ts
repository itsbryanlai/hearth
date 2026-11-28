export function diffLanguage(path: string): string | undefined {
  const extension = path.split("/").at(-1)?.split(".").at(-1)?.toLowerCase();
  switch (extension) {
    case "js": case "mjs": case "cjs": return "javascript";
    case "ts": case "mts": case "cts": return "typescript";
    case "jsx": case "tsx": case "json": return extension;
    case "rs": return "rust";
    case "py": return "python";
    case "sh": case "bash": case "zsh": return "bash";
    default: return undefined;
  }
}
