// Frames of a page whose CSS animations are paused at each moment, from one headless Chrome.
// Needs Google Chrome and Node.js 22 or later (built-in WebSocket).
// usage: node dev/docs/frames.mjs URL OUTDIR WIDTH HEIGHT SCALE FPS SECONDS
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [url, out, w, h, scale, fps, seconds] = process.argv.slice(2);
const chromePath = process.env.CHROME
  ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9200 + Math.floor(Math.random() * 600);
const chrome = spawn(chromePath, ["--headless=new", `--remote-debugging-port=${port}`,
  `--user-data-dir=${mkdtempSync(join(tmpdir(), "frames-"))}`, "--hide-scrollbars",
  "--no-first-run", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  let target;
  for (let i = 0; i < 60 && !target; i++) {
    await sleep(200);
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      target = list.find((t) => t.type === "page");
    } catch {}
  }
  if (!target) throw new Error("Chrome did not start");
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener("open", r));
  let id = 0;
  const waiting = new Map();
  const events = [];
  ws.addEventListener("message", (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg); waiting.delete(msg.id); }
    else events.push(msg);
  });
  const send = (method, params = {}) => new Promise((r) => {
    const n = ++id;
    waiting.set(n, r);
    ws.send(JSON.stringify({ id: n, method, params }));
  });

  await send("Emulation.setDeviceMetricsOverride",
    { width: +w, height: +h, deviceScaleFactor: +scale, mobile: false });
  await send("Emulation.setEmulatedMedia",
    { features: [{ name: "prefers-reduced-motion", value: "no-preference" }] });
  await send("Page.enable");
  await send("Page.navigate", { url });
  for (let i = 0; i < 100 && !events.some((e) => e.method === "Page.loadEventFired"); i++) await sleep(100);

  const count = Math.round(+seconds * +fps);
  for (let i = 0; i < count; i++) {
    const ms = (i / +fps) * 1000;
    await send("Runtime.evaluate", {
      expression: `document.getAnimations().forEach((a) => { a.pause(); a.currentTime = ${ms}; });
        new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))`,
      awaitPromise: true,
    });
    const shot = await send("Page.captureScreenshot", { format: "png" });
    writeFileSync(join(out, `f${String(i).padStart(5, "0")}.png`), Buffer.from(shot.result.data, "base64"));
  }
  ws.close();
} finally {
  chrome.kill();
}
