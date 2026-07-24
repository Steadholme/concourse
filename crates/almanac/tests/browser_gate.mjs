import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdirSync } from "node:fs";
import { createServer } from "node:net";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = fileURLToPath(new URL("../../../../", import.meta.url));
const SITEFLOW_PACKAGE = path.join(REPO_ROOT, "siteflow", "package.json");
const ALMANAC_BIN = process.env.ALMANAC_BROWSER_BIN
  || path.join(REPO_ROOT, "concourse", "crates", "almanac", "target", "debug", "almanac");
const CHROMIUM_PATH = process.env.CHROMIUM_PATH || "/usr/bin/chromium";
const SCREENSHOTS = path.join(REPO_ROOT, ".workflow", "evidence", "almanac-browser-gate");
const WIDTHS = [1440, 559, 389, 320];

assert.ok(existsSync(SITEFLOW_PACKAGE), `Playwright host package missing: ${SITEFLOW_PACKAGE}`);
assert.ok(existsSync(ALMANAC_BIN), `Almanac binary missing; build it first: ${ALMANAC_BIN}`);
assert.ok(existsSync(CHROMIUM_PATH), `Chromium executable missing: ${CHROMIUM_PATH}`);

const { chromium } = createRequire(SITEFLOW_PACKAGE)("playwright");

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

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
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/healthz`, { redirect: "manual" });
      if (response.status === 200) return;
    } catch {
      // The listener may not be ready yet.
    }
    await delay(100);
  }
  assert.fail(`Almanac did not become healthy.\n${serverLogs()}`);
};

const viewportFor = (width) => ({ width, height: width >= 1000 ? 900 : 844 });

test("Almanac real-server browser gate", async (t) => {
  mkdirSync(SCREENSHOTS, { recursive: true });
  const port = await freePort();
  const baseUrl = `http://localhost:${port}`;
  let logs = "";
  let stopping = false;
  const server = spawn(ALMANAC_BIN, [], {
    env: {
      ...process.env,
      BIND_ADDR: `127.0.0.1:${port}`,
      ALMANAC_STORE: "memory",
      GATEWAY_HMAC_KEY: "",
      ALMANAC_KLAXON_NOTIFY_URL: "",
      ALMANAC_KLAXON_INGEST_TOKEN: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const rememberLog = (chunk) => {
    logs = `${logs}${chunk}`.slice(-16_384);
  };
  server.stdout.on("data", rememberLog);
  server.stderr.on("data", rememberLog);
  server.on("exit", (code, signal) => {
    if (!stopping) {
      console.error(
        `[almanac-browser] server exited code=${code} signal=${signal}\n${logs}`,
      );
    }
  });

  const stopServer = async () => {
    const exited = () => server.exitCode !== null || server.signalCode !== null;
    if (exited()) return;
    stopping = true;
    const gracefulExit = once(server, "exit");
    server.kill("SIGTERM");
    await Promise.race([gracefulExit, delay(3_000)]);
    if (!exited()) {
      const forcedExit = once(server, "exit");
      server.kill("SIGKILL");
      await forcedExit;
    }
  };
  await waitForHealth(baseUrl, () => logs);
  console.error(`[almanac-browser] server healthy at ${baseUrl}`);

  const browser = await chromium.launch({
    headless: true,
    executablePath: CHROMIUM_PATH,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  t.after(async () => {
    await browser.close();
    await stopServer();
  });
  console.error("[almanac-browser] chromium ready");

  const openPage = async (url, width, options = {}) => {
    const context = await browser.newContext({
      viewport: viewportFor(width),
      ...options,
    });
    const page = await context.newPage();
    const errors = { console: [], page: [] };
    page.on("console", (message) => {
      if (message.type() === "error") errors.console.push(message.text());
    });
    page.on("pageerror", (error) => errors.page.push(error.message));
    const response = await page.goto(`${baseUrl}${url}`, { waitUntil: "domcontentloaded" });
    assert.equal(response?.status(), 200, `${url} should return 200`);
    return { context, page, errors };
  };

  const createEvent = async (page, isoDate, title, hour) => {
    await page.goto(`${baseUrl}/new?date=${isoDate}`, { waitUntil: "domcontentloaded" });
    await page.locator("#title").fill(title);
    await page.locator("#starts_at").fill(`${isoDate}T${String(hour).padStart(2, "0")}:00`);
    await page.locator("#ends_at").fill(`${isoDate}T${String(hour).padStart(2, "0")}:30`);
    const button = page.locator('form.card.editor button[type="submit"]');
    await button.focus();
    const navigated = page.waitForURL((url) => url.pathname === "/", { timeout: 10_000 });
    await page.keyboard.press("Enter");
    await navigated;
    assert.equal(new URL(page.url()).pathname, "/", `${title} should persist through native form`);
  };

  const seedContext = await browser.newContext({ viewport: viewportFor(1440) });
  const seedPage = await seedContext.newPage();
  await seedPage.goto(`${baseUrl}/`, { waitUntil: "domcontentloaded" });
  const todayLabel = await seedPage.locator(".cal-day--today").getAttribute("aria-label");
  assert.match(todayLabel || "", /^\d{4}-\d{2}-\d{2}\b/);
  const isoDate = todayLabel.slice(0, 10);
  console.error(`[almanac-browser] seeding ${isoDate}`);
  for (let index = 0; index < 4; index += 1) {
    await createEvent(seedPage, isoDate, `Responsive specimen ${index + 1}`, 8 + index);
    console.error(`[almanac-browser] seeded event ${index + 1}/4`);
  }
  await seedContext.close();
  console.error("[almanac-browser] seed context closed");
  const [year, month] = isoDate.split("-");
  const monthUrl = `/?y=${year}&m=${Number(month)}`;

  await t.test("server-counted chips stay visible at every frozen viewport", async () => {
    for (const width of WIDTHS) {
      const { context, page, errors } = await openPage(monthUrl, width);
      const day = page.locator(`.cal-day[aria-label^="${isoDate}"]`);
      assert.equal(await day.locator(".cal-event").count(), 3, `three real chips at ${width}px`);
      assert.equal(await day.locator(".cal-more").count(), 1, `one overflow link at ${width}px`);
      assert.match((await day.locator(".cal-more").innerText()).replace(/\s+/g, " "), /\+1 more/);
      const visibility = await day.locator(".cal-event").evaluateAll((nodes) => nodes.map((node) => {
        const style = getComputedStyle(node);
        const rect = node.getBoundingClientRect();
        return {
          display: style.display,
          visibility: style.visibility,
          opacity: style.opacity,
          width: rect.width,
          height: rect.height,
        };
      }));
      assert.ok(
        visibility.every((chip) => chip.display !== "none"
          && chip.visibility !== "hidden"
          && chip.opacity !== "0"
          && chip.width > 0
          && chip.height > 0),
        `all server-counted chips visible at ${width}px: ${JSON.stringify(visibility)}`,
      );
      const overflow = await page.evaluate(() => ({
        client: document.documentElement.clientWidth,
        root: document.documentElement.scrollWidth,
        body: document.body.scrollWidth,
      }));
      assert.ok(
        overflow.root <= overflow.client && overflow.body <= overflow.client,
        `horizontal overflow at ${width}px: ${JSON.stringify(overflow)}`,
      );
      assert.deepEqual(errors, { console: [], page: [] }, `browser errors at ${width}px`);
      await page.screenshot({
        path: path.join(SCREENSHOTS, `month-${width}.png`),
        fullPage: true,
      });
      await context.close();
    }
  });

  await t.test("no-JavaScript path renders and creates through native forms", async () => {
    const { context, page, errors } = await openPage(monthUrl, 390, {
      javaScriptEnabled: false,
    });
    assert.equal(await page.locator("script").count() > 0, true, "production script remains in markup");
    assert.equal(await page.locator(".cal-event").count(), 3);
    await createEvent(page, isoDate, "No JavaScript specimen", 13);
    await page.goto(`${baseUrl}${monthUrl}`, { waitUntil: "domcontentloaded" });
    const day = page.locator(`.cal-day[aria-label^="${isoDate}"]`);
    assert.equal(await day.locator(".cal-event").count(), 3);
    assert.match((await day.locator(".cal-more").innerText()).replace(/\s+/g, " "), /\+2 more/);
    assert.deepEqual(errors, { console: [], page: [] });
    await context.close();
  });

  await t.test("skip target, keyboard focus, landmarks, and labels are operable", async () => {
    const { context, page } = await openPage(monthUrl, 390);
    assert.equal(await page.locator("a.skip-link").count(), 1);
    assert.equal(await page.locator("main#main-pane[tabindex='-1']").count(), 1);
    assert.equal(await page.locator("h1").count(), 1);
    assert.ok(await page.locator("nav[aria-label]").count() >= 1);
    assert.equal(
      await page.locator("input:not([type='hidden']):not([aria-label]):not([aria-labelledby])").evaluateAll(
        (nodes) => nodes.filter((node) => !node.labels || node.labels.length === 0).length,
      ),
      0,
      "every visible input has an accessible label",
    );
    await page.keyboard.press("Tab");
    const focus = await page.evaluate(() => {
      const element = document.activeElement;
      const style = getComputedStyle(element);
      const rect = element.getBoundingClientRect();
      return {
        className: element.className,
        href: element.getAttribute("href"),
        outlineStyle: style.outlineStyle,
        outlineWidth: style.outlineWidth,
        visible: rect.width > 0 && rect.height > 0 && rect.bottom > 0,
      };
    });
    assert.equal(focus.className, "skip-link");
    assert.equal(focus.href, "#main-pane");
    assert.equal(focus.outlineStyle, "solid");
    assert.ok(parseFloat(focus.outlineWidth) >= 2);
    assert.ok(focus.visible);
    await page.keyboard.press("Enter");
    assert.equal(await page.evaluate(() => document.activeElement?.id), "main-pane");
    await context.close();
  });

  await t.test("reduced motion and forced colors preserve the conservatory", async () => {
    const reduced = await openPage(monthUrl, 390, { reducedMotion: "reduce" });
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
    await reduced.context.close();

    const forced = await openPage(monthUrl, 390, { forcedColors: "active" });
    const structure = await forced.page.locator(".cal-event").first().evaluate((node) => {
      const style = getComputedStyle(node);
      const rect = node.getBoundingClientRect();
      return {
        borderStyle: style.borderStyle,
        borderWidth: style.borderLeftWidth,
        visible: rect.width > 0 && rect.height > 0,
        text: node.textContent.trim(),
      };
    });
    assert.notEqual(structure.borderStyle, "none");
    assert.ok(parseFloat(structure.borderWidth) >= 1);
    assert.ok(structure.visible);
    assert.match(structure.text, /Responsive specimen/);
    await forced.page.screenshot({
      path: path.join(SCREENSHOTS, "month-forced-colors-390.png"),
      fullPage: true,
    });
    await forced.context.close();
  });

  await t.test("200 percent reflow equivalent retains the complete reading surface", async () => {
    const { context, page } = await openPage(monthUrl, 640, {
      javaScriptEnabled: false,
      deviceScaleFactor: 2,
    });
    const overflow = await page.evaluate(() => ({
      client: document.documentElement.clientWidth,
      root: document.documentElement.scrollWidth,
      body: document.body.scrollWidth,
    }));
    assert.ok(
      overflow.root <= overflow.client && overflow.body <= overflow.client,
      `200% reflow equivalent causes horizontal overflow: ${JSON.stringify(overflow)}`,
    );
    assert.ok(await page.locator("main#main-pane").isVisible());
    assert.equal(await page.locator(".cal-event").count(), 3);
    await context.close();
  });
});
