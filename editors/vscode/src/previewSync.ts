// Decisions of the preview sync, kept free of the VS Code API so they can be
// tested in Node. The wiring lives in preview.ts.
//
// The editor tells the server where the cursor is (`asciidoc/cursorMoved`);
// scrolling the page scrolls the editor back (`asciidoc/revealLine`), with the
// cursor when the page asks for it (a chapter change, its start or end).
// Against the two sides pushing each other back and forth, a revealLine does
// nothing when the line is already at the top, and the cursor move it forces
// is not reported as a cursorMoved.

/** Interval of the trailing throttle on cursorMoved. */
export const THROTTLE_MS = 100;
/** How long the cursor position a revealLine left counts as its echo. */
export const ECHO_MS = 250;
/** Lines the top of the editor may differ from a revealLine and still count as there: the page rounds its line. */
export const TOLERANCE = 1;

/** Params of `asciidoc/revealLine`; `line` is 0-based. */
export interface RevealLineParams {
  uri: string;
  line: number;
  cursor?: boolean;
}

/** The parts of an editor a revealLine looks at; lines are 0-based. */
export interface EditorView {
  uri: string;
  lineCount: number;
  /** First and last visible line. */
  top: number;
  bottom: number;
}

export type RevealAction =
  | { kind: "none" }
  | { kind: "scroll"; line: number }
  | { kind: "cursor"; line: number; scroll: boolean };

/** Whether `params` is a well-formed revealLine. */
export function isRevealLineParams(params: unknown): params is RevealLineParams {
  if (typeof params !== "object" || params === null) {
    return false;
  }
  const p = params as Record<string, unknown>;
  return (
    typeof p.uri === "string" &&
    typeof p.line === "number" &&
    Number.isInteger(p.line) &&
    (p.cursor === undefined || typeof p.cursor === "boolean")
  );
}

/** What a revealLine does to `view`. Only an editor showing the document reacts. */
export function revealAction(params: RevealLineParams, view: EditorView): RevealAction {
  if (params.uri !== view.uri || view.lineCount === 0) {
    return { kind: "none" };
  }
  const line = Math.min(Math.max(params.line, 0), view.lineCount - 1);
  if (params.cursor === true) {
    // The view moves only if the line is out of sight.
    return { kind: "cursor", line, scroll: line < view.top || line > view.bottom };
  }
  if (Math.abs(view.top - line) <= TOLERANCE) {
    return { kind: "none" };
  }
  return { kind: "scroll", line };
}

/**
 * Remembers the cursor a revealLine forced, so the selection change it causes
 * is not sent back. The record stays for further events at the same place and
 * goes once the cursor has moved on or the time is up.
 */
export class EchoGuard {
  private forced: { uri: string; line: number; until: number } | undefined;

  constructor(private readonly now: () => number = Date.now) {}

  record(uri: string, line: number): void {
    this.forced = { uri, line, until: this.now() + ECHO_MS };
  }

  isEcho(uri: string, line: number): boolean {
    const forced = this.forced;
    if (forced && forced.uri === uri && forced.line === line && this.now() <= forced.until) {
      return true;
    }
    this.forced = undefined;
    return false;
  }
}

/**
 * Trailing throttle: selection changes fire per keystroke. A pending call is
 * left alone, so continuous movement still sends once per interval; `send`
 * reads the position when it runs, so the last one wins.
 */
export class TrailingThrottle {
  private timer: unknown;

  constructor(
    private readonly send: () => void,
    private readonly delay: number = THROTTLE_MS,
    private readonly timers: {
      set: (fn: () => void, ms: number) => unknown;
      clear: (handle: unknown) => void;
    } = {
      set: (fn, ms) => setTimeout(fn, ms),
      clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
    },
  ) {}

  schedule(): void {
    if (this.timer !== undefined) {
      return;
    }
    this.timer = this.timers.set(() => {
      this.timer = undefined;
      this.send();
    }, this.delay);
  }

  cancel(): void {
    if (this.timer !== undefined) {
      this.timers.clear(this.timer);
      this.timer = undefined;
    }
  }
}

/** The page URL from the answer to `asciidoc.preview.open`, or `undefined`. */
export function previewUrl(result: unknown): string | undefined {
  if (typeof result !== "object" || result === null) {
    return undefined;
  }
  const url = (result as Record<string, unknown>).url;
  return typeof url === "string" ? url : undefined;
}

/**
 * HTML of the webview: the page of the language server in a full-size iframe.
 * The webview's own policy allows nothing but that frame.
 */
export function webviewHtml(url: string): string {
  const origin = new URL(url).origin;
  const escaped = url.replace(/&/g, "&amp;").replace(/"/g, "&quot;");
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; frame-src ${origin}; style-src 'unsafe-inline';">
<style>html, body, iframe { margin: 0; padding: 0; border: 0; width: 100%; height: 100%; overflow: hidden; }</style>
</head>
<body>
<iframe src="${escaped}" title="AsciiDoc Preview"></iframe>
</body>
</html>`;
}

/** The server command that sets the preview's TOC. */
export const TOC_COMMAND = "asciidoc.preview.toc";

/** A choice of the TOC quick pick. */
export interface TocMode {
  label: string;
  value: "auto" | "off" | "left" | "right";
}

/** The TOC modes, in the order of the page's select. */
export const TOC_MODES: readonly TocMode[] = [
  { label: "Auto", value: "auto" },
  { label: "Off", value: "off" },
  { label: "Left", value: "left" },
  { label: "Right", value: "right" },
];

/** The `workspace/executeCommand` params for a picked mode; none for a cancelled pick. */
export function tocRequest(picked: TocMode | undefined): { command: string; arguments: string[] } | undefined {
  return picked === undefined ? undefined : { command: TOC_COMMAND, arguments: [picked.value] };
}

