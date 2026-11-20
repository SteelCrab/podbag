// Exercise the packaged WebView2 window on a disposable CI runner.
const { chromium } = require("playwright-core");
const { setTimeout: delay } = require("node:timers/promises");

async function testWindow(output) {
  const endpoint = "http://127.0.0.1:9222";
  for (let attempt = 0; attempt < 60; attempt++) {
    if (await fetch(endpoint + "/json/version").then(r => r.ok).catch(() => false)) break;
    await delay(1000);
  }
  const browser = await chromium.connectOverCDP(endpoint, { timeout: 15000 });
  try {
    const context = browser.contexts()[0];
    const page = context.pages()[0] || await context.waitForEvent("page", { timeout: 30000 });
    await page.waitForFunction(() => document.documentElement.lang === "en"
      && document.querySelector('#nav button[title="Tutorials"]'));
    // The Tutorials view works before WSL is set up. It also tests the native IPC bridge.
    await page.evaluate(() => {
      if (document.querySelector("dialog[open]")) document.querySelector("dialog[open]").close();
      document.querySelector('#nav button[title="Tutorials"]').click();
    });
    await page.locator("#tutorial-heading").waitFor();
    const session = await page.evaluate(() => window.__TAURI__.core.invoke("tutorial_lab", { action: "status" }));
    if (session.active !== false) throw new Error("Unexpected initial tutorial session");
    await page.screenshot({ path: output });
  } finally {
    await browser.close();
  }
}
testWindow(process.argv[2]).catch(error => {
  console.error(error);
  process.exitCode = 1;
});
