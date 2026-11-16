export interface ResolvedServer {
  command: string;
  source: "setting" | "bundled" | "path";
}

export function binaryName(platform: string): string {
  return platform === "win32" ? "asciidoc-lsp.exe" : "asciidoc-lsp";
}

/** Setting first, then the binary shipped in the VSIX, then `PATH`. */
export function resolveServer(opts: {
  setting: string;
  bundled: string;
  exists: (path: string) => boolean;
}): ResolvedServer {
  const setting = opts.setting.trim();
  if (setting !== "") {
    return { command: setting, source: "setting" };
  }
  if (opts.exists(opts.bundled)) {
    return { command: opts.bundled, source: "bundled" };
  }
  return { command: "asciidoc-lsp", source: "path" };
}
