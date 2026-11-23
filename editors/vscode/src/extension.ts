import { existsSync } from "node:fs";
import * as vscode from "vscode";
import { LanguageClient, State } from "vscode-languageclient/node";
import { disposeQuietly, runningStatus, startFailure } from "./lifecycle";
import { Preview } from "./preview";
import { binaryName, resolveServer, type ResolvedServer } from "./serverPath";

let client: LanguageClient | undefined;

function createClient(
  context: vscode.ExtensionContext,
  preview: Preview,
): { client: LanguageClient; server: ResolvedServer } {
  const server = resolveServer({
    setting: vscode.workspace.getConfiguration("asciidoc-lsp").get<string>("server.path", ""),
    bundled: context.asAbsolutePath(`server/${binaryName(process.platform)}`),
    exists: existsSync,
  });
  const client = new LanguageClient(
    "asciidoc-lsp",
    "AsciiDoc LSP",
    { command: server.command },
    { documentSelector: [{ language: "asciidoc", scheme: "file" }], middleware: preview.middleware },
  );
  return { client, server };
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const status = vscode.languages.createLanguageStatusItem("asciidoc-lsp.version", {
    language: "asciidoc",
  });
  status.name = "AsciiDoc LSP";
  context.subscriptions.push(status);
  const preview = new Preview();
  context.subscriptions.push(preview);

  const start = async (): Promise<void> => {
    const { client: current, server } = createClient(context, preview);
    client = current;
    preview.setClient(current);
    status.detail = undefined;
    status.text = "AsciiDoc LSP: starting";
    status.severity = vscode.LanguageStatusSeverity.Information;
    current.onDidChangeState(({ newState }) => {
      if (newState === State.Stopped) {
        status.text = "AsciiDoc LSP: stopped";
        status.severity = vscode.LanguageStatusSeverity.Error;
      } else if (newState === State.Running) {
        const running = runningStatus(current.initializeResult?.serverInfo);
        status.text = running.text;
        status.severity = vscode.LanguageStatusSeverity.Information;
      }
    });
    try {
      await current.start();
    } catch {
      // The library already shows its own error notification.
      const failure = startFailure(server);
      status.text = failure.text;
      status.severity = vscode.LanguageStatusSeverity.Error;
      status.detail = failure.detail;
    }
  };

  context.subscriptions.push(
    vscode.commands.registerCommand("asciidoc-lsp.restartServer", async () => {
      await disposeQuietly(client);
      await start();
      await preview.reopen();
    }),
    vscode.commands.registerCommand("asciidoc-lsp.preview.openToSide", () => preview.openToSide()),
    vscode.commands.registerCommand("asciidoc-lsp.preview.openInBrowser", () => preview.openInBrowser()),
    vscode.commands.registerCommand("asciidoc-lsp.preview.setToc", () => preview.setToc()),
  );
  await start();
}

export async function deactivate(): Promise<void> {
  await client?.dispose();
}
