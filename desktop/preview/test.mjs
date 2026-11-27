// Run the Desktop preview tests with Node.js 22+ and Chrome (or CHROME=path).
import { spawn } from "node:child_process";
import { EventEmitter, once } from "node:events";
import { mkdtemp, readdir, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, isAbsolute, relative, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const desktop = fileURLToPath(new URL("../", import.meta.url));
const chrome = process.env.CHROME || (process.platform === "darwin"
  ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "google-chrome");
const mime = {
  ".css": "text/css",
  ".html": "text/html",
  ".js": "text/javascript",
  ".json": "application/json",
  ".mjs": "text/javascript",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".woff2": "font/woff2",
};
const binding = "__podbagPreviewResult";

async function timeout(promise, ms, reason) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(reason)), ms); }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

const server = createServer(async (request, response) => {
  try {
    const pathname = decodeURIComponent(new URL(request.url, "http://127.0.0.1").pathname);
    const path = resolve(desktop, "." + pathname);
    const file = relative(desktop, path);
    if (!file || file === ".." || file.startsWith("../") || file.startsWith("..\\") || isAbsolute(file)) {
      response.writeHead(404).end();
      return;
    }
    const body = await readFile(path);
    response.writeHead(200, {
      "Content-Type": mime[extname(path)] ?? "application/octet-stream",
      "Cache-Control": "no-store",
    }).end(body);
  } catch {
    response.writeHead(404).end();
  }
});

function startChrome(profile) {
  const child = spawn(chrome, [
    "--headless=new", "--window-size=1280,900", "--no-first-run", "--lang=en-US",
    "--no-default-browser-check", "--disable-background-networking",
    "--disable-background-timer-throttling", "--disable-renderer-backgrounding",
    "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0",
    `--user-data-dir=${profile}`, "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"] });
  const startup = Promise.withResolvers();
  const logs = [];
  createInterface({ input: child.stderr }).on("line", (line) => {
    logs.push(line);
    if (logs.length > 12) logs.shift();
    const url = line.match(/^DevTools listening on (ws:\/\/127\.0\.0\.1:\d+\/devtools\/browser\/\S+)$/)?.[1];
    if (url) startup.resolve(url);
  });
  child.once("error", startup.reject);
  const closed = new Promise((done) => child.once("close", (code, signal) => {
    startup.reject(new Error(`Chrome exited (${code ?? signal}).\n${logs.join("\n")}`));
    done();
  }));
  return { child, closed, debuggerUrl: startup.promise };
}

async function connect(url) {
  const socket = new WebSocket(url);
  const opened = Promise.withResolvers();
  const pending = new Map();
  const events = new EventEmitter();
  let sequence = 0;
  const disconnected = () => {
    const error = new Error("Chrome DevTools disconnected");
    opened.reject(error);
    for (const request of pending.values()) request.reject(error);
    pending.clear();
  };
  socket.addEventListener("open", opened.resolve, { once: true });
  socket.addEventListener("error", disconnected, { once: true });
  socket.addEventListener("close", disconnected, { once: true });
  socket.addEventListener("message", ({ data }) => {
    const { id, sessionId, method, params, result, error } = JSON.parse(data);
    if (id === undefined) {
      if (sessionId) events.emit(sessionId, method, params);
      return;
    }
    const request = pending.get(id);
    if (!request) return;
    pending.delete(id);
    if (error) request.reject(new Error(error.message));
    else request.resolve(result);
  });
  try {
    await timeout(opened.promise, 10_000, "Chrome DevTools did not connect");
  } catch (error) {
    socket.close();
    throw error;
  }
  return {
    events,
    close: () => socket.close(),
    async send(method, params = {}, sessionId) {
      if (socket.readyState !== WebSocket.OPEN) throw new Error("Chrome DevTools is closed");
      const id = ++sequence;
      const request = Promise.withResolvers();
      pending.set(id, request);
      try {
        socket.send(JSON.stringify({ id, method, params, sessionId }));
        return await timeout(request.promise, 10_000, `Chrome did not answer ${method}`);
      } finally {
        pending.delete(id);
      }
    },
  };
}

// Run before the test's scripts so even an immediate result is observed.
const observeResult = `
  if (window === window.top) {
    const observer = new MutationObserver(() => {
      const result = document.getElementById("result")?.textContent.trim();
      if (result === "PASS" || result?.startsWith("FAIL")) {
        observer.disconnect();
        window[${JSON.stringify(binding)}](result);
      }
    });
    observer.observe(document, { childList: true, characterData: true, subtree: true });
  }
`;

async function runTest(cdp, url) {
  const { browserContextId } = await cdp.send("Target.createBrowserContext");
  try {
    const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank", browserContextId });
    const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
    const send = (method, params) => cdp.send(method, params, sessionId);
    const result = Promise.withResolvers();
    const errors = [];
    const onEvent = (method, params) => {
      if (method === "Runtime.bindingCalled" && params.name === binding) {
        result.resolve(params.payload);
      } else if (method === "Runtime.exceptionThrown") {
        const { exception, text } = params.exceptionDetails;
        errors.push(exception?.description || text);
      }
    };
    cdp.events.on(sessionId, onEvent);
    try {
      await send("Page.enable");
      await send("Runtime.enable");
      await send("Runtime.addBinding", { name: binding });
      await send("Page.addScriptToEvaluateOnNewDocument", { source: observeResult });
      const navigation = await send("Page.navigate", { url });
      if (navigation.errorText) throw new Error(navigation.errorText);
      const status = await timeout(result.promise, 30_000, "Timed out waiting for #result");
      if (status !== "PASS") throw new Error(status);
    } catch (error) {
      if (errors.length) error.message += "\n" + errors.join("\n");
      throw error;
    } finally {
      cdp.events.off(sessionId, onEvent);
    }
  } finally {
    await cdp.send("Target.disposeBrowserContext", { browserContextId });
  }
}

async function main() {
  const tests = (await readdir(new URL("./", import.meta.url), { recursive: true, withFileTypes: true }))
    .filter((entry) => entry.isFile() && entry.name.endsWith(".test.html"))
    .map((entry) => relative(desktop, resolve(entry.parentPath, entry.name)))
    .sort();
  if (!tests.length) throw new Error("No Desktop preview tests found");
  const profile = await mkdtemp(resolve(tmpdir(), "podbag-desktop-tests-"));
  let browser, cdp;
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const origin = `http://127.0.0.1:${server.address().port}`;
    browser = startChrome(profile);
    cdp = await connect(await timeout(browser.debuggerUrl, 15_000, "Chrome did not start DevTools"));

    let passed = 0;
    for (const file of tests) {
      try {
        await runTest(cdp, `${origin}/${file.split(/[\\/]/).map(encodeURIComponent).join("/")}`);
        console.log(`PASS ${file}`);
        passed++;
      } catch (error) {
        console.error(`FAIL ${file}\n${error.message}`);
      }
    }
    console.log(`${passed}/${tests.length} Desktop preview tests passed`);
    if (passed !== tests.length) process.exitCode = 1;
  } finally {
    cdp?.close();
    browser?.child.kill();
    await browser?.closed;
    server.close();
    server.closeAllConnections();
    await rm(profile, { recursive: true, force: true, maxRetries: 3 });
  }
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
