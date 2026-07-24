import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";

class Element {
  constructor(tag = "div") {
    this.tagName = tag;
    this.children = [];
    this.listeners = {};
    this.classList = { add() {}, remove() {} };
    this.disabled = false;
    this.parentNode = null;
    this.value = "";
    this.textContent = "";
  }

  addEventListener(name, handler) {
    this.listeners[name] = handler;
  }

  appendChild(child) {
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  removeChild(child) {
    this.children = this.children.filter((candidate) => candidate !== child);
    child.parentNode = null;
  }

  replaceChildren(...children) {
    this.children = [];
    for (const child of children) this.appendChild(child);
  }

  setAttribute() {}
}

const input = new Element("input");
input.value = "Dentist tomorrow 12pm";
const reviewButton = new Element("button");
const intent = new Element("input");
intent.value = "review";
const csrf = new Element("input");
csrf.value = "csrf";
const calendar = new Element("select");
calendar.value = "default:test";
const controls = {
  text: input,
  intent,
  csrf_token: csrf,
  calendar_id: calendar,
};
const form = new Element("form");
form.querySelector = (selector) => {
  if (selector === "input[name=text]") return input;
  if (selector === "button[type=submit]") return reviewButton;
  const match = selector.match(/^\[name="([^"]+)"\]$/);
  return match ? controls[match[1]] ?? null : null;
};
form.submit = () => {
  throw new Error("native submit must not run during the enhanced review/commit flow");
};

const stage = new Element("section");
const toastHost = new Element("div");
const created = [];
globalThis.document = {
  querySelector(selector) {
    return selector === "[data-quickadd]" ? form : null;
  },
  getElementById(id) {
    if (id === "bench-stage") return stage;
    if (id === "toast-host") return toastHost;
    return null;
  },
  createElement(tag) {
    const element = new Element(tag);
    created.push(element);
    return element;
  },
};
globalThis.requestAnimationFrame = (callback) => callback();
globalThis.setTimeout = () => 0;

let fetchCount = 0;
let commitResolve;
globalThis.fetch = (_url, options) => {
  fetchCount += 1;
  const payload = JSON.parse(options.body);
  if (payload.intent === "review") {
    return Promise.resolve({
      json: () =>
        Promise.resolve({
          kind: "review",
          title: "Dentist",
          starts_at: 4_000,
          when: "Tomorrow at 12:00",
          calendar_name: "Default",
        }),
    });
  }
  return new Promise((resolve) => {
    commitResolve = () =>
      resolve({
        json: () =>
          Promise.resolve({
            kind: "committed",
            title: "Dentist",
            when: "Tomorrow at 12:00",
          }),
      });
  });
};

const source = fs.readFileSync(
  new URL("../src/handlers/events.rs", import.meta.url),
  "utf8",
);
const enhancer = source.match(
  /const QUICKADD_ENHANCER: &str = r##"([\s\S]*?)"##;/,
)?.[1];
assert.ok(enhancer, "extract the production enhancer");
const script = enhancer.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script, "extract the production script");
vm.runInThisContext(script, { filename: "QUICKADD_ENHANCER" });

form.listeners.submit({ preventDefault() {} });
await new Promise(setImmediate);
await new Promise(setImmediate);
const commit = created.find(
  (element) =>
    element.tagName === "button" && element.textContent === "Add to calendar",
);
assert.ok(commit, "review creates the real Commit control");
assert.equal(fetchCount, 1, "review starts exactly one request");

commit.listeners.click();
commit.listeners.click();
assert.equal(commit.disabled, true, "Commit disables synchronously");
assert.equal(fetchCount, 2, "two synchronous Commit activations start one request");

commitResolve();
await new Promise(setImmediate);
await new Promise(setImmediate);
assert.equal(commit.disabled, false, "terminal response releases the shared latch");
