import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ECHO_MS,
  EchoGuard,
  TrailingThrottle,
  isRevealLineParams,
  previewUrl,
  revealAction,
  webviewHtml,
} from "../src/previewSync";

const view = { uri: "file:///book/ch1.adoc", lineCount: 200, top: 40, bottom: 80 };

test("revealLine for another document does nothing", () => {
  assert.deepEqual(revealAction({ uri: "file:///book/ch2.adoc", line: 10 }, view), { kind: "none" });
});

test("revealLine scrolls the line to the top", () => {
  assert.deepEqual(revealAction({ uri: view.uri, line: 60 }, view), { kind: "scroll", line: 60 });
});

test("revealLine within the tolerance of the top does nothing", () => {
  for (const line of [39, 40, 41]) {
    assert.deepEqual(revealAction({ uri: view.uri, line }, view), { kind: "none" }, `line ${line}`);
  }
});

test("revealLine without cursor never moves the cursor", () => {
  assert.equal(revealAction({ uri: view.uri, line: 150, cursor: false }, view).kind, "scroll");
});

test("revealLine with cursor sets the cursor and scrolls only when the line is out of sight", () => {
  assert.deepEqual(revealAction({ uri: view.uri, line: 60, cursor: true }, view), {
    kind: "cursor",
    line: 60,
    scroll: false,
  });
  assert.deepEqual(revealAction({ uri: view.uri, line: 150, cursor: true }, view), {
    kind: "cursor",
    line: 150,
    scroll: true,
  });
});

test("revealLine with cursor on the top line still sets the cursor", () => {
  assert.equal(revealAction({ uri: view.uri, line: 40, cursor: true }, view).kind, "cursor");
});

test("revealLine past the end lands on the last line", () => {
  assert.deepEqual(revealAction({ uri: view.uri, line: 500, cursor: true }, view), {
    kind: "cursor",
    line: 199,
    scroll: true,
  });
});

test("revealLine params are checked", () => {
  assert.equal(isRevealLineParams({ uri: "file:///a.adoc", line: 3 }), true);
  assert.equal(isRevealLineParams({ uri: "file:///a.adoc", line: 3, cursor: true }), true);
  assert.equal(isRevealLineParams({ uri: "file:///a.adoc", line: 1.5 }), false);
  assert.equal(isRevealLineParams({ uri: "file:///a.adoc", line: "3" }), false);
  assert.equal(isRevealLineParams({ uri: "file:///a.adoc", line: 3, cursor: "yes" }), false);
  assert.equal(isRevealLineParams(null), false);
});

test("the cursor a revealLine forced is an echo until it moves or the time is up", () => {
  let now = 1000;
  const guard = new EchoGuard(() => now);
  guard.record(view.uri, 60);
  assert.equal(guard.isEcho(view.uri, 60), true);
  assert.equal(guard.isEcho(view.uri, 60), true, "further events at the same place");
  now += ECHO_MS + 1;
  assert.equal(guard.isEcho(view.uri, 60), false, "time is up");
});

test("a cursor that moved on is no echo, nor later back at the forced line", () => {
  const guard = new EchoGuard(() => 0);
  guard.record(view.uri, 60);
  assert.equal(guard.isEcho(view.uri, 61), false);
  assert.equal(guard.isEcho(view.uri, 60), false);
});

test("an echo is bound to its document", () => {
  const guard = new EchoGuard(() => 0);
  guard.record(view.uri, 60);
  assert.equal(guard.isEcho("file:///book/ch2.adoc", 60), false);
});

function fakeTimers() {
  const pending: Array<() => void> = [];
  return {
    pending,
    timers: {
      set: (fn: () => void) => {
        pending.push(fn);
        return pending.length;
      },
      clear: () => {
        pending.length = 0;
      },
    },
  };
}

test("the throttle sends once per interval, after the last change", () => {
  const { pending, timers } = fakeTimers();
  let sent = 0;
  const throttle = new TrailingThrottle(() => sent++, 100, timers);
  throttle.schedule();
  throttle.schedule();
  throttle.schedule();
  assert.equal(sent, 0, "nothing before the interval ends");
  assert.equal(pending.length, 1);
  pending.shift()?.();
  assert.equal(sent, 1);
  throttle.schedule();
  assert.equal(pending.length, 1, "a new interval after the send");
});

test("a cancelled throttle sends nothing", () => {
  const { pending, timers } = fakeTimers();
  let sent = 0;
  const throttle = new TrailingThrottle(() => sent++, 100, timers);
  throttle.schedule();
  throttle.cancel();
  assert.equal(pending.length, 0);
  throttle.schedule();
  assert.equal(pending.length, 1, "schedules again after a cancel");
});

test("the preview URL is read from the command result", () => {
  assert.equal(previewUrl({ url: "http://127.0.0.1:4711/abc/" }), "http://127.0.0.1:4711/abc/");
  assert.equal(previewUrl({ url: 3 }), undefined);
  assert.equal(previewUrl(null), undefined);
  assert.equal(previewUrl("http://127.0.0.1:4711/abc/"), undefined);
});

test("the webview frames the page and allows only its origin", () => {
  const html = webviewHtml("http://127.0.0.1:4711/abc/");
  assert.match(html, /<iframe src="http:\/\/127\.0\.0\.1:4711\/abc\/"/);
  assert.match(html, /frame-src http:\/\/127\.0\.0\.1:4711;/);
  assert.match(html, /default-src 'none'/);
});

test("the webview escapes the URL in the attribute", () => {
  const html = webviewHtml('http://127.0.0.1:4711/a"b&c/');
  assert.doesNotMatch(html, /a"b/);
  assert.match(html, /a&quot;b&amp;c/);
});
