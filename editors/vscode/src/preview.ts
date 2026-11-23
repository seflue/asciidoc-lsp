// The live preview of asciidoc-lsp in VS Code: the server's page in a
// webview panel (an iframe on its localhost URL) or in the browser, plus the
// sync between editor and page. The decisions live in previewSync.ts.
import * as vscode from "vscode";
import {
  ExecuteCommandRequest,
  type LanguageClient,
  type Middleware,
  type ResponseError,
  type ShowDocumentParams,
  ShowDocumentRequest,
  type ShowDocumentResult,
} from "vscode-languageclient/node";
import {
  EchoGuard,
  TrailingThrottle,
  TOC_MODES,
  isRevealLineParams,
  previewUrl,
  revealAction,
  tocRequest,
  webviewHtml,
} from "./previewSync";

const OPEN_COMMAND = "asciidoc.preview.open";
const CURSOR_MOVED = "asciidoc/cursorMoved";
const REVEAL_LINE = "asciidoc/revealLine";

function isAsciidoc(editor: vscode.TextEditor | undefined): editor is vscode.TextEditor {
  return editor?.document.languageId === "asciidoc" && editor.document.uri.scheme === "file";
}

export class Preview implements vscode.Disposable {
  private client: LanguageClient | undefined;
  private clientSubscription: vscode.Disposable | undefined;
  private panel: vscode.WebviewPanel | undefined;
  /** Whether the page follows the editor: a panel is open or the page was opened in the browser. */
  private openedInBrowser = false;
  /**
   * The asciidoc editor the reader last worked in. While the panel has the
   * focus, VS Code has no active text editor, but the page still follows
   * this one.
   */
  private editor: vscode.TextEditor | undefined;
  private readonly echo = new EchoGuard();
  private readonly throttle = new TrailingThrottle(() => this.sendCursor());
  private readonly subscriptions: vscode.Disposable[] = [];

  constructor() {
    this.editor = isAsciidoc(vscode.window.activeTextEditor) ? vscode.window.activeTextEditor : undefined;
    this.subscriptions.push(
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (!isAsciidoc(editor)) {
          return;
        }
        this.editor = editor;
        if (this.following()) {
          this.throttle.schedule();
        }
      }),
      vscode.window.onDidChangeTextEditorSelection((event) => {
        if (!isAsciidoc(event.textEditor) || !this.following()) {
          return;
        }
        const uri = this.uriOf(event.textEditor.document);
        if (uri !== undefined && this.echo.isEcho(uri, event.selections[0].active.line)) {
          return;
        }
        this.editor = event.textEditor;
        this.throttle.schedule();
      }),
    );
  }

  /**
   * Middleware for the language client. The built-in `window/showDocument`
   * opens the file in the active editor group, which after a double-click in
   * the page is the panel's; this one opens it where the reader edits.
   */
  readonly middleware: Middleware = {
    window: {
      showDocument: async (params, token, next) => this.showDocument(params, token, next),
    },
  };

  /** Use `client` from now on; after a restart the page gets the new server's URL. */
  setClient(client: LanguageClient): void {
    this.clientSubscription?.dispose();
    this.client = client;
    this.clientSubscription = client.onNotification(REVEAL_LINE, (params: unknown) => this.revealLine(params));
  }

  /** The page of the current asciidoc editor, in a panel beside it. */
  async openToSide(): Promise<void> {
    const url = await this.open();
    if (url === undefined) {
      return;
    }
    const target = await vscode.env.asExternalUri(vscode.Uri.parse(url));
    if (this.panel) {
      // Stays in its column; Beside would move it next to whatever has the focus.
      this.panel.reveal(undefined, true);
    } else {
      this.panel = vscode.window.createWebviewPanel(
        "asciidoc-lsp.preview",
        "AsciiDoc Preview",
        { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
        // The page keeps its scroll position and event stream while hidden.
        { enableScripts: true, retainContextWhenHidden: true },
      );
      this.panel.onDidDispose(() => {
        this.panel = undefined;
        if (!this.following()) {
          this.throttle.cancel();
        }
      });
    }
    const html = webviewHtml(target.toString(true));
    // Setting the same HTML again reloads the page; the URL changes only with a new server.
    if (this.panel.webview.html !== html) {
      this.panel.webview.html = html;
    }
    this.sendCursor();
  }

  /** Pick the preview's TOC mode and send it to the server. */
  async setToc(): Promise<void> {
    if (!this.client?.isRunning()) {
      void vscode.window.showErrorMessage("AsciiDoc Preview: the language server is not running.");
      return;
    }
    const request = tocRequest(await vscode.window.showQuickPick(TOC_MODES, { placeHolder: "Preview TOC" }));
    if (request === undefined) {
      return;
    }
    try {
      await this.client.sendRequest(ExecuteCommandRequest.type, request);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      void vscode.window.showErrorMessage(`AsciiDoc Preview: ${message}`);
    }
  }

  /** The page of the current asciidoc editor, in the browser. */
  async openInBrowser(): Promise<void> {
    const url = await this.open();
    if (url === undefined) {
      return;
    }
    if (!(await vscode.env.openExternal(vscode.Uri.parse(url)))) {
      void vscode.window.showErrorMessage(`AsciiDoc Preview: could not open ${url}`);
      return;
    }
    this.openedInBrowser = true;
    this.sendCursor();
  }

  /** After a server restart: point an open panel at the new server. */
  async reopen(): Promise<void> {
    // A page in the browser still points at the old server; it is opened anew.
    this.openedInBrowser = false;
    if (this.panel) {
      await this.openToSide();
    }
  }

  dispose(): void {
    this.throttle.cancel();
    this.clientSubscription?.dispose();
    this.panel?.dispose();
    for (const subscription of this.subscriptions) {
      subscription.dispose();
    }
  }

  private following(): boolean {
    return this.panel !== undefined || this.openedInBrowser;
  }

  private uriOf(document: vscode.TextDocument): string | undefined {
    return this.client?.code2ProtocolConverter.asUri(document.uri);
  }

  /** Ask the server for the page of the current editor; it becomes the shown document. */
  private async open(): Promise<string | undefined> {
    const editor = isAsciidoc(vscode.window.activeTextEditor) ? vscode.window.activeTextEditor : this.editor;
    if (!editor) {
      void vscode.window.showErrorMessage("AsciiDoc Preview: open an AsciiDoc file first.");
      return undefined;
    }
    if (!this.client?.isRunning()) {
      void vscode.window.showErrorMessage("AsciiDoc Preview: the language server is not running.");
      return undefined;
    }
    this.editor = editor;
    try {
      const result: unknown = await this.client.sendRequest(ExecuteCommandRequest.type, {
        command: OPEN_COMMAND,
        arguments: [this.client.code2ProtocolConverter.asUri(editor.document.uri), { mode: "document" }],
      });
      const url = previewUrl(result);
      if (url === undefined) {
        void vscode.window.showErrorMessage("AsciiDoc Preview: asciidoc-lsp returned no preview URL.");
      }
      return url;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      void vscode.window.showErrorMessage(`AsciiDoc Preview: ${message}`);
      return undefined;
    }
  }

  /** Tell the server the cursor line of the editor the page follows. */
  private sendCursor(): void {
    const editor = this.editor;
    if (!this.following() || !isAsciidoc(editor) || !this.client?.isRunning()) {
      return;
    }
    void this.client.sendNotification(CURSOR_MOVED, {
      uri: this.client.code2ProtocolConverter.asUri(editor.document.uri),
      line: editor.selection.active.line,
    });
  }

  /** Handler of `asciidoc/revealLine`: the reader scrolled the page. */
  private revealLine(params: unknown): void {
    if (!this.following() || !isRevealLineParams(params)) {
      return;
    }
    const editor = this.editorFor(params.uri);
    const visible = editor?.visibleRanges[0];
    if (!editor || !visible) {
      return;
    }
    const action = revealAction(params, {
      uri: params.uri,
      lineCount: editor.document.lineCount,
      top: visible.start.line,
      bottom: editor.visibleRanges[editor.visibleRanges.length - 1].end.line,
    });
    if (action.kind === "none") {
      return;
    }
    const position = new vscode.Position(action.line, 0);
    const range = new vscode.Range(position, position);
    if (action.kind === "cursor") {
      if (!editor.selection.active.isEqual(position) || !editor.selection.isEmpty) {
        this.echo.record(params.uri, action.line);
        editor.selection = new vscode.Selection(position, position);
      }
      if (!action.scroll) {
        return;
      }
    }
    editor.revealRange(range, vscode.TextEditorRevealType.AtTop);
  }

  /** The visible editor of the document `uri`, the followed one first. */
  private editorFor(uri: string): vscode.TextEditor | undefined {
    const candidates = [this.editor, ...vscode.window.visibleTextEditors];
    return candidates.find(
      (editor): editor is vscode.TextEditor =>
        editor !== undefined &&
        vscode.window.visibleTextEditors.includes(editor) &&
        this.uriOf(editor.document) === uri,
    );
  }

  private async showDocument(
    params: ShowDocumentParams,
    token: vscode.CancellationToken,
    next: ShowDocumentRequest.HandlerSignature,
  ): Promise<ShowDocumentResult | ResponseError<void>> {
    if (params.external === true || !this.panel || !this.client) {
      return next(params, token);
    }
    const uri = this.client.protocol2CodeConverter.asUri(params.uri);
    const open = vscode.window.visibleTextEditors.find((editor) => editor.document.uri.toString() === uri.toString());
    const column = open?.viewColumn ?? this.editor?.viewColumn;
    if (column === undefined || column === this.panel.viewColumn) {
      return next(params, token);
    }
    try {
      await vscode.window.showTextDocument(uri, {
        viewColumn: column,
        preserveFocus: params.takeFocus !== true,
        selection: params.selection ? this.client.protocol2CodeConverter.asRange(params.selection) : undefined,
      });
      return { success: true };
    } catch {
      return { success: false };
    }
  }
}
