import type { ResolvedServer } from "./serverPath";

export interface Status {
  text: string;
  severity: "info" | "error";
}

/** Disposing a client that failed to start or is still starting throws; the caller only wants it gone. */
export async function disposeQuietly(client: { dispose(): Promise<void> } | undefined): Promise<void> {
  try {
    await client?.dispose();
  } catch {
    // The old client is unusable either way.
  }
}

export interface StartFailure extends Status {
  detail: string;
}

export function runningStatus(info: { name: string; version?: string } | undefined): Status {
  const text = info ? `${info.name} ${info.version ?? ""}`.trim() : "asciidoc-lsp";
  return { text, severity: "info" };
}

export function startFailure(server: ResolvedServer): StartFailure {
  const detail =
    server.source === "setting"
      ? `Could not start "${server.command}" from asciidoc-lsp.server.path. Check that the path exists and is executable.`
      : "Set asciidoc-lsp.server.path or put asciidoc-lsp on PATH.";
  return { text: "AsciiDoc LSP: failed to start", severity: "error", detail };
}
