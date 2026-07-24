import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHmac } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdirSync } from "node:fs";
import { createServer as createHttpServer, request as httpRequest } from "node:http";
import { connect as connectTcp, createServer as createTcpServer } from "node:net";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = fileURLToPath(new URL("../../../../", import.meta.url));
const SITEFLOW_PACKAGE = path.join(REPO_ROOT, "siteflow", "package.json");
const MURMUR_BIN = process.env.MURMUR_BROWSER_BIN
  || path.join(REPO_ROOT, "concourse", "crates", "murmur", "target", "debug", "murmur");
const CHROMIUM_PATH = process.env.CHROMIUM_PATH || "/usr/bin/chromium";
const SCREENSHOTS = path.join(REPO_ROOT, ".workflow", "evidence", "murmur-browser-gate");
const GATEWAY_KEY = "murmur-browser-gate-only";
const CHAT_ORIGIN = "https://chat.w33d.xyz";
const CSRF = "murmur_browser_gate_csrf";
const WIDTHS = [1440, 390, 320];
const SEARCH_MARKER = "archived searchable marker";
const OWN_MARKER = "browser-owned archived marker";
const MENTION_MARKER = "cross-user archived marker @browser";

const ADMIN = {
  sub: "browser_admin",
  email: "browser@hf",
  groups: "admins",
};
const WRITER = {
  sub: "browser_writer",
  email: "writer@hf",
  groups: "",
};

assert.ok(existsSync(SITEFLOW_PACKAGE), `Playwright host package missing: ${SITEFLOW_PACKAGE}`);
assert.ok(existsSync(MURMUR_BIN), `Murmur debug binary missing; build it first: ${MURMUR_BIN}`);
assert.ok(existsSync(CHROMIUM_PATH), `Chromium executable missing: ${CHROMIUM_PATH}`);

const { chromium } = createRequire(SITEFLOW_PACKAGE)("playwright");
const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
const viewportFor = (width) => ({ width, height: width >= 1000 ? 900 : 844 });

const freePort = async () => {
  const listener = createTcpServer();
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

// Test-only gateway simulation — never application or production code. Every HTTP request and
// WebSocket upgrade models Sluice's strip/inject boundary with a fresh minute-bound identity
// signature; only WebSocket Origin is additionally normalized to the canonical production value.
const startOriginProxy = async (backendPort) => {
  const responseHolds = [];
  const proxy = createHttpServer((request, response) => {
    const pathname = new URL(request.url, "http://loopback").pathname;
    const hold = responseHolds.find((candidate) => (
      !candidate.claimed
      && candidate.method === request.method
      && candidate.pathname === pathname
    ));
    if (hold) hold.claimed = true;
    const stripped = Object.fromEntries(
      Object.entries(request.headers).filter(([name]) => !name.startsWith("x-auth-")),
    );
    const upstream = httpRequest({
      hostname: "127.0.0.1",
      port: backendPort,
      path: request.url,
      method: request.method,
      headers: {
        ...stripped,
        ...identityHeaders(ADMIN),
        host: `127.0.0.1:${backendPort}`,
      },
    }, (upstreamResponse) => {
      if (!hold) {
        response.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
        upstreamResponse.pipe(response);
        return;
      }
      const chunks = [];
      upstreamResponse.on("data", (chunk) => chunks.push(chunk));
      upstreamResponse.on("end", async () => {
        hold.startedResolve({
          status: upstreamResponse.statusCode ?? 502,
          pathname,
        });
        await hold.released;
        if (response.destroyed) return;
        if (hold.override) {
          response.writeHead(hold.override.status, {
            "cache-control": "private, no-store",
            "content-type": "application/json",
          });
          response.end(hold.override.body);
        } else {
          response.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
          response.end(Buffer.concat(chunks));
        }
      });
    });
    upstream.on("error", (error) => {
      if (!response.headersSent) response.writeHead(502);
      response.end(`local gateway error: ${error.message}`);
    });
    request.pipe(upstream);
  });

  proxy.on("upgrade", (request, socket, head) => {
    const upstream = connectTcp(backendPort, "127.0.0.1");
    upstream.on("connect", () => {
      const stripped = Object.fromEntries(
        Object.entries(request.headers).filter(([name]) => !name.startsWith("x-auth-")),
      );
      const headers = {
        ...stripped,
        // Chromium does not copy Playwright extraHTTPHeaders into a native WebSocket upgrade.
        // Sluice does inject this verified identity in production, so the loopback gateway does
        // the same and signs every reconnect against the current minute.
        ...identityHeaders(ADMIN),
        host: `127.0.0.1:${backendPort}`,
        origin: CHAT_ORIGIN,
      };
      const lines = [`${request.method} ${request.url} HTTP/${request.httpVersion}`];
      for (const [name, value] of Object.entries(headers)) {
        if (Array.isArray(value)) {
          for (const item of value) lines.push(`${name}: ${item}`);
        } else if (value !== undefined) {
          lines.push(`${name}: ${value}`);
        }
      }
      upstream.write(`${lines.join("\r\n")}\r\n\r\n`);
      if (head.length > 0) upstream.write(head);
      socket.pipe(upstream).pipe(socket);
    });
    const closeBoth = () => {
      socket.destroy();
      upstream.destroy();
    };
    upstream.on("error", closeBoth);
    socket.on("error", closeBoth);
  });

  proxy.listen(0, "127.0.0.1");
  await once(proxy, "listening");
  const address = proxy.address();
  assert.equal(typeof address, "object");
  const holdNextResponse = (method, pathname) => {
    let startedResolve;
    let releaseResolve;
    const started = new Promise((resolve) => {
      startedResolve = resolve;
    });
    const released = new Promise((resolve) => {
      releaseResolve = resolve;
    });
    responseHolds.push({
      method,
      pathname,
      claimed: false,
      startedResolve,
      released,
      override: null,
    });
    return {
      started,
      release: () => releaseResolve(),
    };
  };
  const overrideNextResponse = (method, pathname, status, body) => {
    let startedResolve;
    const started = new Promise((resolve) => {
      startedResolve = resolve;
    });
    responseHolds.push({
      method,
      pathname,
      claimed: false,
      startedResolve,
      released: Promise.resolve(),
      override: { status, body },
    });
    return { started };
  };
  return {
    proxy,
    port: address.port,
    holdNextResponse,
    overrideNextResponse,
  };
};

const identityHeaders = (actor) => {
  const minute = Math.floor(Date.now() / 60_000);
  const signature = createHmac("sha256", GATEWAY_KEY)
    .update(`${actor.sub}\n${actor.groups}\n${minute}`)
    .digest("hex");
  return {
    "x-auth-subject": actor.sub,
    "x-auth-email": actor.email,
    "x-auth-groups": actor.groups,
    "x-auth-sig": signature,
  };
};

const waitForHealth = async (baseUrl, serverLogs) => {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/healthz`, { redirect: "manual" });
      if (response.status === 200) return;
    } catch {
      // The listener may not be ready yet.
    }
    await delay(100);
  }
  assert.fail(`Murmur did not become healthy.\n${serverLogs()}`);
};

const eventually = async (predicate, message, timeout = 10_000) => {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await delay(50);
  }
  assert.fail(message);
};

const afterBrowserTask = (page) => page.evaluate(
  () => new Promise((resolve) => setTimeout(resolve, 0)),
);

const noHorizontalOverflow = async (page, label) => {
  const overflow = await page.evaluate(() => ({
    client: document.documentElement.clientWidth,
    root: document.documentElement.scrollWidth,
    body: document.body.scrollWidth,
    offenders: Array.from(document.querySelectorAll("body *")).flatMap((node) => {
      const rect = node.getBoundingClientRect();
      return rect.right > document.documentElement.clientWidth + 0.5 || rect.left < -0.5
        ? [{
          tag: node.tagName,
          id: node.id,
          className: typeof node.className === "string" ? node.className : "",
          left: Math.round(rect.left * 10) / 10,
          right: Math.round(rect.right * 10) / 10,
          width: Math.round(rect.width * 10) / 10,
        }]
        : [];
    }).slice(0, 12),
  }));
  assert.ok(
    overflow.root <= overflow.client && overflow.body <= overflow.client,
    `${label} has horizontal overflow: ${JSON.stringify(overflow)}`,
  );
  return overflow;
};

test("Murmur real-server browser gate", async (t) => {
  mkdirSync(SCREENSHOTS, { recursive: true });
  const backendPort = await freePort();
  const backendUrl = `http://localhost:${backendPort}`;
  let proxy;
  let baseUrl;
  let logs = "";
  let stopping = false;
  let browser;

  const server = spawn(MURMUR_BIN, [], {
    env: {
      ...process.env,
      BIND_ADDR: `127.0.0.1:${backendPort}`,
      MURMUR_STORE: "memory",
      GATEWAY_HMAC_KEY: GATEWAY_KEY,
      AUDIT_ENABLED: "false",
      WATCHTOWER_URL: "",
      AUDIT_INGEST_TOKEN: "",
      KLAXON_NOTIFY_URL: "",
      KLAXON_INGEST_TOKEN: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const rememberLog = (chunk) => {
    logs = `${logs}${chunk}`.slice(-32_768);
  };
  server.stdout.on("data", rememberLog);
  server.stderr.on("data", rememberLog);
  server.on("exit", (code, signal) => {
    if (!stopping) {
      console.error(`[murmur-browser] server exited code=${code} signal=${signal}\n${logs}`);
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

  t.after(async () => {
    if (browser) await browser.close();
    if (proxy) await new Promise((resolve) => proxy.close(resolve));
    await stopServer();
  });

  await waitForHealth(backendUrl, () => logs);
  const proxyState = await startOriginProxy(backendPort);
  proxy = proxyState.proxy;
  baseUrl = `http://localhost:${proxyState.port}`;
  console.error(
    `[murmur-browser] server healthy at ${backendUrl}; test-only gateway simulation ${baseUrl}`,
  );

  const apiRequest = async (
    actor,
    route,
    {
      method = "GET",
      json,
      form,
      expected = 200,
    } = {},
  ) => {
    const headers = {
      ...identityHeaders(actor),
      cookie: `__Host-csrf=${CSRF}`,
    };
    let body;
    if (json !== undefined) {
      headers["content-type"] = "application/json";
      headers["x-csrf-token"] = CSRF;
      body = JSON.stringify(json);
    } else if (form !== undefined) {
      headers["content-type"] = "application/x-www-form-urlencoded";
      body = new URLSearchParams(form).toString();
    }
    const response = await fetch(`${backendUrl}${route}`, {
      method,
      headers,
      body,
      redirect: "manual",
    });
    const text = await response.text();
    assert.equal(
      response.status,
      expected,
      `${method} ${route} returned ${response.status}, expected ${expected}: ${text}`,
    );
    let value = null;
    if (text && response.headers.get("content-type")?.includes("application/json")) {
      value = JSON.parse(text);
    }
    return { response, text, value };
  };

  await apiRequest(ADMIN, "/api/rooms");
  const archivedRoom = (await apiRequest(ADMIN, "/api/rooms", {
    method: "POST",
    json: { name: "Browser archived room" },
    expected: 201,
  })).value.room;
  const ownMessage = (await apiRequest(
    ADMIN,
    `/api/rooms/${archivedRoom.id}/messages`,
    {
      method: "POST",
      json: { body: `${OWN_MARKER} ${SEARCH_MARKER}` },
      expected: 201,
    },
  )).value.message;
  await apiRequest(
    ADMIN,
    `/api/rooms/${archivedRoom.id}/messages/${ownMessage.id}/react`,
    { method: "POST", json: { emoji: "👍" } },
  );
  await apiRequest(
    ADMIN,
    `/api/rooms/${archivedRoom.id}/messages/${ownMessage.id}/pin`,
    { method: "POST", json: {} },
  );
  await apiRequest(WRITER, `/api/rooms/${archivedRoom.id}/join`, {
    method: "POST",
    json: {},
  });
  const mentionMessage = (await apiRequest(
    WRITER,
    `/api/rooms/${archivedRoom.id}/messages`,
    {
      method: "POST",
      json: { body: MENTION_MARKER },
      expected: 201,
    },
  )).value.message;
  await apiRequest(ADMIN, `/admin/rooms/${archivedRoom.id}/archive`, {
    method: "POST",
    form: { csrf: CSRF },
    expected: 303,
  });

  const activeRoom = (await apiRequest(ADMIN, "/api/rooms", {
    method: "POST",
    json: { name: "Browser active room" },
    expected: 201,
  })).value.room;
  const activeMessage = (await apiRequest(ADMIN, `/api/rooms/${activeRoom.id}/messages`, {
    method: "POST",
    json: { body: "active controls specimen" },
    expected: 201,
  })).value.message;
  await apiRequest(ADMIN, `/api/rooms/${activeRoom.id}/messages/${activeMessage.id}/react`, {
    method: "POST",
    json: { emoji: "✅" },
  });
  const liveRoom = (await apiRequest(ADMIN, "/api/rooms", {
    method: "POST",
    json: { name: "Browser live revocation room" },
    expected: 201,
  })).value.room;
  const liveMessage = (await apiRequest(ADMIN, `/api/rooms/${liveRoom.id}/messages`, {
    method: "POST",
    json: { body: "live revocation specimen" },
    expected: 201,
  })).value.message;
  const createEpochRoom = async (name) => {
    const room = (await apiRequest(ADMIN, "/api/rooms", {
      method: "POST",
      json: { name },
      expected: 201,
    })).value.room;
    const message = (await apiRequest(ADMIN, `/api/rooms/${room.id}/messages`, {
      method: "POST",
      json: { body: `${name} reply target` },
      expected: 201,
    })).value.message;
    return { room, message };
  };
  const epochSuccessA = await createEpochRoom("Epoch success A");
  const epochSuccessB = await createEpochRoom("Epoch success B");
  const epochFailureA = await createEpochRoom("Epoch failure A");
  const epochFailureB = await createEpochRoom("Epoch failure B");
  const readRaceA = await createEpochRoom("Read race A");
  const readRaceB = await createEpochRoom("Read race B");
  await apiRequest(WRITER, `/api/rooms/${readRaceA.room.id}/join`, {
    method: "POST",
    json: {},
  });
  const snapshotSelected = await createEpochRoom("Snapshot selected");
  const snapshotOther = await createEpochRoom("Snapshot other");
  const snapshotTrigger = await createEpochRoom("Snapshot reconnect trigger");
  await apiRequest(WRITER, `/api/rooms/${snapshotOther.room.id}/join`, {
    method: "POST",
    json: {},
  });
  console.error("[murmur-browser] real-service fixtures ready");

  browser = await chromium.launch({
    headless: true,
    executablePath: CHROMIUM_PATH,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  console.error("[murmur-browser] chromium ready");

  const openPage = async (route, width, options = {}) => {
    const context = await browser.newContext({
      viewport: viewportFor(width),
      ...options,
    });
    await context.route("**/favicon.ico", (routeHandler) => routeHandler.fulfill({
      status: 204,
      contentType: "image/x-icon",
      body: "",
    }));
    const page = await context.newPage();
    const errors = { console: [], page: [] };
    const responses = [];
    const socketEvents = [];
    page.on("console", (message) => {
      if (message.type() === "error") errors.console.push(message.text());
    });
    page.on("pageerror", (error) => errors.page.push(error.message));
    page.on("response", (response) => responses.push(response));
    page.on("websocket", (socket) => {
      socketEvents.push({ type: "open", url: socket.url() });
      socket.on("close", () => socketEvents.push({ type: "close", url: socket.url() }));
    });
    const response = await page.goto(`${baseUrl}${route}`, { waitUntil: "domcontentloaded" });
    assert.equal(response?.status(), 200, `${route} should return 200`);
    assert.equal(response?.headers()["cache-control"], "private, no-store");
    return {
      context,
      page,
      errors,
      responses,
      socketEvents,
    };
  };

  await t.test("dashboard bootstrap mints the real double-submit CSRF cookie", async () => {
    const context = await browser.newContext({
      viewport: viewportFor(390),
    });
    await context.route("**/favicon.ico", (routeHandler) => routeHandler.fulfill({
      status: 204,
      contentType: "image/x-icon",
      body: "",
    }));
    const page = await context.newPage();
    const errors = { console: [], page: [] };
    page.on("console", (message) => {
      if (message.type() === "error") errors.console.push(message.text());
    });
    page.on("pageerror", (error) => errors.page.push(error.message));
    const response = await page.goto(`${baseUrl}/?room=${activeRoom.id}`, {
      waitUntil: "domcontentloaded",
    });
    assert.equal(response?.status(), 200);
    const bootCsrf = await page.locator("#pb-boot").evaluate(
      (node) => JSON.parse(node.textContent).csrf,
    );
    const csrfCookie = (await context.cookies(baseUrl)).find(
      (cookie) => cookie.name === "__Host-csrf",
    );
    assert.ok(csrfCookie, "dashboard must mint __Host-csrf");
    assert.equal(csrfCookie.value, bootCsrf);
    assert.equal(csrfCookie.path, "/");
    assert.equal(csrfCookie.secure, true);
    assert.equal(csrfCookie.sameSite, "Lax");
    assert.equal(csrfCookie.httpOnly, false);
    await eventually(
      () => page.locator("#pb-transport.is-connected").count(),
      "CSRF bootstrap page did not establish its real socket",
    );
    assert.deepEqual(errors, { console: [], page: [] });
    await context.close();
  });

  await t.test("desktop and mobile layouts retain the complete Patchbay", async () => {
    for (const width of WIDTHS) {
      const { context, page, errors } = await openPage(`/?room=${activeRoom.id}`, width);
      assert.equal(await page.locator("main#pb-deck").count(), 1);
      assert.equal(await page.locator("h1#pb-room-title").innerText(), activeRoom.name);
      assert.ok(await page.locator("#pb-tape").isVisible());
      assert.ok(await page.locator("#pb-cue").isVisible());
      if (width < 768) {
        assert.ok(await page.locator("#pb-patchbar").isVisible(), `patchbar visible at ${width}px`);
      } else {
        assert.equal(await page.locator("#pb-patchbar").isVisible(), false);
      }
      const overflow = await noHorizontalOverflow(page, `${width}px Patchbay`);
      if (width === 390) {
        console.error(
          `[murmur-browser] 390px JS reflow client=${overflow.client} root=${overflow.root} body=${overflow.body}`,
        );
      }
      await eventually(
        () => page.locator("#pb-transport.is-connected").count(),
        `Patchbay did not establish its real socket at ${width}px`,
      );
      assert.deepEqual(errors, { console: [], page: [] }, `browser errors at ${width}px`);
      await page.screenshot({
        path: path.join(SCREENSHOTS, `active-${width}.png`),
        fullPage: true,
      });
      await context.close();
    }
  });

  await t.test("no-JavaScript native composer persists a real message", async () => {
    const { context, page, errors } = await openPage(`/?room=${activeRoom.id}`, 390, {
      javaScriptEnabled: false,
    });
    assert.equal(await page.locator("html.js").count(), 0);
    assert.ok(await page.locator("noscript .deck__nojs").isVisible());
    const body = "no-JavaScript persisted specimen";
    await page.locator("#pb-cue-input").fill(body);
    const sent = page.waitForResponse((response) => (
      response.request().method() === "POST"
      && new URL(response.url()).pathname === `/api/rooms/${activeRoom.id}/messages`
    ));
    const navigated = page.waitForURL((url) => (
      url.pathname === "/"
      && url.searchParams.get("room") === activeRoom.id
      && url.searchParams.has("receipt_message")
    ));
    await page.locator("#pb-cue-send").focus();
    await page.keyboard.press("Enter");
    const sendResponse = await sent;
    await navigated;
    assert.equal(sendResponse.status(), 303);
    assert.match(await page.locator("#pb-receipt").innerText(), /^Saved · \d{2}:\d{2} UTC$/);
    assert.ok(await page.getByText(body, { exact: true }).isVisible());
    const noJsOverflow = await noHorizontalOverflow(page, "390px no-JavaScript Patchbay");
    console.error(
      `[murmur-browser] 390px no-JS reflow client=${noJsOverflow.client} `
      + `root=${noJsOverflow.root} body=${noJsOverflow.body}`,
    );
    assert.deepEqual(errors, { console: [], page: [] });
    await context.close();
  });

  await t.test("skip link, keyboard focus, labels, and landmarks are operable", async () => {
    const { context, page, errors } = await openPage(`/?room=${activeRoom.id}`, 390);
    assert.equal(await page.locator("a.skip-link").count(), 1);
    assert.equal(await page.locator("main#pb-deck[tabindex='-1']").count(), 1);
    assert.equal(await page.locator("#pb-deck").count(), 1, "skip target id is unique");
    assert.equal(await page.locator("main").count(), 1);
    assert.equal(await page.locator("h1").count(), 1);
    assert.ok(await page.locator("nav[aria-label]").count() >= 1);
    assert.equal(await page.locator("aside[aria-label='Patch Ledger']").count(), 1);
    assert.equal(await page.locator("ol[aria-label='Conversation tape']").count(), 1);
    assert.equal(
      await page.locator("input:not([type='hidden']), textarea, select").evaluateAll(
        (nodes) => nodes.filter((node) => (
          (!node.labels || node.labels.length === 0)
          && !node.getAttribute("aria-label")
          && !node.getAttribute("aria-labelledby")
        )).length,
      ),
      0,
      "every visible form field has an accessible label",
    );

    await page.evaluate(() => document.activeElement?.blur());
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
        top: rect.top,
        visible: rect.width > 0 && rect.height > 0 && rect.bottom > 0,
      };
    });
    assert.equal(focus.className, "skip-link");
    assert.equal(focus.href, "#pb-deck");
    assert.equal(focus.outlineStyle, "solid");
    assert.ok(parseFloat(focus.outlineWidth) >= 2);
    assert.ok(focus.top >= 0 && focus.visible);
    await page.keyboard.press("Enter");
    assert.equal(await page.evaluate(() => document.activeElement?.id), "pb-deck");
    assert.equal(await page.evaluate(() => document.activeElement?.tagName), "MAIN");

    await page.locator("#pb-ledger-toggle").click();
    assert.equal(await page.locator("#pb-ledger").getAttribute("role"), "dialog");
    await page.keyboard.press("Escape");
    assert.equal(await page.locator("#pb-ledger").getAttribute("role"), null);
    assert.equal(await page.evaluate(() => document.activeElement?.id), "pb-ledger-toggle");
    assert.deepEqual(errors, { console: [], page: [] });
    await context.close();
  });

  await t.test("active-to-archived in-place switch is read-only while discovery and reads work", async () => {
    const {
      context,
      page,
      errors,
      responses,
    } = await openPage(`/?room=${activeRoom.id}`, 1440);
    const activeRow = page.locator(`#msg-${activeMessage.id}`);
    for (const action of ["reply", "react", "pin", "edit", "delete"]) {
      assert.ok(
        await activeRow.locator(`[data-act="${action}"]`).isVisible(),
        `${action} remains visible in the active room`,
      );
    }
    assert.ok(await activeRow.locator(".reaction").isVisible());
    console.error("[murmur-browser] archived switch: active controls confirmed");

    const activeUrl = page.url();
    const activeMainFrame = page.mainFrame();
    const mainFrameNavigations = [];
    page.on("framenavigated", (frame) => {
      if (frame === activeMainFrame) mainFrameNavigations.push(frame.url());
    });
    await page.evaluate(() => {
      window.__murmurGateDocumentMarker = "active-to-archived";
    });

    const writeRequests = [];
    page.on("request", (request) => {
      if (request.method() !== "POST") return;
      const pathname = new URL(request.url()).pathname;
      if (pathname.endsWith("/read")) return;
      writeRequests.push(`${request.method()} ${pathname}`);
    });

    const archivedJack = page.locator(
      `#pb-jack-list [data-room-id="${archivedRoom.id}"]`,
    );
    assert.equal(await archivedJack.count(), 1);
    await archivedJack.click();
    await eventually(
      () => page.locator("#pb-deck.deck--archived").count(),
      "in-place archived switch never applied deck--archived",
    );
    await eventually(
      () => page.locator(`#msg-${ownMessage.id}`).count(),
      "archived messages did not load after the in-place switch",
    );
    console.error("[murmur-browser] archived switch: in-place room load confirmed");
    const readResponse = await eventually(
      () => responses.find((response) => (
        response.request().method() === "POST"
        && new URL(response.url()).pathname === `/api/rooms/${archivedRoom.id}/read`
      )),
      "archived room did not persist its read marker",
    );
    assert.equal(readResponse.status(), 200);
    console.error("[murmur-browser] archived switch: read marker persisted");

    assert.equal(page.url(), activeUrl, "in-place archived switch changed the document URL");
    assert.equal(page.mainFrame(), activeMainFrame, "in-place archived switch replaced the main frame");
    assert.deepEqual(
      mainFrameNavigations,
      [],
      `in-place archived switch navigated the main frame: ${mainFrameNavigations.join(", ")}`,
    );
    assert.equal(
      await page.evaluate(() => window.__murmurGateDocumentMarker),
      "active-to-archived",
      "in-place archived switch replaced the active document",
    );

    assert.ok(await page.locator("#pb-deck").evaluate((node) => node.classList.contains("deck--archived")));
    assert.equal(await page.locator("#pb-cue-input").isDisabled(), true);
    assert.equal(await page.locator("#pb-cue-send").isDisabled(), true);
    assert.equal(await page.locator(".msg__tool:visible").count(), 0);
    assert.equal(await page.locator(".reaction:visible").count(), 0);
    const archivedRow = page.locator(`#msg-${ownMessage.id}`);
    for (const action of ["reply", "react", "pin"]) {
      const control = archivedRow.locator(`[data-act="${action}"]`);
      const count = await control.count();
      assert.ok(count <= 1);
      if (count === 1) {
        assert.equal(await control.isVisible(), false, `${action} is absent from the rendered UI`);
        let rejected = false;
        try {
          await control.click({ timeout: 350 });
        } catch {
          rejected = true;
        }
        assert.equal(rejected, true, `${action} must not be user-operable`);
      }
    }
    for (const action of ["edit", "delete"]) {
      assert.equal(
        await archivedRow.locator(`[data-act="${action}"]`).count(),
        0,
        `${action} must not be hydrated in an archived room`,
      );
    }
    const chip = archivedRow.locator(".reaction").first();
    const chipCount = await chip.count();
    assert.ok(chipCount <= 1);
    if (chipCount === 1) {
      assert.equal(await chip.isVisible(), false);
      let chipRejected = false;
      try {
        await chip.click({ timeout: 350 });
      } catch {
        chipRejected = true;
      }
      assert.equal(chipRejected, true, "archived reaction chip must not be user-operable");
    }
    console.error("[murmur-browser] archived switch: write controls unreachable");

    await page.locator("#pb-tab-search").click();
    await page.locator("#pb-ledger-q").fill(SEARCH_MARKER);
    const searched = page.waitForResponse((response) => (
      response.request().method() === "GET"
      && new URL(response.url()).pathname === "/api/search"
    ));
    await page.locator("#pb-ledger-q").press("Enter");
    assert.equal((await searched).status(), 200);
    await eventually(
      () => page.locator(`.pb-ledger__item[data-locate="${ownMessage.id}"]`).count(),
      "archived message did not remain searchable",
    );
    console.error("[murmur-browser] archived switch: Search available");

    const mentioned = page.waitForResponse((response) => (
      response.request().method() === "GET"
      && new URL(response.url()).pathname === "/api/mentions"
    ));
    await page.locator("#pb-tab-mentions").click();
    assert.equal((await mentioned).status(), 200);
    await eventually(
      () => page.locator(`.pb-ledger__item[data-locate="${mentionMessage.id}"]`).count(),
      "archived mention did not remain discoverable",
    );
    console.error("[murmur-browser] archived switch: Mentions available");

    const pinned = page.waitForResponse((response) => (
      response.request().method() === "GET"
      && new URL(response.url()).pathname === `/api/rooms/${archivedRoom.id}/pinned`
    ));
    await page.locator("#pb-tab-pins").click();
    assert.equal((await pinned).status(), 200);
    await eventually(
      () => page.locator(`.pb-ledger__item[data-locate="${ownMessage.id}"]`).count(),
      "archived pinned message did not remain discoverable",
    );
    console.error("[murmur-browser] archived switch: Pins available");

    assert.deepEqual(writeRequests, [], `archived UI emitted mutations: ${writeRequests.join(", ")}`);
    assert.deepEqual(errors, { console: [], page: [] });
    await page.screenshot({
      path: path.join(SCREENSHOTS, "archived-in-place-1440.png"),
      fullPage: true,
    });
    await context.close();
  });

  await t.test("a delayed Search response cannot overwrite a newer Pins ledger", async () => {
    const { context, page, errors } = await openPage(`/?room=${archivedRoom.id}`, 1440);
    const hold = proxyState.holdNextResponse("GET", "/api/search");
    await page.locator("#pb-tab-search").click();
    await page.locator("#pb-ledger-q").fill("archived");
    const slowSearch = page.waitForResponse((response) => (
      response.request().method() === "GET"
      && new URL(response.url()).pathname === "/api/search"
    ));
    await page.locator("#pb-ledger-q").press("Enter");
    const upstream = await Promise.race([
      hold.started,
      delay(10_000).then(() => assert.fail("timed out holding the Search response")),
    ]);
    assert.equal(upstream.status, 200);

    const pinsResponse = page.waitForResponse((response) => (
      response.request().method() === "GET"
      && new URL(response.url()).pathname === `/api/rooms/${archivedRoom.id}/pinned`
    ));
    await page.locator("#pb-tab-pins").click();
    assert.equal((await pinsResponse).status(), 200);
    await eventually(
      () => page.locator(`.pb-ledger__item[data-locate="${ownMessage.id}"]`).count(),
      "newer Pins response did not render",
    );
    assert.equal(await page.locator("#pb-tab-pins").getAttribute("aria-current"), "true");
    assert.equal(await page.locator("#pb-tab-search").getAttribute("aria-current"), null);
    assert.equal(
      await page.locator(`.pb-ledger__item[data-locate="${mentionMessage.id}"]`).count(),
      0,
    );

    hold.release();
    const slowSearchResponse = await slowSearch;
    assert.equal(slowSearchResponse.status(), 200);
    await slowSearchResponse.finished();
    await afterBrowserTask(page);
    assert.equal(await page.locator("#pb-tab-pins").getAttribute("aria-current"), "true");
    assert.equal(await page.locator("#pb-tab-search").getAttribute("aria-current"), null);
    assert.equal(
      await page.locator(`.pb-ledger__item[data-locate="${mentionMessage.id}"]`).count(),
      0,
      "stale Search results must not replace the newer Pins DOM",
    );
    assert.ok(await page.locator(`.pb-ledger__item[data-locate="${ownMessage.id}"]`).isVisible());
    console.error("[murmur-browser] ledger race: delayed Search could not overwrite Pins");
    assert.deepEqual(errors, { console: [], page: [] });
    await context.close();
  });

  await t.test("live archive invalidation closes the real socket and reconciles authority", async () => {
    const {
      context,
      page,
      errors,
      socketEvents,
    } = await openPage(`/?room=${liveRoom.id}`, 1440);
    await eventually(
      () => page.locator("#pb-transport.is-connected").count(),
      "real Murmur WebSocket never reached Connected",
    );
    assert.ok(socketEvents.some((event) => event.type === "open"));
    assert.ok(await page.locator(`#msg-${liveMessage.id} [data-act="edit"]`).isVisible());

    await page.evaluate(() => {
      const meter = document.getElementById("pb-transport-label");
      const catchup = document.getElementById("pb-catchup");
      window.__murmurGateTransitions = [];
      const record = () => {
        window.__murmurGateTransitions.push({
          transport: meter?.textContent || "",
          catchingUp: catchup ? !catchup.hidden : false,
        });
      };
      record();
      new MutationObserver(record).observe(meter, {
        attributes: true,
        childList: true,
        subtree: true,
      });
      new MutationObserver(record).observe(catchup, {
        attributes: true,
        childList: true,
        subtree: true,
      });
    });

    const writeRequests = [];
    page.on("request", (request) => {
      if (request.method() !== "POST") return;
      const pathname = new URL(request.url()).pathname;
      if (pathname.endsWith("/read")) return;
      writeRequests.push(`${request.method()} ${pathname}`);
    });
    await apiRequest(ADMIN, `/admin/rooms/${liveRoom.id}/archive`, {
      method: "POST",
      form: { csrf: CSRF },
      expected: 303,
    });

    await eventually(
      () => socketEvents.some((event) => event.type === "close"),
      "archive invalidation did not close the subscribed real WebSocket",
    );
    await eventually(
      async () => (await page.locator("#pb-transport-label").innerText()) === "Connected",
      "browser did not reconnect after archive invalidation",
      15_000,
    );
    await eventually(
      () => page.locator("#pb-deck.deck--archived").count(),
      "reconnect reconcile did not fail closed to archived authority",
      15_000,
    );
    const transitions = await page.evaluate(() => window.__murmurGateTransitions);
    assert.ok(
      transitions.some((state) => state.transport.startsWith("Reconnecting")),
      `missing Reconnecting transition: ${JSON.stringify(transitions)}`,
    );
    assert.ok(
      transitions.some((state) => state.catchingUp),
      `missing Catching up transition: ${JSON.stringify(transitions)}`,
    );
    assert.equal(await page.locator("#pb-catchup").isHidden(), true);
    assert.equal(await page.locator("#pb-cue-input").isDisabled(), true);
    assert.equal(await page.locator("#pb-cue-send").isDisabled(), true);
    assert.equal(await page.locator(".msg__tool:visible").count(), 0);
    assert.equal(await page.locator(".reaction:visible").count(), 0);
    assert.deepEqual(writeRequests, [], `live revocation emitted mutations: ${writeRequests.join(", ")}`);
    assert.deepEqual(errors, { console: [], page: [] });
    console.error(
      `[murmur-browser] live archive: socket close/reconnect/catch-up confirmed `
      + `(${socketEvents.filter((event) => event.type === "close").length} close)`,
    );
    await context.close();
  });

  await t.test("a dynamically delayed rooms snapshot cannot clear newer WebSocket unread", async () => {
    const {
      context,
      page,
      errors,
      socketEvents,
    } = await openPage(`/?room=${snapshotSelected.room.id}`, 1440);
    await eventually(
      () => page.locator(`#msg-${snapshotSelected.message.id}`).count(),
      "snapshot-race selected room did not finish its initial load",
    );
    await eventually(
      () => page.locator("#pb-transport.is-connected").count(),
      "snapshot-race page did not establish its real socket",
    );

    const otherJack = page.locator(
      `#pb-jack-list [data-room-id="${snapshotOther.room.id}"]`,
    );
    const visibleUnread = otherJack.locator(
      ".jack__mark--unread:not([hidden]), .room__unread:not([hidden])",
    );
    assert.equal(await otherJack.count(), 1);
    assert.equal(await visibleUnread.isVisible(), false);

    const initialOpenCount = socketEvents.filter((event) => event.type === "open").length;
    const initialCloseCount = socketEvents.filter((event) => event.type === "close").length;
    const hold = proxyState.holdNextResponse("GET", "/api/rooms");
    const delayedRoomsResponse = page.waitForResponse((response) => (
      response.request().method() === "GET"
      && new URL(response.url()).pathname === "/api/rooms"
    ));
    await apiRequest(ADMIN, `/admin/rooms/${snapshotTrigger.room.id}/archive`, {
      method: "POST",
      form: { csrf: CSRF },
      expected: 303,
    });
    const upstream = await Promise.race([
      hold.started,
      delay(15_000).then(() => assert.fail("reconnect did not produce a held rooms snapshot")),
    ]);
    assert.equal(upstream.status, 200);
    await eventually(
      () => socketEvents.filter((event) => event.type === "close").length > initialCloseCount,
      "snapshot trigger did not close the original real socket",
    );
    await eventually(
      () => socketEvents.filter((event) => event.type === "open").length > initialOpenCount,
      "snapshot trigger did not establish the replacement real socket",
    );
    await eventually(
      () => page.locator("#pb-catchup").isVisible(),
      "held rooms request was not the reconnect snapshot",
    );

    await apiRequest(WRITER, `/api/rooms/${snapshotOther.room.id}/messages`, {
      method: "POST",
      json: { body: "WebSocket unread newer than the held rooms snapshot" },
      expected: 201,
    });
    await eventually(
      () => visibleUnread.isVisible(),
      "new other-room WebSocket message did not create an unread badge",
    );

    const selectedMessagesPath = `/api/rooms/${snapshotSelected.room.id}/messages`;
    const reconciledMessagesResponse = page.waitForResponse((response) => (
      response.request().method() === "GET"
      && new URL(response.url()).pathname === selectedMessagesPath
    ));
    hold.release();
    const roomsResponse = await delayedRoomsResponse;
    assert.equal(roomsResponse.status(), 200);
    assert.equal((await reconciledMessagesResponse).status(), 200);
    await eventually(
      () => page.locator("#pb-catchup").isHidden(),
      "snapshot reconcile did not finish after releasing the held rooms response",
    );
    assert.ok(
      await visibleUnread.isVisible(),
      "stale rooms snapshot cleared the newer WebSocket unread badge",
    );
    assert.equal(
      await page.locator("#pb-room-title").innerText(),
      snapshotSelected.room.name,
    );
    assert.deepEqual(errors, { console: [], page: [] });
    console.error(
      "[murmur-browser] rooms snapshot race: newer WebSocket unread survived stale snapshot",
    );
    await context.close();
  });

  await t.test("late A-room send responses cannot corrupt B-room reply or draft state", async () => {
    const runScenario = async ({
      source,
      destination,
      body,
      draft,
      expectedStatus,
    }) => {
      const { context, page, errors } = await openPage(`/?room=${source.room.id}`, 1440);
      const responsePath = `/api/rooms/${source.room.id}/messages`;
      const hold = proxyState.holdNextResponse("POST", responsePath);
      const lateResponse = page.waitForResponse((response) => (
        response.request().method() === "POST"
        && new URL(response.url()).pathname === responsePath
      ));
      await page.locator("#pb-cue-input").evaluate((node, value) => {
        node.value = value;
        node.dispatchEvent(new Event("input", { bubbles: true }));
      }, body);
      await page.locator("#pb-cue-input").press("Enter");
      const upstream = await Promise.race([
        hold.started,
        delay(10_000).then(() => assert.fail(`timed out waiting to hold ${responsePath}`)),
      ]);
      assert.equal(upstream.status, expectedStatus);

      await page.locator(
        `#pb-jack-list [data-room-id="${destination.room.id}"]`,
      ).click();
      await eventually(
        () => page.locator(`#msg-${destination.message.id}`).count(),
        `destination room ${destination.room.id} did not load`,
      );
      await page.locator(
        `#msg-${destination.message.id} [data-act="reply"]`,
      ).click();
      await page.locator("#pb-cue-input").fill(draft);
      assert.equal(await page.locator("#pb-reply-to").inputValue(), destination.message.id);
      assert.equal(await page.locator("#pb-receipt").innerText(), "");

      hold.release();
      assert.equal((await lateResponse).status(), expectedStatus);
      await eventually(
        async () => !(await page.locator("#pb-cue-send").isDisabled()),
        "destination composer did not recover after the stale response settled",
      );
      assert.equal(await page.locator("#pb-room-title").innerText(), destination.room.name);
      assert.equal(await page.locator("#pb-reply-to").inputValue(), destination.message.id);
      assert.equal(await page.locator("#pb-cue-input").inputValue(), draft);
      assert.equal(await page.locator("#pb-receipt").innerText(), "");
      assert.equal((await page.locator("#pb-tape").innerText()).includes(body.slice(0, 200)), false);
      assert.ok(await page.locator(`#msg-${destination.message.id}`).isVisible());
      assert.deepEqual(errors.page, []);
      if (expectedStatus >= 400) {
        assert.ok(
          errors.console.length >= 1
          && errors.console.every((message) => message.includes(`status of ${expectedStatus}`)),
          `unexpected console errors for the intentional ${expectedStatus}: ${errors.console.join(" | ")}`,
        );
      } else {
        assert.deepEqual(errors.console, []);
      }
      console.error(
        `[murmur-browser] selection epoch: stale ${expectedStatus} response preserved B reply/draft`,
      );
      await context.close();
    };

    await runScenario({
      source: epochSuccessA,
      destination: epochSuccessB,
      body: "late successful A-room send",
      draft: "B reply draft survives A success",
      expectedStatus: 201,
    });
    await runScenario({
      source: epochFailureA,
      destination: epochFailureB,
      body: "x".repeat(8193),
      draft: "B reply draft survives A failure",
      expectedStatus: 400,
    });
  });

  await t.test("a delayed A-room read acknowledgement cannot clear a newer unread badge in B", async () => {
    const readPath = `/api/rooms/${readRaceA.room.id}/read`;
    const hold = proxyState.holdNextResponse("POST", readPath);
    const {
      context,
      page,
      errors,
      responses,
    } = await openPage(`/?room=${readRaceA.room.id}`, 1440);
    const upstream = await Promise.race([
      hold.started,
      delay(10_000).then(() => assert.fail("timed out holding the A-room read acknowledgement")),
    ]);
    assert.equal(upstream.status, 200);
    await eventually(
      () => page.locator("#pb-transport.is-connected").count(),
      "read-race page did not establish its real socket",
    );

    const failedR2 = proxyState.overrideNextResponse(
      "POST",
      readPath,
      503,
      JSON.stringify({ error: "browser_gate_read_failure" }),
    );
    await apiRequest(WRITER, `/api/rooms/${readRaceA.room.id}/messages`, {
      method: "POST",
      json: { body: "m2 starts a newer read sequence" },
      expected: 201,
    });
    const r2Upstream = await Promise.race([
      failedR2.started,
      delay(10_000).then(() => assert.fail("m2 did not start the newer R2 read")),
    ]);
    assert.equal(r2Upstream.status, 200);
    await eventually(
      () => responses.some((response) => (
        response.request().method() === "POST"
        && new URL(response.url()).pathname === readPath
        && response.status() === 503
      )),
      "R2 did not surface the controlled failure",
    );

    await page.locator(
      `#pb-jack-list [data-room-id="${readRaceB.room.id}"]`,
    ).click();
    await eventually(
      () => page.locator(`#msg-${readRaceB.message.id}`).count(),
      "read-race B room did not load",
    );
    await apiRequest(WRITER, `/api/rooms/${readRaceA.room.id}/messages`, {
      method: "POST",
      json: { body: "m3 creates unread after R2 failed" },
      expected: 201,
    });
    const aJack = page.locator(
      `#pb-jack-list [data-room-id="${readRaceA.room.id}"]`,
    );
    const visibleUnread = aJack.locator(
      ".jack__mark--unread:not([hidden]), .room__unread:not([hidden])",
    );
    await eventually(
      () => visibleUnread.isVisible(),
      "new A-room WebSocket message did not create an unread badge",
    );

    hold.release();
    const readResponse = await eventually(
      () => responses.find((response) => (
        response.request().method() === "POST"
        && new URL(response.url()).pathname === readPath
        && response.status() === 200
      )),
      "held A-room read response was not released",
    );
    assert.equal(readResponse.status(), 200);
    await afterBrowserTask(page);
    assert.ok(
      await visibleUnread.isVisible(),
      "stale A-room read acknowledgement cleared the newer unread badge",
    );
    assert.equal(await page.locator("#pb-room-title").innerText(), readRaceB.room.name);
    assert.deepEqual(errors.page, []);
    assert.ok(
      errors.console.length >= 1
      && errors.console.every((message) => message.includes("status of 503")),
      `unexpected read-race console errors: ${errors.console.join(" | ")}`,
    );
    console.error(
      "[murmur-browser] read race: R1 success/R2 failure preserved m3 unread",
    );
    await context.close();
  });

  await t.test("reduced motion and forced colors preserve state and focus geometry", async () => {
    const reduced = await openPage(`/?room=${activeRoom.id}`, 390, {
      reducedMotion: "reduce",
    });
    assert.equal(
      await reduced.page.evaluate(() => matchMedia("(prefers-reduced-motion: reduce)").matches),
      true,
    );
    const moving = await reduced.page.locator(".page-chat, .page-chat *").evaluateAll(
      (nodes) => nodes.flatMap((node) => {
        const style = getComputedStyle(node);
        const values = [
          ...style.transitionDuration.split(","),
          ...style.animationDuration.split(","),
        ];
        const toMilliseconds = (value) => {
          const trimmed = value.trim();
          if (trimmed.endsWith("ms")) return Number.parseFloat(trimmed);
          if (trimmed.endsWith("s")) return Number.parseFloat(trimmed) * 1000;
          return Number.parseFloat(trimmed) || 0;
        };
        return values.some((value) => toMilliseconds(value) > 0.011)
          ? [{ tag: node.tagName, className: node.className, values }]
          : [];
      }),
    );
    assert.deepEqual(moving, []);
    assert.deepEqual(reduced.errors, { console: [], page: [] });
    await reduced.context.close();

    const forced = await openPage(`/?room=${activeRoom.id}`, 390, {
      forcedColors: "active",
    });
    assert.equal(
      await forced.page.evaluate(() => matchMedia("(forced-colors: active)").matches),
      true,
    );
    await forced.page.evaluate(() => document.activeElement?.blur());
    await forced.page.keyboard.press("Tab");
    const forcedFocus = await forced.page.locator(".skip-link").evaluate((node) => {
      const style = getComputedStyle(node);
      const rect = node.getBoundingClientRect();
      return {
        borderStyle: style.borderStyle,
        borderWidth: style.borderLeftWidth,
        outlineStyle: style.outlineStyle,
        outlineWidth: style.outlineWidth,
        visible: rect.width > 0 && rect.height > 0 && rect.top >= 0,
      };
    });
    assert.notEqual(forcedFocus.borderStyle, "none");
    assert.ok(parseFloat(forcedFocus.borderWidth) >= 2);
    assert.equal(forcedFocus.outlineStyle, "solid");
    assert.ok(parseFloat(forcedFocus.outlineWidth) >= 2);
    assert.ok(forcedFocus.visible);
    assert.deepEqual(forced.errors, { console: [], page: [] });
    await forced.page.screenshot({
      path: path.join(SCREENSHOTS, "forced-colors-390.png"),
      fullPage: true,
    });
    await forced.context.close();
  });

  await t.test("200 percent reflow equivalent keeps the full reading and composer surface", async () => {
    const { context, page, errors } = await openPage(`/?room=${activeRoom.id}`, 640, {
      javaScriptEnabled: false,
      deviceScaleFactor: 2,
    });
    await noHorizontalOverflow(page, "200% reflow equivalent");
    assert.ok(await page.locator("main#pb-deck").isVisible());
    assert.ok(await page.locator("#pb-tape").isVisible());
    assert.ok(await page.locator("#pb-cue-input").isVisible());
    assert.equal(await page.locator("a.skip-link").count(), 1);
    assert.deepEqual(errors, { console: [], page: [] });
    await context.close();
  });
});
