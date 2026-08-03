import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = fileURLToPath(new URL("../../../../", import.meta.url));
const SITEFLOW_PACKAGE = path.join(REPO_ROOT, "siteflow", "package.json");
const ATRIUM_GATE_BIN = process.env.ATRIUM_GATE_BIN
  || path.join(REPO_ROOT, "concourse", "crates", "atrium", "target", "debug", "atrium_gate");
const CHROMIUM_PATH = process.env.CHROMIUM_PATH || "/usr/bin/chromium";
const WIDTHS = [320, 390, 768, 1024, 1440];
const ACTOR_HEADERS = {
  "x-auth-subject": "atrium-browser-fixture",
  "x-auth-email": "browser-fixture@example.invalid",
  "x-auth-groups": "",
};

assert.ok(existsSync(SITEFLOW_PACKAGE), `Playwright host package missing: ${SITEFLOW_PACKAGE}`);
assert.ok(existsSync(ATRIUM_GATE_BIN), `build the fixture first: ${ATRIUM_GATE_BIN}`);
assert.ok(existsSync(CHROMIUM_PATH), `Chromium executable missing: ${CHROMIUM_PATH}`);

const { chromium } = createRequire(SITEFLOW_PACKAGE)("playwright");
const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
const viewportFor = (width) => ({ width, height: width >= 1000 ? 900 : 844 });

const freePort = async () => {
  const listener = createServer();
  listener.listen(0, "127.0.0.1");
  await once(listener, "listening");
  const address = listener.address();
  assert.equal(typeof address, "object");
  const port = address.port;
  await new Promise((resolve, reject) => {
    listener.close((error) => (error ? reject(error) : resolve()));
  });
  return port;
};

const waitForHealth = async (baseUrl, serverLogs) => {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/healthz`, { redirect: "manual" });
      if (response.status === 200) return;
    } catch {
      // The loopback listener may not be ready yet.
    }
    await delay(100);
  }
  assert.fail(`Atrium fixture did not become healthy.\n${serverLogs()}`);
};

const noHorizontalOverflow = async (page, label) => {
  const metric = await page.evaluate(() => ({
    client: document.documentElement.clientWidth,
    root: document.documentElement.scrollWidth,
    body: document.body.scrollWidth,
    offenders: Array.from(document.querySelectorAll("body *")).flatMap((node) => {
      const style = getComputedStyle(node);
      if (style.display === "none" || style.visibility === "hidden") return [];
      const rect = node.getBoundingClientRect();
      return rect.right > document.documentElement.clientWidth + 0.5 || rect.left < -0.5
        ? [{
          tag: node.tagName,
          className: typeof node.className === "string" ? node.className : "",
          left: Math.round(rect.left * 10) / 10,
          right: Math.round(rect.right * 10) / 10,
        }]
        : [];
    }).slice(0, 12),
  }));
  assert.ok(
    metric.root <= metric.client && metric.body <= metric.client,
    `${label} has horizontal overflow: ${JSON.stringify(metric)}`,
  );
  return metric;
};

const targetFloor = async (page, label, selector = "a[href],button,input:not([type=hidden]),select") => {
  const undersized = await page.locator(selector).evaluateAll((nodes) => nodes.flatMap((node) => {
    const style = getComputedStyle(node);
    const rect = node.getBoundingClientRect();
    if (style.display === "none" || style.visibility === "hidden" || rect.width === 0 || rect.height === 0) {
      return [];
    }
    return rect.width + 0.1 < 44 || rect.height + 0.1 < 44
      ? [{
        tag: node.tagName,
        className: typeof node.className === "string" ? node.className : "",
        text: (node.textContent || "").trim().slice(0, 40),
        width: Math.round(rect.width * 10) / 10,
        height: Math.round(rect.height * 10) / 10,
      }]
      : [];
  }));
  assert.deepEqual(undersized, [], `${label} has targets below 44px`);
};

const eventually = async (predicate, message, timeout = 5_000) => {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await delay(25);
  }
  assert.fail(message);
};

const walkFiles = (root, relative = "") => readdirSync(path.join(root, relative), {
  withFileTypes: true,
}).flatMap((entry) => {
  const next = path.join(relative, entry.name);
  return entry.isDirectory() ? walkFiles(root, next) : [next];
});

test("Atrium deterministic real-Chromium release gate", async (t) => {
  const artifactRoot = process.env.ATRIUM_GATE_ARTIFACT_DIR
    ? path.resolve(process.env.ATRIUM_GATE_ARTIFACT_DIR)
    : mkdtempSync(path.join(tmpdir(), "atrium-browser-gate-"));
  mkdirSync(artifactRoot, { recursive: true });
  mkdirSync(path.join(artifactRoot, "shots"), { recursive: true });
  mkdirSync(path.join(artifactRoot, "aria"), { recursive: true });

  const port = await freePort();
  const baseUrl = `http://127.0.0.1:${port}`;
  let logs = "";
  let stopping = false;
  let browser;
  const results = [];
  const server = spawn(ATRIUM_GATE_BIN, ["mixed", "0", `127.0.0.1:${port}`, "0"], {
    env: {
      ...process.env,
      GATEWAY_HMAC_KEY: "",
      AUDIT_ENABLED: "false",
      WATCHTOWER_URL: "",
      AUDIT_INGEST_TOKEN: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const rememberLog = (chunk) => {
    logs = `${logs}${chunk}`.slice(-32_768);
  };
  server.stdout.on("data", rememberLog);
  server.stderr.on("data", rememberLog);
  server.on("exit", (code, signal) => {
    if (!stopping) console.error(`[atrium-gate] fixture exited code=${code} signal=${signal}`);
  });

  const stopServer = async () => {
    if (server.exitCode !== null || server.signalCode !== null) return;
    stopping = true;
    const graceful = once(server, "exit");
    server.kill("SIGTERM");
    await Promise.race([graceful, delay(3_000)]);
    if (server.exitCode === null && server.signalCode === null) {
      const forced = once(server, "exit");
      server.kill("SIGKILL");
      await forced;
    }
  };

  t.after(async () => {
    if (browser) await browser.close();
    await stopServer();
    writeFileSync(
      path.join(artifactRoot, "results.json"),
      `${JSON.stringify({ schema: "w33d.atriumBrowserGate.v1", results }, null, 2)}\n`,
    );
    const manifest = walkFiles(artifactRoot)
      .filter((name) => name !== "manifest.sha256")
      .sort()
      .map((name) => {
        const digest = createHash("sha256").update(readFileSync(path.join(artifactRoot, name))).digest("hex");
        return `${digest}  ${name}`;
      });
    writeFileSync(path.join(artifactRoot, "manifest.sha256"), `${manifest.join("\n")}\n`);
    console.error(`[atrium-gate] synthetic artifacts: ${artifactRoot}`);
  });

  await waitForHealth(baseUrl, () => logs);
  browser = await chromium.launch({
    headless: true,
    executablePath: CHROMIUM_PATH,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  const openPage = async (width, options = {}) => {
    const { initScript, ...contextOptions } = options;
    const context = await browser.newContext({
      viewport: viewportFor(width),
      extraHTTPHeaders: ACTOR_HEADERS,
      ...contextOptions,
    });
    if (initScript) await context.addInitScript(initScript);
    const page = await context.newPage();
    const errors = { console: [], page: [], network: [] };
    page.on("console", (message) => {
      if (message.type() === "error") errors.console.push(message.text());
    });
    page.on("pageerror", (error) => errors.page.push(error.message));
    page.on("requestfailed", (request) => errors.network.push({
      url: new URL(request.url()).pathname,
      error: request.failure()?.errorText || "failed",
    }));
    await page.route("**/favicon.ico", (route) => route.fulfill({ status: 204, body: "" }));
    const response = await page.goto(`${baseUrl}/`, { waitUntil: "domcontentloaded" });
    assert.equal(response?.status(), 200);
    assert.equal(response?.headers()["cache-control"], "private, no-store");
    return { context, page, errors };
  };

  await t.test("five frozen viewports preserve targets, hostile content, semantics, and overflow", async () => {
    for (const width of WIDTHS) {
      const { context, page, errors } = await openPage(width);
      assert.equal(await page.locator("h1").count(), 1);
      assert.equal(await page.locator('form[role="search"]').count(), 1);
      assert.equal(await page.locator('[aria-live="polite"]').count(), 1);
      assert.equal(await page.locator('#refresh-status[role="status"]').count(), 1);
      const currentNav = page.locator('.appnav[aria-current="page"]');
      assert.equal(await currentNav.count(), 1);
      assert.equal(await currentNav.isVisible(), width > 720);
      assert.equal(await page.locator("script").count(), 1, "only the application script exists");
      assert.equal(await page.locator("img").count(), 0, "hostile preview did not inject an image");
      const hostile = page.locator('.slip[data-key="feed-hostile"]');
      assert.equal(await hostile.count(), 1);
      assert.match(await hostile.innerText(), /<script>Gate hostile<\/script>/);
      assert.equal(await hostile.locator(".slip__link").getAttribute("href"), "/");
      assert.equal(await page.locator("[onerror],[onclick],[onload]").count(), 0);
      const metric = await noHorizontalOverflow(page, `${width}px`);
      await targetFloor(page, `${width}px`);
      assert.deepEqual(errors, { console: [], page: [], network: [] });
      await page.screenshot({
        path: path.join(artifactRoot, "shots", `dashboard-${width}.png`),
        fullPage: true,
      });
      results.push({ case: "viewport", width, overflow: metric, targetFloor: 44, status: "pass" });
      if (width === 390) {
        writeFileSync(
          path.join(artifactRoot, "aria", "main-390.yml"),
          `${await page.locator("main").ariaSnapshot()}\n`,
        );
      }
      await context.close();
    }
  });

  await t.test("frozen keyboard prefix, menu state, current nav, and focus restoration are executable", async () => {
    const pollInit = () => {
      const nativeSetInterval = window.setInterval.bind(window);
      window.setInterval = (callback, milliseconds, ...args) => {
        if (milliseconds === 20_000) {
          window.__atriumGatePoll = () => callback(...args);
          return 1;
        }
        return nativeSetInterval(callback, milliseconds, ...args);
      };
    };
    const { context, page, errors } = await openPage(1024, { initScript: pollInit });
    const classes = [];
    for (let index = 0; index < 3; index += 1) {
      await page.keyboard.press("Tab");
      classes.push(await page.evaluate(() => document.activeElement?.className || ""));
    }
    assert.deepEqual(classes, ["appbar__brand", "appnav is-active", "skip-link"]);
    const skipBox = await page.locator(".skip-link").boundingBox();
    assert.ok(
      skipBox && skipBox.width + 0.1 >= 44 && skipBox.height + 0.1 >= 44,
      `skip target below 44px: ${JSON.stringify(skipBox)}`,
    );
    await page.keyboard.press("Enter");
    assert.equal(await page.evaluate(() => document.activeElement?.id), "columns-slot");

    const menuButton = page.locator(".usermenu__btn");
    assert.equal(await menuButton.getAttribute("aria-expanded"), "false");
    await menuButton.focus();
    assert.equal(await menuButton.getAttribute("aria-expanded"), "true");
    await eventually(
      () => page.locator(".menuitem").first().isVisible(),
      "expanded account menu did not become visible",
    );
    await page.keyboard.press("Tab");
    const menuFocus = await page.evaluate(() => ({
      tag: document.activeElement?.tagName,
      className: document.activeElement?.className,
      role: document.activeElement?.getAttribute("role"),
    }));
    assert.equal(menuFocus.role, "menuitem", `unexpected focus after menu trigger: ${JSON.stringify(menuFocus)}`);
    assert.equal(await menuButton.getAttribute("aria-expanded"), "true");
    await targetFloor(page, "open account menu", ".usermenu__btn,.menuitem");
    await page.locator(".searchbar__q").focus();
    await page.waitForTimeout(0);
    assert.equal(await menuButton.getAttribute("aria-expanded"), "false");
    assert.equal(await page.locator('.appnav[aria-current="page"]').count(), 1);

    const action = page.locator(
      '.slip[data-key="notification-beta"] .slip__act[data-action="read"]',
    );
    await action.focus();
    const responsePromise = page.waitForResponse((response) => (
      new URL(response.url()).pathname === "/api/inbox" && response.request().method() === "GET"
    ));
    await page.evaluate(() => window.__atriumGatePoll());
    const response = await responsePromise;
    assert.equal(response.status(), 200);
    assert.equal(response.headers()["cache-control"], "private, no-store");
    assert.ok(Number.isInteger((await response.json()).total_unread));
    await eventually(
      () => page.evaluate(() => ({
        key: document.activeElement?.closest(".slip")?.getAttribute("data-key"),
        action: document.activeElement?.getAttribute("data-action"),
      })).then((focus) => focus.key === "notification-beta" && focus.action === "read"),
      "fragment replacement did not restore the same action focus",
    );
    assert.deepEqual(errors, { console: [], page: [], network: [] });
    results.push({ case: "keyboard-focus", width: 1024, status: "pass" });
    await context.close();
  });

  await t.test("exact sent URL and integer payload guards reject hostile races atomically", async () => {
    const { context, page, errors } = await openPage(390);
    let releaseExact;
    let exactStartedResolve;
    let exactFinishedResolve;
    const exactStarted = new Promise((resolve) => { exactStartedResolve = resolve; });
    const exactFinished = new Promise((resolve) => { exactFinishedResolve = resolve; });
    const exactRelease = new Promise((resolve) => { releaseExact = resolve; });
    await page.route("**/api/inbox**", async (route) => {
      const url = new URL(route.request().url());
      if (url.searchParams.get("q") !== "beta") {
        await route.continue();
        return;
      }
      exactStartedResolve();
      await exactRelease;
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        headers: { "cache-control": "private, no-store" },
        body: JSON.stringify({
          summary: '<section class="annun" tabindex="-1">race summary</section>',
          columns: '<section class="dispatches"><p id="forbidden-exact-url-marker">wrong URL</p></section>',
          total_unread: 1,
        }),
      });
      exactFinishedResolve();
    });
    await page.locator(".searchbar__q").fill("beta");
    await exactStarted;
    assert.match(page.url(), /\?q=beta$/);
    await page.evaluate(() => history.replaceState(null, "", "/?q=beta#different-exact-url"));
    releaseExact();
    await exactFinished;
    await page.waitForTimeout(50);
    assert.equal(await page.locator("#forbidden-exact-url-marker").count(), 0);
    assert.equal(await page.locator('.slip[data-key="notification-beta"]').count(), 1);
    assert.equal(await page.locator("#refresh-status").isHidden(), true);
    await page.unroute("**/api/inbox**");

    let invalidStartedResolve;
    const invalidStarted = new Promise((resolve) => { invalidStartedResolve = resolve; });
    await page.route("**/api/inbox**", async (route) => {
      const url = new URL(route.request().url());
      if (url.searchParams.get("q") !== "alpha") {
        await route.continue();
        return;
      }
      invalidStartedResolve();
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        headers: { "cache-control": "private, no-store" },
        body: JSON.stringify({
          summary: '<section class="annun" tabindex="-1">invalid total</section>',
          columns: '<section class="dispatches"><p id="forbidden-fraction-marker">fraction</p></section>',
          total_unread: 1.5,
        }),
      });
    });
    const before = await page.locator("#columns-slot").innerHTML();
    await page.locator(".searchbar__q").fill("alpha");
    await invalidStarted;
    await eventually(
      () => page.locator("#refresh-status").isVisible(),
      "fractional total_unread was not rejected",
    );
    assert.equal(await page.locator("#columns-slot").innerHTML(), before);
    assert.equal(await page.locator("#forbidden-fraction-marker").count(), 0);
    assert.match(await page.locator("#refresh-status").innerText(), /may be stale/);
    assert.deepEqual(errors, { console: [], page: [], network: [] });
    results.push({ case: "exact-url-and-integer-payload", width: 390, status: "pass" });
    await context.close();
  });

  await t.test("no-JavaScript, forced-colors, and reduced-motion paths retain truthful operation", async () => {
    const noJs = await openPage(390, { javaScriptEnabled: false });
    await noJs.page.locator(".searchbar__q").fill("hostile");
    await Promise.all([
      noJs.page.waitForNavigation({ waitUntil: "domcontentloaded" }),
      noJs.page.locator('.searchbar button[type="submit"]').click(),
    ]);
    assert.equal(new URL(noJs.page.url()).searchParams.get("q"), "hostile");
    assert.equal(await noJs.page.locator('.slip[data-key="feed-hostile"]').count(), 1);
    assert.equal(await noJs.page.locator(".viewtruth").isVisible(), true);
    await noHorizontalOverflow(noJs.page, "390px no-JavaScript");
    await targetFloor(noJs.page, "390px no-JavaScript");
    assert.deepEqual(noJs.errors, { console: [], page: [], network: [] });
    await noJs.context.close();

    const forced = await openPage(390, { forcedColors: "active" });
    await forced.page.locator('.slip__act[data-action="read"]').first().focus();
    const forcedStyle = await forced.page.locator('.slip__act[data-action="read"]').first().evaluate((node) => {
      const style = getComputedStyle(node);
      return { border: style.borderStyle, width: style.borderWidth, outline: style.outlineStyle };
    });
    assert.notEqual(forcedStyle.border, "none");
    assert.ok(parseFloat(forcedStyle.width) >= 1);
    assert.notEqual(forcedStyle.outline, "none");
    await forced.page.screenshot({
      path: path.join(artifactRoot, "shots", "dashboard-forced-colors-390.png"),
      fullPage: true,
    });
    assert.deepEqual(forced.errors, { console: [], page: [], network: [] });
    await forced.context.close();

    const reduced = await openPage(390, { reducedMotion: "reduce" });
    const moving = await reduced.page.locator("*").evaluateAll((nodes) => nodes.flatMap((node) => {
      const style = getComputedStyle(node);
      const durations = `${style.transitionDuration},${style.animationDuration}`
        .split(",")
        .map((value) => value.trim());
      return durations.some((value) => value !== "0s" && value !== "0ms")
        ? [{ tag: node.tagName, className: node.className, durations }]
        : [];
    }));
    assert.deepEqual(moving, []);
    assert.deepEqual(reduced.errors, { console: [], page: [], network: [] });
    await reduced.context.close();
    results.push({ case: "no-js-forced-colors-reduced-motion", width: 390, status: "pass" });
  });
});
