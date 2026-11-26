// Client smoke test: runs app.js in a stub DOM.
// Legacy checks use stub fetch; the three user workflows drive the real
// server (GALLY_BASE_URL, set by tests/client_smoke.rs) so client and server
// contracts are exercised together. Every button path in index.html is exercised.
"use strict";

const assert = require("assert");
const { webcrypto } = require("node:crypto");
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const APP = fs.readFileSync(process.env.APP_JS || path.join(__dirname, "..", "app.js"), "utf8");
const BASE = (process.env.GALLY_BASE_URL || "").replace(/\/+$/, "");
const INVITE = process.env.GALLY_TEST_INVITE;

// 1x1 PNG: real bytes so the server can generate previews.
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
  "base64"
);

const START_HIDDEN = new Set([
  "edit-btn",
  "editbar",
  "empty",
  "file-input",
  "gallery-state",
  "home-link",
  "key-banner",
  "lightbox",
  "load-more",
  "setpass-private",
  "setpass-remove",
]);

class ClassList {
  constructor() {
    this.set = new Set();
  }
  add(...names) {
    for (const n of names) if (n) this.set.add(n);
  }
  remove(...names) {
    for (const n of names) this.set.delete(n);
  }
  contains(name) {
    return this.set.has(name);
  }
  toggle(name, force) {
    const on = force === undefined ? !this.set.has(name) : !!force;
    if (on) this.set.add(name);
    else this.set.delete(name);
    return on;
  }
}

function matches(el, selector) {
  return selector.split(",").some((part) => {
    const sel = part.trim();
    if (!sel) return false;
    if (sel.startsWith(".")) return el.classList.contains(sel.slice(1));
    if (sel.startsWith("#")) return el.id === sel.slice(1);
    return el.tagName === sel.toUpperCase();
  });
}

class El {
  constructor(tag, page) {
    this.tagName = tag.toUpperCase();
    this.page = page;
    this.id = "";
    this.parentNode = null;
    this.children = [];
    this.listeners = new Map();
    this.classList = new ClassList();
    this.dataset = {};
    this.style = {};
    this.attributes = {};
    this.text = "";
    this.hidden = false;
    this.disabled = false;
    this.value = "";
    this.checked = false;
    this.files = [];
    this.open = false;
    this.clicked = false;
    this.clientWidth = 800;
    this.complete = true;
    this.naturalWidth = 8;
    this.paused = false;
  }

  get className() {
    return Array.from(this.classList.set).join(" ");
  }
  set className(v) {
    this.classList.set = new Set(String(v).split(/\s+/).filter(Boolean));
  }

  get textContent() {
    return this.text + this.children.map((c) => c.textContent).join("");
  }
  set textContent(v) {
    for (const c of this.children) c.parentNode = null;
    this.children = [];
    this.text = String(v);
  }

  get isConnected() {
    let root = this;
    while (root.parentNode) root = root.parentNode;
    const doc = this.page.document;
    return root === doc.head || root === doc.body || root === doc.documentElement;
  }

  addEventListener(type, fn) {
    const list = this.listeners.get(type) || [];
    list.push(fn);
    this.listeners.set(type, list);
  }

  async dispatch(type, event = {}) {
    const e = { type, target: this, preventDefault() {}, ...event };
    for (const fn of (this.listeners.get(type) || []).slice()) await fn(e);
  }

  click() {
    this.clicked = true;
    return this.dispatch("click", { type: "click" });
  }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
    if (name === "class") this.className = value;
  }
  getAttribute(name) {
    return Object.prototype.hasOwnProperty.call(this.attributes, name) ? this.attributes[name] : null;
  }

  appendChild(node) {
    node.remove();
    node.parentNode = this;
    this.children.push(node);
    return node;
  }
  append(...nodes) {
    for (const node of nodes) this.appendChild(node);
  }
  prepend(node) {
    node.remove();
    node.parentNode = this;
    this.children.unshift(node);
    return node;
  }
  replaceWith(node) {
    if (!this.parentNode) return;
    const i = this.parentNode.children.indexOf(this);
    node.remove();
    if (i >= 0) this.parentNode.children[i] = node;
    node.parentNode = this.parentNode;
    this.parentNode = null;
  }
  remove() {
    if (!this.parentNode) return;
    const i = this.parentNode.children.indexOf(this);
    if (i >= 0) this.parentNode.children.splice(i, 1);
    this.parentNode = null;
  }

  querySelector(selector) {
    for (const child of this.children) {
      if (matches(child, selector)) return child;
      const found = child.querySelector(selector);
      if (found) return found;
    }
    return null;
  }

  showModal() {
    this.open = true;
  }
  close() {
    this.open = false;
    this.dispatch("close", { type: "close" });
  }
  focus() { this.page.document.activeElement = this; }
  pause() { this.paused = true; }
  select() { this.selected = true; }
}

function makePage({ pathname = "/", key = "", listing = null, handler = null, session: store = null, storageThrows = false, clipboardApi = true, clock = null, script = APP, heicConvert = null } = {}) {
  const els = new Map();
  const calls = [];
  const docListeners = new Map();
  const session = store || new Map();
  const local = new Map();
  const clipboard = [];
  const page = { calls, els, session, local, clipboard, location: null, document: null };

  const location = {
    pathname,
    assigned: "",
    assign(url) {
      location.assigned = url;
    },
  };
  page.location = location;

  function el(id) {
    if (els.has(id)) return els.get(id);
    const e = new El("div", page);
    e.id = id;
    e.hidden = START_HIDDEN.has(id);
    els.set(id, e);
    return e;
  }

  const document = {
    head: new El("head", page),
    body: new El("body", page),
    documentElement: new El("html", page),
    getElementById: el,
    createElement: (tag) => {
      const node = new El(tag, page);
      if (tag === "img" && heicConvert) node.decode = async () => {
        if (node.src === "blob:image/heic") throw new Error("no native HEIC support");
      };
      return node;
    },
    addEventListener(type, fn) {
      const list = docListeners.get(type) || [];
      list.push(fn);
      docListeners.set(type, list);
    },
  };
  document.documentElement.dataset.theme = "light";
  document.activeElement = document.body;
  page.document = document;

  if (key) session.set("gallery-key", key);

  const storage = (map) => {
    if (storageThrows) {
      const denied = () => { throw new Error("storage blocked"); };
      return { getItem: denied, setItem: denied, removeItem: denied };
    }
    return {
      getItem: (k) => (map.has(k) ? map.get(k) : null),
      setItem: (k, v) => map.set(k, String(v)),
      removeItem: (k) => map.delete(k),
    };
  };

  const response = (status, body) => ({
    status,
    ok: status >= 200 && status < 300,
    statusText: "status " + status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  });

  const fetchImpl = async (url, opts = {}) => {
    calls.push({ url, opts });
    if ((url.endsWith("/photos") || url.includes("/photos?")) && opts.method === undefined && listing) {
      return response(200, listing);
    }
    if (handler) return handler(url, opts, calls);
    if (!BASE) throw new Error("live request without GALLY_BASE_URL: " + url);
    return globalThis.fetch(BASE + url, opts);
  };

  const context = {
    document,
    window: { innerHeight: 600, addEventListener() {} },
    navigator: clipboardApi
      ? { clipboard: { writeText: async (text) => { clipboard.push(String(text)); } } }
      : {},
    sessionStorage: storage(session),
    localStorage: storage(local),
    location,
    matchMedia: () => ({ matches: false }),
    fetch: fetchImpl,
    crypto: webcrypto,
    performance: clock || performance,
    setTimeout: clock ? clock.setTimeout : setTimeout,
    AbortController,
    AbortSignal,
    Blob,
    TextDecoder,
    Image: class {},
    URL: { createObjectURL: (blob) => "blob:" + blob.type, revokeObjectURL() {} },
    heic2any: heicConvert,
    IntersectionObserver: class {
      observe() {}
      disconnect() {}
    },
    console,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(script, context);

  async function fire(id, event) {
    const target = els.get(id);
    assert(target, "no element #" + id);
    const type = (event && event.type) || "click";
    assert((target.listeners.get(type) || []).length > 0, "no handler registered for #" + id);
    await target.dispatch(type, event || {});
  }

  page.fire = fire;
  // For a handler that opens a dialog and waits for the user: start it and
  // let the test drive the dialog. Awaiting would deadlock.
  page.start = (id, event = {}) => {
    const target = els.get(id);
    assert(target, "no element #" + id);
    const type = event.type || "click";
    assert((target.listeners.get(type) || []).length > 0, "no handler registered for #" + id);
    target.dispatch(type, event).catch((e) => {
      console.error(e);
      process.exit(1);
    });
  };
  page.fireEl = (target, type = "click", event = {}) => target.dispatch(type, event);
  page.fireDocument = async (type, event) => {
    const e = { type, preventDefault() {}, ...event };
    for (const fn of (docListeners.get(type) || []).slice()) await fn(e);
  };
  return page;
}

const tick = async () => {
  for (let i = 0; i < 5; i += 1) await new Promise((r) => setImmediate(r));
};

function virtualClock() {
  let now = 0;
  let timers = [];
  return {
    now: () => now,
    setTimeout: (fn, delay) => timers.push({ fn, at: now + delay }),
    async run(batch) {
      let settled = false;
      batch.then(() => { settled = true; }, () => { settled = true; });
      for (let i = 0; i < 50; i += 1) {
        await tick();
        if (settled) return batch;
        assert(timers.length, "batch is waiting without a timer");
        now = Math.min(...timers.map((timer) => timer.at));
        const ready = timers.filter((timer) => timer.at === now);
        timers = timers.filter((timer) => timer.at !== now);
        for (const timer of ready) timer.fn();
      }
      throw new Error("batch did not settle on the virtual clock");
    },
  };
}

async function waitFor(fn, what, ms = 10000) {
  const deadline = Date.now() + ms;
  for (;;) {
    if (fn()) return;
    if (Date.now() > deadline) throw new Error("timed out waiting for " + what);
    await new Promise((r) => setTimeout(r, 10));
  }
}

// --- real-server helpers (the workflows below) ---

async function mint(key) {
  const headers = { "Content-Type": "application/json", "X-Gallery-Key": key };
  if (INVITE) headers["X-Invite-Code"] = INVITE;
  const res = await globalThis.fetch(BASE + "/api/galleries/resolve", {
    method: "POST",
    headers,
    body: "{}",
  });
  assert.strictEqual(res.status, 200, "mint gallery");
  return (await res.json()).slug;
}

async function upload(slug, key, name, bytes, type) {
  const res = await globalThis.fetch(BASE + "/api/photos/" + encodeURIComponent(name), {
    method: "PUT",
    headers: { "X-Gallery-Key": key, "X-Gallery-Slug": slug, "Content-Type": type },
    body: bytes,
  });
  const status = res.status;
  await res.arrayBuffer();
  assert.strictEqual(status, 201, "upload " + name);
}

async function publish(slug, key, value) {
  const res = await globalThis.fetch(BASE + "/api/galleries/" + slug + "/visibility", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
    body: JSON.stringify({ public: value }),
  });
  const status = res.status;
  await res.arrayBuffer();
  assert.strictEqual(status, 200, "set visibility");
}

async function setLock(slug, key, password) {
  const res = await globalThis.fetch(BASE + "/api/galleries/" + slug + "/lock", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
    body: JSON.stringify({ password }),
  });
  const status = res.status;
  await res.arrayBuffer();
  assert.strictEqual(status, 200, "set lock");
}

async function galleryStatus(slug) {
  const res = await globalThis.fetch(BASE + "/api/galleries/" + slug + "/photos?limit=1");
  const status = res.status;
  await res.arrayBuffer();
  return status;
}

async function photoCount(slug, key) {
  const res = await globalThis.fetch(BASE + "/api/galleries/" + slug + "/photos?limit=200", {
    headers: { "X-Gallery-Key": key },
  });
  assert.strictEqual(res.status, 200, "owner listing");
  return (await res.json()).photos.length;
}

async function submitKey(page, value) {
  page.els.get("key-input").value = value;
  await page.fire("key-form", { type: "submit", submitter: { value: "default" }, preventDefault() {} });
}

// --- workflow 1: upload ---

async function workflowUpload() {
  const key = "Mint7!";
  const session = new Map();
  const home = makePage({ pathname: "/", session });

  await home.fire("upload-btn");
  assert.strictEqual(home.els.get("file-input").clicked, true, "Upload opens the picker");

  home.els.get("file-input").files = [
    new File([PNG], "first.png", { type: "image/png" }),
    new File([PNG], "second.png", { type: "image/png" }),
  ];
  const batch = home.fire("file-input", { type: "change" });

  await waitFor(() => home.els.get("key-dialog").open, "new-gallery key dialog");
  assert.match(home.els.get("key-title").textContent, /New gallery key/);

  await submitKey(home, "short");
  assert.match(home.els.get("key-error").textContent, /6-128/, "five characters are rejected");
  assert.strictEqual(home.location.assigned, "", "no navigation for a weak key");
  await submitKey(home, "mint7!");
  assert.match(home.els.get("key-error").textContent, /capital letter/);
  await submitKey(home, "Mint77");
  assert.match(home.els.get("key-error").textContent, /symbol/);

  await submitKey(home, key);
  if (INVITE) {
    await waitFor(() => home.els.get("key-dialog").open &&
      home.els.get("key-title").textContent === "Invite code", "live invite prompt");
    await submitKey(home, "wrong-code");
    await waitFor(() => /Incorrect invite code/.test(home.els.get("key-error").textContent), "live invite rejection");
    await submitKey(home, INVITE);
  }
  await batch;
  assert.match(home.location.assigned, /^\/[a-z]+-[a-z]+-[a-z]+-[a-z]+-[0-9a-f]{4}$/, "navigates to minted gallery");
  const slug = home.location.assigned.slice(1);
  assert.strictEqual(session.get("show-key:" + slug), key, "banner key survives navigation");
  assert.strictEqual(await photoCount(slug, key), 2, "both files uploaded to one gallery");

  const gallery = makePage({ pathname: "/" + slug, session });
  await waitFor(() => gallery.els.get("grid").children.length === 2, "2 tiles after navigation");
  assert.strictEqual(gallery.els.get("key-banner").hidden, false);
  assert.strictEqual(gallery.els.get("key-banner-key").textContent, key);
  assert.strictEqual(gallery.els.get("gallery-state").textContent, "Private", "private state is visible");
  await gallery.fire("key-banner-copy");
  assert.deepStrictEqual(gallery.clipboard, [key], "Copy key copies the key");
  await gallery.fire("key-banner-close");
  assert.strictEqual(gallery.els.get("key-banner").hidden, true, "banner dismisses");
  assert.strictEqual(gallery.els.get("gallery-label").textContent, slug);

  // Upload from the gallery page: the stored key is reused, no dialog.
  gallery.els.get("file-input").files = [new File([PNG], "third.png", { type: "image/png" })];
  await gallery.fire("file-input", { type: "change" });
  await waitFor(() => gallery.els.get("grid").children.length === 3, "tile appears after gallery upload");
  assert.strictEqual(gallery.location.assigned, "", "gallery upload stays put");
  assert.strictEqual(await photoCount(slug, key), 3);
}

async function testDelayedListingSharesUploadPrompt() {
  const key = "Shared-Key-42!";
  const slug = await mint(key);
  let releaseInitial;
  const initial = new Promise((resolve) => { releaseInitial = resolve; });
  let initialStatus;
  const page = makePage({
    pathname: "/" + slug,
    async handler(url, opts) {
      const res = await globalThis.fetch(BASE + url, opts);
      if (url.includes("/photos?") && !opts.headers["X-Gallery-Key"]) {
        initialStatus = res.status;
        await initial;
      }
      return res;
    },
  });
  await waitFor(() => initialStatus === 404, "private initial listing held in flight");
  page.els.get("file-input").files = [new File([PNG], "shared.png", { type: "image/png" })];
  const batch = page.fire("file-input", { type: "change" });
  await waitFor(() => page.els.get("key-dialog").open, "upload key prompt before listing returns");
  const input = page.els.get("key-input");
  input.value = key;
  releaseInitial();
  await tick();
  assert.strictEqual(input.value, key, "the late listing doesn't reset the shared prompt");
  await submitKey(page, input.value);
  await waitFor(() => !page.els.get("upload-btn").disabled, "upload settles after one key prompt");
  await batch;
  assert.strictEqual(page.els.get("upload-status").textContent, "Uploaded 1 photo.");
  assert.strictEqual(page.els.get("key-dialog").open, false);
  assert.strictEqual(await photoCount(slug, key), 1, "the real server received the upload");
  assert.strictEqual(page.els.get("grid").children.length, 1);
}

async function testVisibilityLostReply() {
  const key = "Visibility-Key-77!";
  const slug = await mint(key);
  let releaseStale;
  const stale = new Promise((resolve) => { releaseStale = resolve; });
  let releaseRecovery;
  const recovery = new Promise((resolve) => { releaseRecovery = resolve; });
  let loseWrite = false;
  let loseRead = false;
  let holdNextListing = false;
  let stalePublic;
  let recoveryStarted = false;
  const page = makePage({
    pathname: "/" + slug,
    key,
    async handler(url, opts) {
      const res = await globalThis.fetch(BASE + url, opts);
      if (loseWrite && url.endsWith("/visibility")) {
        assert.strictEqual(res.status, 200, "the server committed before the reply was lost");
        await res.arrayBuffer();
        throw new Error("lost visibility reply");
      }
      if (url.endsWith("/photos?limit=1")) {
        assert.strictEqual(opts.headers["X-Gallery-Key"], key, "visibility uses an owner read");
        assert.strictEqual(opts.cache, "no-store");
        if (loseRead) {
          await res.arrayBuffer();
          throw new Error("lost owner read");
        }
        recoveryStarted = true;
        await recovery;
      } else if (holdNextListing && url.includes("/photos?")) {
        holdNextListing = false;
        stalePublic = (await res.clone().json()).public;
        await stale;
      }
      return res;
    },
  });
  const state = page.els.get("gallery-state");
  const checkbox = page.els.get("public-check");
  const status = page.els.get("edit-status");
  await waitFor(() => state.textContent === "Private", "initial private visibility");

  // Delay a real listing requested by a completed upload until after the write.
  holdNextListing = true;
  page.els.get("file-input").files = [new File([PNG], "visibility.png", { type: "image/png" })];
  const batch = page.fire("file-input", { type: "change" });
  await waitFor(() => stalePublic === false, "pre-write private listing");
  await page.fire("edit-btn");
  loseWrite = true;
  checkbox.checked = true;
  const publishFromCheckbox = page.fire("public-check", { type: "change" });
  await waitFor(() => recoveryStarted, "owner reread after lost publish reply");
  assert.strictEqual(state.textContent, "Visibility unknown");
  assert(checkbox.indeterminate && checkbox.disabled);
  assert.strictEqual(status.textContent, "", "no confirmation before an authoritative reply");
  releaseRecovery();
  await publishFromCheckbox;
  assert.strictEqual(status.textContent, "", "confirmed visibility needs no success message");
  releaseStale();
  await batch;
  assert.strictEqual(state.textContent, "Public", "pre-write listing cannot revert visibility");
  assert(checkbox.checked && !checkbox.indeterminate && !checkbox.disabled);
  assert.strictEqual(await galleryStatus(slug), 200, "public state matches the real server");

  loseWrite = false;
  checkbox.checked = false;
  await page.fire("public-check", { type: "change" });
  await page.fire("lock-btn");
  loseWrite = true;
  loseRead = true;
  await page.fire("setpass-public");
  assert.strictEqual(await galleryStatus(slug), 200, "Make public also committed before losing its reply");
  assert.strictEqual(state.textContent, "Visibility unknown", "failed owner read cannot restore Private");
  assert(checkbox.indeterminate && checkbox.disabled);
  assert.strictEqual(status.textContent, "", "Protect cannot confirm an ambiguous write");
  assert.match(page.els.get("setpass-error").textContent, /Visibility unknown.*Reload/);
  assert.match(page.els.get("setpass-help").textContent, /Visibility unknown/);
}

async function testPasswordUnconfirmedReply() {
  const key = "Password-Reply-Key-99";
  const slug = await mint(key);
  const password = "Viewer-Password-88";
  await publish(slug, key, true);
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  let committed = false;
  let refreshedLocked;
  const page = makePage({
    pathname: "/" + slug,
    key,
    async handler(url, opts) {
      const res = await globalThis.fetch(BASE + url, opts);
      if (url.endsWith("/lock")) {
        assert.strictEqual(res.status, 200, "the server committed the password change");
        assert.deepStrictEqual(await res.json(), { slug, locked: true });
        committed = true;
        return new Response("{", { status: 200 });
      }
      if (committed && url.includes("/photos?")) {
        refreshedLocked = (await res.clone().json()).locked;
        await pending;
      }
      return res;
    },
  });
  const badge = page.els.get("gallery-state");
  const status = page.els.get("upload-status");
  const storageKey = "gallery-pass:" + slug;
  const message = "Password change was not confirmed. Try again.";
  await waitFor(() => badge.textContent === "Public", "initial unlocked badge");
  await page.fire("lock-btn");
  const dialog = page.els.get("setpass-dialog");
  page.els.get("setpass-input").value = password;
  const save = page.fire("setpass-form", { type: "submit", submitter: { value: "default" } });
  dialog.close(); // Native method=dialog closes before the async reply.
  await waitFor(() => refreshedLocked === true, "authoritative locked listing");
  assert.strictEqual(status.textContent, message, "corrupt acknowledgment never reports success");
  assert.strictEqual(badge.textContent, "Public", "wait for the authoritative listing");
  assert.strictEqual(page.session.has(storageKey), false);
  release();
  await save;
  assert.strictEqual(badge.textContent, "Public · Protected");
  assert.strictEqual(status.textContent, message);
  assert.strictEqual(page.session.has(storageKey), false, "listing cannot confirm the new password");
  assert.strictEqual(dialog.open, true);
  assert.strictEqual(page.els.get("setpass-error").textContent, message);
}

// --- workflow 2: edit ---

async function workflowEdit() {
  const key = "Edit-Gallery-77";
  const slug = await mint(key);
  for (let i = 0; i < 3; i += 1) {
    await upload(slug, key, "photo-" + i + ".png", PNG, "image/png");
  }
  await publish(slug, key, false);

  const page = makePage({ pathname: "/" + slug, key });
  await waitFor(() => page.els.get("grid").children.length === 3, "3 tiles");
  assert.strictEqual(page.els.get("gallery-state").textContent, "Private", "private gallery state");
  const tiles = () => page.els.get("grid").children;
  const sel = () => page.els.get("sel-count").textContent;

  await page.fire("edit-btn");
  assert.strictEqual(page.els.get("editbar").hidden, false, "edit bar opens");
  assert.strictEqual(page.els.get("upload-btn").hidden, true, "upload hidden while editing");
  assert.strictEqual(page.els.get("edit-btn").textContent, "Editing…");
  await page.fire("show-key-btn");
  assert.strictEqual(page.els.get("key-banner").hidden, false, "Show key reveals the banner");
  assert.strictEqual(page.els.get("key-banner-key").textContent, key);
  await page.fire("key-banner-close");

  await page.fireEl(tiles()[0]);
  assert.strictEqual(sel(), "1 selected");
  assert.strictEqual(page.els.get("delete-btn").disabled, false);
  await page.fireEl(tiles()[1]);
  assert.strictEqual(sel(), "2 selected");
  await page.fireEl(tiles()[1]);
  assert.strictEqual(sel(), "1 selected", "clicking a selected tile deselects it");

  assert.strictEqual(page.els.get("public-check").checked, false, "new galleries are private");
  page.els.get("public-check").checked = true;
  await page.fire("public-check", { type: "change" });
  assert.strictEqual(page.els.get("public-check").checked, true);
  assert.strictEqual(page.els.get("gallery-state").textContent, "Public", "public state updates");
  assert.strictEqual(await galleryStatus(slug), 200, "public gallery lists anonymously");

  page.els.get("public-check").checked = false;
  await page.fire("public-check", { type: "change" });
  assert.strictEqual(page.els.get("public-check").checked, false);
  assert.strictEqual(page.els.get("gallery-state").textContent, "Private", "private state updates");
  assert.strictEqual(await galleryStatus(slug), 404, "private gallery hides");

  page.els.get("public-check").checked = true;
  await page.fire("public-check", { type: "change" });

  await page.fire("lock-btn");
  assert.strictEqual(page.els.get("setpass-dialog").open, true);
  assert.strictEqual(page.els.get("setpass-remove").hidden, true, "unlocked gallery has nothing to remove");
  page.els.get("setpass-input").value = "Viewer-Pass-9";
  await page.fire("setpass-form", { type: "submit", submitter: { value: "default" } });
  assert.strictEqual(page.els.get("upload-status").textContent, "Gallery protected.");
  assert.strictEqual(page.els.get("gallery-state").textContent, "Public · Protected", "protected state");
  assert.strictEqual(await galleryStatus(slug), 401, "protected gallery asks for the password");

  await page.fire("lock-btn");
  assert.strictEqual(page.els.get("setpass-remove").hidden, false, "locked gallery offers Remove");
  await page.fire("setpass-form", { type: "submit", submitter: { value: "remove" } });
  assert.strictEqual(page.els.get("upload-status").textContent, "Protection removed.");
  assert.strictEqual(page.els.get("gallery-state").textContent, "Public", "state after unlock");
  assert.strictEqual(await galleryStatus(slug), 200);

  await page.fire("lock-btn");
  await page.fire("setpass-cancel");
  assert.strictEqual(page.els.get("setpass-dialog").open, false, "Protect dialog cancels");

  // Leave and re-enter edit mode to clear the selection.
  await page.fire("done-btn");
  assert.strictEqual(page.els.get("editbar").hidden, true);
  assert.strictEqual(page.els.get("upload-btn").hidden, false);
  await page.fire("edit-btn");
  assert.strictEqual(sel(), "0 selected");
  assert.strictEqual(page.els.get("delete-btn").disabled, true);

  await page.fireEl(tiles()[0]);
  await page.fire("delete-btn");
  assert.match(page.els.get("confirm-text").textContent, /Delete 1 photo\?/);
  await page.fire("confirm-form", { type: "submit", submitter: { value: "default" } });
  assert.strictEqual(await photoCount(slug, key), 2, "selected photo deleted");
  await waitFor(() => tiles().length === 2, "grid reloads after delete");

  await page.fireEl(tiles()[0]);
  await page.fire("delete-btn");
  await page.fire("confirm-form", { type: "submit", submitter: { value: "cancel" } });
  await tick();
  assert.strictEqual(await photoCount(slug, key), 2, "cancel keeps the photo");

  await page.fire("delete-gallery-btn");
  assert.match(page.els.get("confirm-text").textContent, /Delete this gallery and all its photos\?/);
  await page.fire("confirm-form", { type: "submit", submitter: { value: "default" } });
  assert.strictEqual(page.location.assigned, "/", "deleted gallery returns home");
  assert.strictEqual(await galleryStatus(slug), 404);
}

// --- workflow 3: view and mess around ---

async function testBoundedTextPreview() {
  const key = "Text-Preview-Key-99";
  const slug = await mint(key);
  const prefix = "x".repeat(65_536);
  const original = prefix + "not in preview";
  await upload(slug, key, "large.txt", Buffer.from(original), "text/plain");
  await upload(slug, key, "image.png", PNG, "image/png");

  let mode = "live";
  let reads = 0;
  let canceled = false;
  let release;
  const delayed = new Promise((resolve) => { release = resolve; });
  const page = makePage({
    pathname: "/" + slug,
    key,
    async handler(url, opts) {
      if (url.endsWith("_large.txt") && mode === "proxy") {
        return new Response(new ReadableStream({
          pull(controller) { reads += 1; controller.close(); },
          cancel() { canceled = true; },
        }, { highWaterMark: 0 }));
      }
      if (url.endsWith("_large.txt") && mode === "oversized") {
        return new Response(new TextEncoder().encode(original), { status: 206 });
      }
      if (url.endsWith("_large.txt") && mode === "delayed") {
        await delayed;
        throw new Error("delayed text request failed");
      }
      return globalThis.fetch(BASE + url, opts);
    },
  });
  const grid = page.els.get("grid");
  const viewport = page.els.get("viewport");
  await waitFor(() => grid.children.length === 2, "image and text tiles");
  const textTile = grid.children.find((tile) => tile.querySelector(".file-generic"));
  assert(textTile, "text tile");
  await page.fireEl(textTile);
  await waitFor(() => viewport.querySelector("pre"), "bounded live preview");
  assert.strictEqual(viewport.querySelector("pre").textContent, prefix);
  assert.match(viewport.textContent, /First 64 KiB/);
  const link = viewport.querySelector("a");
  assert.strictEqual(link.textContent, "Download original");
  const range = page.calls.find((call) => call.url === link.href);
  assert.strictEqual(range.opts.headers.Range, "bytes=0-65535");
  assert.strictEqual(range.opts.headers["X-Gallery-Key"], key);
  const download = await globalThis.fetch(BASE + link.href, { headers: { "X-Gallery-Key": key } });
  assert.strictEqual(download.status, 200);
  assert.strictEqual(await download.text(), original);

  await page.fire("close");
  mode = "proxy";
  await page.fireEl(textTile);
  await waitFor(() => viewport.querySelector("a"), "download-only fallback");
  assert.strictEqual(reads, 0, "never consume a full-body response");
  assert.strictEqual(canceled, true);
  assert.strictEqual(viewport.querySelector("pre"), null);
  assert.strictEqual(viewport.querySelector("a").href, link.href);

  await page.fire("close");
  mode = "oversized";
  await page.fireEl(textTile);
  await waitFor(() => viewport.querySelector("pre"), "oversized partial response");
  assert.strictEqual(viewport.querySelector("pre").textContent, prefix);
  assert.match(viewport.textContent, /First 64 KiB/);

  await page.fire("close");
  mode = "delayed";
  await page.fireEl(textTile);
  await page.fire("next");
  const image = viewport.querySelector("img");
  assert(image && image.src.endsWith("_image.png"));
  release();
  await tick();
  assert.strictEqual(viewport.querySelector("img"), image, "a late text failure cannot replace the image");
}

async function workflowView() {
  const key = "Viewer-Gallery-99";
  const slug = await mint(key);
  // 200 text files fill the first page; 3 images make sure photo viewing has
  // real media. Ordering is by created/name, so find an image tile below.
  for (let i = 0; i < 200; i += 1) {
    await upload(slug, key, "note-" + String(i).padStart(3, "0") + ".txt", Buffer.from("note " + i), "text/plain");
  }
  for (let i = 0; i < 3; i += 1) {
    await upload(slug, key, "photo-" + i + ".png", PNG, "image/png");
  }
  await publish(slug, key, true);

  const page = makePage({ pathname: "/" + slug });
  await waitFor(() => page.els.get("grid").children.length === 200, "first 200 photos");
  const tiles = () => page.els.get("grid").children;
  assert.strictEqual(page.els.get("load-more").hidden, false, "Load more appears");

  await page.fire("load-more");
  assert.strictEqual(tiles().length, 203, "Load more appends the rest");
  assert.strictEqual(page.els.get("load-more").hidden, true);

  const imageAt = tiles().findIndex((t) => t.querySelector("img"));
  assert(imageAt >= 0, "an image tile exists");
  const counter = page.els.get("counter");
  await page.fireEl(tiles()[imageAt]);
  assert.strictEqual(page.els.get("lightbox").open, true);
  assert.strictEqual(counter.textContent, `${imageAt + 1} / 203`);
  const shown = page.els.get("viewport").querySelector("img");
  assert(shown && shown.src.includes("/photos/") && shown.src.endsWith(".png"), "lightbox shows the original");

  await page.fire("next");
  assert.strictEqual(counter.textContent, `${((imageAt + 1) % 203) + 1} / 203`);
  await page.fire("prev");
  assert.strictEqual(counter.textContent, `${imageAt + 1} / 203`);

  await page.fire("close");
  assert.strictEqual(page.els.get("lightbox").open, false);

  await page.fireEl(tiles()[0]);
  await page.fire("prev");
  assert.strictEqual(counter.textContent, "203 / 203", "previous wraps to the last photo");
  await page.fireDocument("keydown", { key: "ArrowRight" });
  assert.strictEqual(counter.textContent, "1 / 203");
  await page.fireDocument("keydown", { key: "Escape" });
  assert.strictEqual(page.els.get("lightbox").open, false, "Escape closes the viewer");

  await page.fire("theme-btn");
  assert.strictEqual(page.document.documentElement.dataset.theme, "dark");
  assert.strictEqual(page.local.get("theme"), "dark");
  await page.fire("theme-btn");
  assert.strictEqual(page.document.documentElement.dataset.theme, "light");

  // Edit without a stored key: the key dialog prompts, Esc cancels.
  page.start("edit-btn");
  await waitFor(() => page.els.get("key-dialog").open, "key prompt for editing");
  assert.match(page.els.get("key-title").textContent, /Gallery key/);
  page.els.get("key-dialog").close();
  await tick();
  assert.strictEqual(page.els.get("editbar").hidden, true, "cancelled edit stays closed");

  page.start("edit-btn");
  await waitFor(() => page.els.get("key-dialog").open, "key prompt for editing");
  await submitKey(page, key);
  await waitFor(() => page.els.get("editbar").hidden === false, "owner key opens edit mode");
  await page.fire("done-btn");

  // Locked gallery: pass dialog cancel, then unlock with the password.
  const pass = "Viewer-Pass-9-é";
  await setLock(slug, key, pass);
  const locked = makePage({ pathname: "/" + slug });
  await waitFor(() => locked.els.get("pass-dialog").open, "password prompt");
  await locked.fire("pass-cancel");
  await waitFor(
    () => locked.els.get("empty").textContent === "This gallery is locked.",
    "locked gallery message"
  );
  assert.strictEqual(locked.els.get("gallery-state").textContent, "Public · Protected", "locked state");
  assert.strictEqual(locked.els.get("grid").children.length, 0);

  let viewerCookie = "";
  const unlocking = makePage({
    pathname: "/" + slug,
    handler: async (url, opts) => {
      const res = await globalThis.fetch(BASE + url, opts);
      const cookie = res.headers.get("Set-Cookie");
      if (cookie) viewerCookie = cookie.split(";")[0];
      return res;
    },
  });
  const field = "gallery-pass:" + slug;
  await waitFor(() => unlocking.els.get("pass-dialog").open, "password prompt");
  unlocking.els.get("pass-input").value = "wrong-password";
  await unlocking.fire("pass-form", { type: "submit" });
  assert.strictEqual(unlocking.els.get("pass-dialog").open, true, "wrong password keeps the prompt open");
  assert.match(unlocking.els.get("pass-error").textContent, /Incorrect password/);
  assert.strictEqual(unlocking.session.has(field), false, "incorrect password is not saved");
  assert.strictEqual(unlocking.els.get("grid").children.length, 0);

  unlocking.els.get("pass-input").value = pass;
  const check = unlocking.fire("pass-form", { type: "submit" });
  assert.strictEqual(unlocking.els.get("pass-dialog").open, true, "prompt stays open during the request");
  assert.strictEqual(unlocking.session.has(field), false, "pending password is not saved");
  await check;
  await waitFor(() => unlocking.els.get("grid").children.length === 200, "unlocked listing");
  assert.strictEqual(unlocking.els.get("pass-dialog").open, false);
  assert.strictEqual(unlocking.session.get(field), pass);
  assert.strictEqual(unlocking.calls.length, 3, "renders the listing that accepted the password");
  assert(viewerCookie, "the accepted listing issues a media cookie");
  const res = await globalThis.fetch(BASE + shown.src, { headers: { Cookie: viewerCookie } });
  assert.strictEqual(res.status, 200, "the viewer cookie authorizes the original");
  assert.deepStrictEqual(Buffer.from(await res.arrayBuffer()), PNG);

  const ownerAsViewer = makePage({ pathname: "/" + slug });
  await waitFor(() => ownerAsViewer.els.get("pass-dialog").open, "owner-as-viewer prompt");
  ownerAsViewer.els.get("pass-input").value = key;
  await ownerAsViewer.fire("pass-form", { type: "submit" });
  await waitFor(() => ownerAsViewer.els.get("grid").children.length === 200, "owner key works in viewer prompt");

  // Private gallery without a key: prompt, cancel, stay on an explained page.
  await publish(slug, key, false);
  const hidden = makePage({ pathname: "/" + slug });
  await waitFor(() => hidden.els.get("key-dialog").open, "key prompt on a private gallery");
  await hidden.fire("key-cancel");
  await waitFor(
    () => /private or does not exist/.test(hidden.els.get("empty").textContent),
    "unavailable message"
  );
  assert.strictEqual(hidden.location.assigned, "", "cancel does not redirect");
  assert.strictEqual(hidden.els.get("edit-btn").hidden, true, "edit hidden when unavailable");
  assert.strictEqual(hidden.els.get("home-link").hidden, false, "home link remains");
  await testLoadMoreDuringUploadRefresh(slug, key);
}

async function testLoadMoreDuringUploadRefresh(slug, key) {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const firstPages = [];
  const morePages = [];
  const page = makePage({
    pathname: "/" + slug,
    key,
    async handler(url, opts) {
      const res = await globalThis.fetch(BASE + url, opts);
      if (url.includes("/photos?")) {
        const data = await res.clone().json();
        const cursor = new URL(BASE + url).searchParams.get("before");
        if (cursor) {
          morePages.push({ cursor, data });
          if (morePages.length === 1) await pending;
        } else {
          firstPages.push(data);
        }
      }
      return res;
    },
  });
  const names = () => page.els.get("grid").children.map((tile) => (
    tile.querySelector("img")?.alt || tile.querySelector(".file-generic").textContent
  ));
  const display = (photo) => photo.name.split("_").slice(2).join("_");
  await waitFor(() => !page.els.get("load-more").hidden, "initial page cursor");
  const oldRequest = page.fire("load-more");
  await waitFor(() => morePages.length === 1, "old page held after server reply");
  assert.strictEqual(morePages[0].cursor, firstPages[0].next);
  page.els.get("file-input").files = [new File(["new"], "refresh.txt", { type: "text/plain" })];
  await page.fire("file-input", { type: "change" });
  assert.strictEqual(firstPages.length, 2, "completed upload refreshes the first page");
  const refreshed = firstPages[1];
  assert.notStrictEqual(refreshed.next, firstPages[0].next);
  assert(names().includes("refresh.txt"));
  release();
  await oldRequest;
  assert.deepStrictEqual(names(), refreshed.photos.map(display), "old page did not append");
  assert.strictEqual(page.els.get("load-more").hidden, false, "fresh cursor remains available");
  await page.fire("load-more");
  assert.strictEqual(morePages[1].cursor, refreshed.next);
  assert.deepStrictEqual(names(), [...refreshed.photos, ...morePages[1].data.photos].map(display));
  assert.strictEqual(new Set(names()).size, 204, "only fresh names were appended");
}

// --- legacy stub-DOM checks ---

const SLUG = "velvet-lagoon-quiet-river-1a2b";
const KEY = "Sunset-42-Key!";

function galleryPage(locked) {
  return makePage({
    pathname: "/" + SLUG,
    key: KEY,
    listing: { slug: SLUG, public: true, locked, photos: [] },
    handler: () => {
      throw new Error("unexpected request");
    },
  });
}

async function testBlockedStorageStillLoads() {
  // A browser that blocks site data throws from sessionStorage. The page must
  // still render and keep its controls working.
  const page = makePage({ pathname: "/", storageThrows: true });
  await tick();
  assert.strictEqual(page.els.get("upload-btn").hidden, false);
  assert.strictEqual(page.els.get("key-dialog").open, false);
}

async function testLightboxFocusAndLinkCleanup() {
  const page = makePage({
    pathname: "/" + SLUG,
    listing: {
      slug: SLUG,
      public: true,
      locked: false,
      photos: [{ name: SLUG + "_" + "a".repeat(32) + "_file.pdf" }],
    },
  });
  await tick();
  const tile = page.els.get("grid").children[0];
  assert(tile, "generic file tile");
  tile.focus();
  await page.fireEl(tile);
  assert.strictEqual(page.els.get("lightbox").open, true);
  assert.strictEqual(page.document.activeElement.id, "close", "close button gets focus");
  const generic = page.els.get("viewport").querySelector(".file-generic");
  assert(generic && generic.querySelector("a"), "generic file offers download");
  await page.fire("close");
  assert.strictEqual(page.els.get("lightbox").open, false);
  assert.strictEqual(page.document.activeElement, tile, "focus returns to the tile");
  assert.strictEqual(tile.querySelector("a"), null, "download link stays out of the tile");
}

async function testLightboxPausesAudio() {
  const page = makePage({
    pathname: "/" + SLUG,
    listing: {
      slug: SLUG,
      public: true,
      locked: false,
      photos: [{ name: SLUG + "_" + "b".repeat(32) + "_sound.mp3" }],
    },
  });
  await tick();
  const tile = page.els.get("grid").children[0];
  await page.fireEl(tile);
  const audio = page.els.get("viewport").querySelector("audio");
  assert(audio, "audio player");
  assert.strictEqual(audio.paused, false);
  await page.fire("close");
  assert.strictEqual(audio.paused, true, "audio pauses on close");
}

function testAccessibleMarkup() {
  const index = fs.readFileSync(path.join(__dirname, "..", "index.html"), "utf8");
  assert.match(index, /id="key-input"[^>]*aria-labelledby="key-title"/);
  assert.match(index, /id="key-input"[^>]*aria-describedby="key-help"/);
  assert.match(index, /id="setpass-input"[^>]*aria-label=/);
  assert.match(index, /<dialog id="lightbox"/);
  const admin = fs.readFileSync(path.join(__dirname, "..", "admin.html"), "utf8");
  assert.match(admin, /id="password"[^>]*aria-label=/);
  assert.match(admin, /id="load-more-galleries"/);
}

async function testButtonsWithStoredKey() {
  // Regression: KEY_MIN_LEN was undefined, so every one of these threw.
  const page = galleryPage(false);
  await tick();
  await page.fire("edit-btn");
  await page.fire("lock-btn");
}

async function testProtectReflectsLock() {
  const lockedPage = galleryPage(true);
  await tick();
  await lockedPage.fire("lock-btn");
  assert.strictEqual(lockedPage.els.get("setpass-remove").hidden, false, "locked gallery offers Remove");

  const openPage = galleryPage(false);
  await tick();
  await openPage.fire("lock-btn");
  assert.strictEqual(openPage.els.get("setpass-remove").hidden, true, "unlocked gallery hides Remove");

  // Enter in #setpass-input must submit Save, never Remove: browsers use the
  // first submit button in tree order as the default submitter.
  const html = fs.readFileSync(path.join(__dirname, "..", "index.html"), "utf8");
  const formStart = html.indexOf('id="setpass-form"');
  const form = html.slice(formStart, html.indexOf("</form>", formStart));
  const submits = [...form.matchAll(/<button[^>]*type="submit"[^>]*>/g)].map((m) => m[0]);
  assert.strictEqual(submits.length, 2, "setpass-form has Save and Remove");
  assert.match(submits[0], /id="setpass-confirm"/, "Enter submits Save");
  assert.match(submits[0], /value="default"/);
  assert.match(submits[1], /id="setpass-remove"/);

  // Implicit Enter (browsers may omit `submitter`) must send the typed
  // password, not the remove action's empty one.
  let sent = null;
  const enterPage = makePage({
    pathname: "/" + SLUG,
    key: KEY,
    listing: { slug: SLUG, public: true, locked: true, photos: [] },
    handler: (url, opts) => {
      if (url.endsWith("/lock")) {
        sent = JSON.parse(opts.body);
        return {
          status: 200,
          ok: true,
          statusText: "status 200",
          json: async () => ({ slug: SLUG, locked: true }),
        };
      }
      throw new Error("unexpected request " + url);
    },
  });
  await tick();
  await enterPage.fire("lock-btn");
  enterPage.els.get("setpass-input").value = "Viewer-Pass-9";
  await enterPage.fire("setpass-form", { type: "submit" });
  assert.deepStrictEqual(sent, { password: "Viewer-Pass-9" }, "Enter saves the new password");
}

async function testNoHeicNeverLoadsDecoder() {
  // The 1.3 MB decoder is injected on demand; a HEIC-free gallery must leave
  // the <head> untouched.
  const page = makePage({
    pathname: "/" + SLUG,
    key: KEY,
    listing: {
      slug: SLUG,
      public: true,
      locked: false,
      photos: [{ name: "20260101_120000_kitten.jpg" }],
    },
    handler: () => {
      throw new Error("unexpected request");
    },
  });
  await tick();
  assert.strictEqual(page.document.head.children.length, 0, "no HEIC: decoder script not injected");
}

async function testHeicFetchesOnlyWhenVisible() {
  // The stub's IntersectionObserver never fires, so an off-screen HEIC tile
  // must not hit the network until it intersects.
  const page = makePage({
    pathname: "/" + SLUG,
    key: KEY,
    listing: {
      slug: SLUG,
      public: true,
      locked: false,
      photos: [{ name: "20260101_120000_kitten.heic" }],
    },
    handler: () => {
      throw new Error("unexpected request");
    },
  });
  await tick();
  assert(
    !page.calls.some((c) => c.url.includes(".heic")),
    "off-screen HEIC is not fetched before the observer fires"
  );
}

async function testHeicBudgetAndFallback() {
  const names = ["first.heic", "second.heic", "oversized.heic", "unknown-length.heic"];
  const cap = 8 * 1024 * 1024;
  let finish;
  const held = new Promise((resolve) => { finish = resolve; });
  let conversions = 0;
  let declaredReads = 0;
  const canceled = [];
  const page = makePage({
    pathname: "/" + SLUG,
    key: KEY,
    listing: { slug: SLUG, public: false, locked: false, photos: names.map((name) => ({ name })) },
    async heicConvert({ blob }) {
      assert.strictEqual(blob.size, 4, "only small originals reach the decoder");
      conversions += 1;
      if (conversions === 1) await held;
      return new Blob(["jpeg"], { type: "image/jpeg" });
    },
    handler(url) {
      if (url.endsWith("/oversized.heic")) {
        return new Response(new ReadableStream({
          pull() { declaredReads += 1; },
          cancel() { canceled.push("declared"); },
        }, { highWaterMark: 0 }), { headers: { "Content-Length": String(cap + 1) } });
      }
      if (url.endsWith("/unknown-length.heic")) {
        return new Response(new ReadableStream({
          start(controller) {
            controller.enqueue(new Uint8Array(cap));
            controller.enqueue(new Uint8Array(1));
          },
          cancel() { canceled.push("streamed"); },
        }));
      }
      return new Response("heic");
    },
  });
  const grid = page.els.get("grid");
  const viewport = page.els.get("viewport");
  page.document.body.append(grid, viewport);
  await waitFor(() => grid.children.length === 4, "HEIC tiles");
  await page.fireEl(grid.children[0]);
  await waitFor(() => page.document.head.children.length, "fallback decoder loading");
  page.document.head.children[0].onload();
  await waitFor(() => conversions === 1, "first conversion held");
  await page.fire("next");
  await tick();
  assert(!page.calls.some(({ url }) => url.endsWith("/second.heic")), "next fetch waits for the active conversion");
  finish();
  await waitFor(() => viewport.querySelector("img")?.src === "blob:image/jpeg", "second preview displayed");
  assert.strictEqual(conversions, 2);
  assert.strictEqual(viewport.querySelector("img").alt, "second.heic");

  for (const [index, reason] of [[2, "declared"], [3, "streamed"]]) {
    await page.fire("next");
    await waitFor(() => viewport.querySelector("a"), "oversized original remains downloadable");
    const link = viewport.querySelector("a");
    assert.strictEqual(link.textContent, "Download original");
    assert.strictEqual(link.href, "/photos/" + names[index]);
    assert.match(viewport.textContent, new RegExp(names[index]));
    assert(canceled.includes(reason));
  }
  assert.strictEqual(declaredReads, 0);
  assert.strictEqual(conversions, 2, "neither oversized response is decoded");
}

function homePage(uploadHandler, clock) {
  return makePage({
    pathname: "/",
    handler: uploadHandler,
    clock,
  });
}

const filed = (name, size = 10) => ({
  name,
  type: "image/jpeg",
  size,
  slice(start, end) {
    return { size: end - start };
  },
});

async function startBatch(page, files) {
  page.els.get("file-input").files = files;
  const done = page.fire("file-input", { type: "change" });
  await tick();
  // Home uploads always prompt: answer the key dialog.
  await submitKey(page, "Home-Key-1234");
  await done;
}

async function testHomeUploadUsesOneSlug() {
  const puts = [];
  const page = homePage((url, opts) => {
    if (url === "/api/galleries/resolve") return { status: 200, ok: true, json: async () => ({ slug: SLUG }) };
    if (opts.method === "PUT") {
      puts.push(opts);
      return { status: 201, ok: true, json: async () => ({ name: "x", slug: SLUG }) };
    }
    throw new Error("unexpected " + url);
  });
  await startBatch(page, [filed("a.jpg"), filed("b.jpg")]);
  assert.strictEqual(puts.length, 2, "both files uploaded");
  for (const p of puts) assert.strictEqual(p.headers["X-Gallery-Slug"], SLUG, "batch targets the minted slug");
  assert.strictEqual(page.location.assigned, "/" + SLUG, "navigates to the batch gallery");
}

async function testHomeUploadReportsPartialFailure() {
  const session = new Map();
  const page = makePage({
    pathname: "/",
    session,
    handler: (url, opts) => {
      if (url === "/api/galleries/resolve") {
        return { status: 200, ok: true, json: async () => ({ slug: SLUG }) };
      }
      if (opts.method === "PUT" && url.endsWith("/good.jpg")) {
        return { status: 201, ok: true, json: async () => ({ name: "x", slug: SLUG }) };
      }
      if (opts.method === "PUT") {
        return {
          status: 413,
          ok: false,
          statusText: "status 413",
          json: async () => ({ error: "file too large" }),
        };
      }
      throw new Error("unexpected " + url);
    },
  });
  await startBatch(page, [filed("good.jpg"), filed("bad.jpg")]);
  assert.strictEqual(page.location.assigned, "/" + SLUG);
  assert.match(session.get("upload-failed:" + SLUG), /bad\.jpg: file too large/);

  const gallery = makePage({
    pathname: "/" + SLUG,
    session,
    listing: { slug: SLUG, public: false, locked: false, photos: [{ name: "x" }] },
  });
  await tick();
  assert.match(gallery.els.get("upload-status").textContent, /bad\.jpg: file too large/);
  assert.strictEqual(session.has("upload-failed:" + SLUG), false, "failure note is one-shot");
}

async function testLostReplyDoesNotMintAgain() {
  let putCount = 0;
  const puts = [];
  const resolves = [];
  const page = homePage((url, opts) => {
    if (url === "/api/galleries/resolve") {
      resolves.push(opts);
      return { status: 200, ok: true, json: async () => ({ slug: SLUG }) };
    }
    if (opts.method === "PUT") {
      puts.push({ url, headers: opts.headers });
      putCount += 1;
      if (putCount === 1) throw new TypeError("network lost");
      return { status: 201, ok: true, json: async () => ({ name: "x", slug: SLUG }) };
    }
    throw new Error("unexpected " + url);
  });
  await startBatch(page, [filed("a.jpg"), filed("b.jpg")]);
  assert.strictEqual(resolves.length, 1, "mints exactly one gallery per batch");
  assert(puts.length >= 2, "the retried chunk and the next file were uploaded");
  // The failed chunk was retried with the same id and range, so the server
  // can resume instead of restarting the file.
  const failedFile = puts.filter((p) => p.url.endsWith("/a.jpg"));
  assert.strictEqual(failedFile.length, 2, "a.jpg retried once");
  assert.strictEqual(failedFile[0].headers["Content-Range"], failedFile[1].headers["Content-Range"]);
  assert.strictEqual(failedFile[0].headers["X-Upload-Id"], failedFile[1].headers["X-Upload-Id"]);
  for (const p of puts) {
    assert.strictEqual(p.headers["X-Gallery-Slug"], SLUG, "every file keeps the batch slug");
  }
  assert.strictEqual(page.location.assigned, "/" + SLUG);
}

async function testUploadRetriesLostChunk() {
  let putCount = 0;
  const puts = [];
  const page = homePage((url, opts) => {
    if (url === "/api/galleries/resolve") return { status: 200, ok: true, json: async () => ({ slug: SLUG }) };
    if (opts.method === "PUT") {
      puts.push(opts);
      putCount += 1;
      if (putCount === 1) throw new TypeError("network lost");
      return { status: 201, ok: true, json: async () => ({ name: "x", slug: SLUG }) };
    }
    throw new Error("unexpected " + url);
  });
  await startBatch(page, [filed("a.jpg")]);
  assert.strictEqual(puts.length, 2, "the lost chunk is retried");
  assert.strictEqual(puts[0].headers["Content-Range"], "bytes 0-9/10");
  assert.strictEqual(puts[1].headers["Content-Range"], puts[0].headers["Content-Range"]);
  assert.strictEqual(puts[1].headers["X-Upload-Id"], puts[0].headers["X-Upload-Id"]);
  assert.strictEqual(page.location.assigned, "/" + SLUG, "the retried upload still lands");
}

const UPLOAD_CHUNK = 8 * 1024 * 1024;

async function testWrongInviteCodeIsAskedAgain() {
  const resolves = [];
  const page = homePage((url, opts) => {
    if (url === "/api/galleries/resolve") {
      const code = opts.headers["X-Invite-Code"] || "";
      resolves.push(code);
      if (code === "good-code") return { status: 200, ok: true, json: async () => ({ slug: SLUG }) };
      // A cached wrong code would loop forever; fail loudly instead.
      if (resolves.length > 2) throw new Error("rejected invite code re-sent");
      return {
        status: 403,
        ok: false,
        statusText: "status 403",
        json: async () => ({ error: "invite required" }),
      };
    }
    if (opts.method === "PUT") {
      return { status: 201, ok: true, json: async () => ({ name: "x", slug: SLUG }) };
    }
    throw new Error("unexpected " + url);
  });
  page.els.get("file-input").files = [filed("a.jpg")];
  const done = page.fire("file-input", { type: "change" });
  await tick();
  await submitKey(page, "Home-Key-1234");
  await waitFor(
    () => resolves.length === 1 && /Invite code/.test(page.els.get("key-title").textContent),
    "invite prompt"
  );
  assert.strictEqual(resolves[0], "", "the first mint has no invite header");

  page.els.get("key-input").value = "wrong-code";
  await page.fire("key-form", { type: "submit", submitter: { value: "default" }, preventDefault() {} });
  await waitFor(() => resolves.length === 2, "second mint with the wrong code");
  assert.strictEqual(resolves[1], "wrong-code");
  // The rejected code is cleared and the dialog asks again instead of
  // re-sending it in a tight loop.
  await tick();
  assert.strictEqual(resolves.length, 2, "the wrong code is not re-sent");
  assert.strictEqual(page.els.get("key-dialog").open, true);
  assert.match(page.els.get("key-title").textContent, /Invite code/);
  assert.match(page.els.get("key-error").textContent, /Incorrect invite code/);

  page.els.get("key-input").value = "good-code";
  await page.fire("key-form", { type: "submit", submitter: { value: "default" }, preventDefault() {} });
  await done;
  assert.strictEqual(resolves.length, 3);
  assert.strictEqual(resolves[2], "good-code");
  assert.strictEqual(page.location.assigned, "/" + SLUG);
}

async function testEmptyPasswordKeepsDialogOpen() {
  const page = makePage({
    pathname: "/" + SLUG,
    handler: (url) => {
      if (url.startsWith("/api/galleries/" + SLUG + "/photos")) {
        return {
          status: 401,
          ok: false,
          statusText: "status 401",
          json: async () => ({ error: "locked" }),
        };
      }
      throw new Error("unexpected " + url);
    },
  });
  await waitFor(() => page.els.get("pass-dialog").open, "password prompt");
  let prevented = false;
  await page.fire("pass-form", {
    type: "submit",
    submitter: { value: "unlock" },
    preventDefault() { prevented = true; },
  });
  assert.strictEqual(prevented, true, "empty submit is not the default dialog close");
  assert.strictEqual(page.els.get("pass-dialog").open, true, "dialog stays open");
  assert.match(page.els.get("pass-error").textContent, /Enter the gallery password/);
  assert.strictEqual(page.session.get("gallery-pass:" + SLUG), undefined);
}

async function testPasswordCheckErrorsAndCancel() {
  let finish;
  const page = makePage({
    pathname: "/" + SLUG,
    handler: (_url, { headers }) => {
      const pass = headers["X-Gallery-Pass"];
      if (!pass) return new Response(null, { status: 401 });
      if (pass === "rate") return new Response(null, { status: 429 });
      if (pass === "network") throw new TypeError("network down");
      if (pass === "empty") return Response.json({});
      return new Promise((resolve) => { finish = resolve; });
    },
  });
  await waitFor(() => page.els.get("pass-dialog").open, "password prompt");
  for (const [value, message] of [["rate", /Too many attempts/], ["network", /Check your connection/], ["empty", /Could not check the password\. Try again\./]]) {
    page.els.get("pass-input").value = value;
    await page.fire("pass-form", { type: "submit" });
    assert.match(page.els.get("pass-error").textContent, message);
    assert.strictEqual(page.els.get("pass-dialog").open, true);
    assert.strictEqual(page.els.get("pass-input").value, value);
    assert.strictEqual(page.session.has("gallery-pass:" + SLUG), false);
  }

  page.els.get("pass-input").value = "pending";
  const check = page.fire("pass-form", { type: "submit" });
  const signal = page.calls.at(-1).opts.signal;
  assert.strictEqual(page.els.get("pass-confirm").disabled, true);
  await page.fire("pass-cancel");
  assert.strictEqual(signal.aborted, true);
  finish(Response.json({ slug: SLUG, photos: [{ name: "would-have-rendered.png" }] }));
  await check;
  assert.strictEqual(page.session.has("gallery-pass:" + SLUG), false, "late success after cancelling is ignored");
  assert.strictEqual(page.els.get("grid").children.length, 0);
}

async function testAdminLoadErrorIsVisible() {
  const html = fs.readFileSync(path.join(__dirname, "..", "admin.html"), "utf8");
  const page = makePage({
    script: html.match(/<script>([\s\S]*?)<\/script>/)[1],
    handler: (url) => {
      if (url.endsWith("/unlock")) return Response.json({ ok: true });
      if (url.includes("/api/admin/galleries")) {
        return Response.json({ error: "database unavailable" }, { status: 500 });
      }
      throw new Error("unexpected " + url);
    },
  });
  page.els.get("password").value = "admin-password";
  await page.fire("unlock");
  assert.strictEqual(page.els.get("login").hidden, true);
  assert.strictEqual(page.els.get("panel").hidden, false);
  assert.match(page.els.get("panel-error").textContent, /database unavailable/);
}

async function testAdminLogout() {
  const html = fs.readFileSync(path.join(__dirname, "..", "admin.html"), "utf8");
  let logout;
  const page = makePage({
    script: html.match(/<script>([\s\S]*?)<\/script>/)[1],
    handler: (url) => {
      if (url.endsWith("/unlock")) return Response.json({ ok: true });
      if (url.includes("/api/admin/galleries")) {
        return Response.json({ galleries: [{ slug: SLUG, photos: 1 }], next: null });
      }
      assert(url.endsWith("/logout"));
      return logout();
    },
  });
  for (const result of [{ status: 200, body: { ok: true } }, { status: 403, body: { error: "admin session inactive" } }]) {
    page.els.get("password").value = "admin-password";
    await page.fire("unlock");
    for (const failure of [
      () => { throw new TypeError("network down"); },
      () => Response.json({ error: "internal" }, { status: 500 }),
      () => new Response("Forbidden by proxy", { status: 403 }),
      () => Response.json({ error: "forbidden" }, { status: 403 }),
    ]) {
      logout = failure;
      await page.fire("logout");
      assert.strictEqual(page.els.get("panel").hidden, false);
      assert.strictEqual(page.els.get("login").hidden, true);
      assert.match(page.els.get("galleries").textContent, new RegExp(SLUG));
      assert.match(page.els.get("panel-error").textContent, /Could not confirm logout.*try again/);
    }
    logout = () => Response.json(result.body, { status: result.status });
    await page.fire("logout");
    assert.strictEqual(page.els.get("panel").hidden, true);
    assert.strictEqual(page.els.get("login").hidden, false);
    assert.strictEqual(page.els.get("password").value, "");
  }
}

async function testRejectedFilesDoNotMintGallery() {
  const page = homePage(() => {
    throw new Error("unexpected request");
  });
  page.els.get("file-input").files = [
    filed("empty.jpg", 0),
    filed("huge.jpg", 11 * 1024 * 1024 * 1024),
  ];
  await page.fire("file-input", { type: "change" });
  assert.strictEqual(page.calls.length, 0, "nothing is minted or sent for a doomed batch");
  assert.strictEqual(page.els.get("key-dialog").open, false, "no key prompt without an upload");
  const status = page.els.get("upload-status").textContent;
  assert.match(status, /empty\.jpg: empty file/);
  assert.match(status, /huge\.jpg: file is too large/);
}

async function testUploadResumesFromServerOffset() {
  const chunks = 7;
  const size = chunks * UPLOAD_CHUNK + 10;
  const clock = virtualClock();
  const puts = [];
  let committed = 0;
  const page = homePage((url, opts) => {
    if (url === "/api/galleries/resolve") return Response.json({ slug: SLUG });
    puts.push(opts.headers);
    const [, start, end] = opts.headers["Content-Range"].match(/^bytes (\d+)-(\d+)\//);
    if (Number(start) !== committed) {
      return Response.json({ error: "offset mismatch", received: committed }, { status: 409 });
    }
    committed = Number(end) + 1;
    if (committed < size) throw new TypeError("202 response lost");
    return Response.json({ ok: true }, { status: 201 });
  }, clock);
  await clock.run(startBatch(page, [filed("big.jpg", size)]));
  assert.strictEqual(puts.length, chunks * 2 + 1, "each lost reply is reconciled");
  assert.strictEqual(new Set(puts.map((p) => p["X-Upload-Id"])).size, 1);
  assert.strictEqual(puts.at(-1)["Content-Range"], `bytes ${chunks * UPLOAD_CHUNK}-${size - 1}/${size}`);
  assert.strictEqual(page.location.assigned, "/" + SLUG);
}

async function testUploadBusyAndInvalidOffsets() {
  const size = UPLOAD_CHUNK + 10;
  const clock = virtualClock();
  const replies = [
    Response.json({ error: "upload in progress", received: size }, { status: 409 }),
    Response.json({ received: UPLOAD_CHUNK }, { status: 202 }),
    Response.json({ error: "offset mismatch", received: size }, { status: 409 }),
    Response.json({ received: size }, { status: 202 }),
    Response.json({ error: "offset mismatch", received: UPLOAD_CHUNK - 5 }, { status: 409 }),
    Response.json({ ok: true }, { status: 201 }),
  ];
  const puts = [];
  const page = homePage((url, opts) => {
    if (url === "/api/galleries/resolve") return Response.json({ slug: SLUG });
    puts.push(opts.headers);
    return replies.shift();
  }, clock);
  await clock.run(startBatch(page, [filed("big.jpg", size)]));
  const first = `bytes 0-${UPLOAD_CHUNK - 1}/${size}`;
  const last = `bytes ${UPLOAD_CHUNK}-${size - 1}/${size}`;
  assert.deepStrictEqual(puts.map((p) => p["Content-Range"]), [first, first, last, last, last, `bytes ${UPLOAD_CHUNK - 5}-${size - 1}/${size}`]);
  assert.strictEqual(new Set(puts.map((p) => p["X-Upload-Id"])).size, 1);
  assert.strictEqual(replies.length, 0, "the final 201 was reached");
  assert.strictEqual(page.location.assigned, "/" + SLUG);

  const badClock = virtualClock();
  let attempts = 0;
  const bad = homePage((url) => {
    if (url === "/api/galleries/resolve") return Response.json({ slug: SLUG });
    attempts += 1;
    return Response.json({ received: 10 }, { status: 202 });
  }, badClock);
  await badClock.run(startBatch(bad, [filed("never.jpg")]));
  assert(attempts > 1, "invalid EOF is retried but never counted as success");
  assert.strictEqual(bad.location.assigned, "");
  assert.match(bad.els.get("upload-status").textContent, /Upload was not confirmed/);

  const dupClock = virtualClock();
  let dupPuts = 0;
  const dup = homePage((url) => {
    if (url === "/api/galleries/resolve") return Response.json({ slug: SLUG });
    dupPuts += 1;
    return Response.json({ error: "upload id already used" }, { status: 409 });
  }, dupClock);
  await dupClock.run(startBatch(dup, [filed("dup.jpg")]));
  assert.strictEqual(dupPuts, 1, "a definite id conflict is not retried");
  assert.match(dup.els.get("upload-status").textContent, /This upload ID is already used\. Select the file again/);
  assert.strictEqual(dup.location.assigned, "");
}

async function testUploadRateWait() {
  const clock = virtualClock();
  const limit = 2;
  const window = 60_000;
  let resetAt = 17_000;
  let used = limit;
  let shorter = true;
  const sent = [];
  const stored = [];
  const mints = [];
  const page = homePage((url, opts) => {
    const now = clock.now();
    const mint = opts.method === "POST";
    if (mint) mints.push(now);
    else sent.push(now);
    if (now >= resetAt) {
      resetAt = now + window;
      used = 0;
    }
    if (used === limit) {
      // In-flight responses can disagree; the longest observed wait wins.
      const seconds = !mint && shorter ? 2 : (resetAt - now) / 1000;
      if (!mint) shorter = false;
      return Response.json({ error: "too many requests" }, { status: 429, headers: { "Retry-After": String(seconds) } });
    }
    used += 1;
    if (mint) return Response.json({ slug: SLUG });
    stored.push(url);
    return Response.json({ ok: true }, { status: 201 });
  }, clock);

  const names = ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg"];
  page.els.get("file-input").files = names.map((name) => filed(name));
  const batch = page.fire("file-input", { type: "change" });
  assert.strictEqual(page.els.get("upload-btn").disabled, true, "batch owns the picker while prompting");
  page.els.get("file-input").files = [filed("competing.jpg")];
  await page.fire("file-input", { type: "change" });
  await submitKey(page, "Home-Key-1234");
  await clock.run(batch);
  assert.deepStrictEqual(mints, [0, 17_000], "mint honors Retry-After");
  assert(sent.every((t) => t >= 17_000 && (t - 17_000) % window === 0), "no worker sends before the shared deadline");
  assert.deepStrictEqual(stored.sort(), names.map((name) => "/api/photos/" + name), "a batch above the limit completes");
  assert.strictEqual(page.location.assigned, "/" + SLUG);
}

async function testUploadKeyCheckWaitsForRateLimit() {
  const clock = virtualClock();
  const started = [];
  const page = makePage({
    pathname: "/" + SLUG,
    key: KEY,
    clock,
    listing: { slug: SLUG, photos: [] },
    handler: (url, opts) => {
      started.push([opts.method, clock.now()]);
      if (url === "/api/galleries/resolve") {
        assert.strictEqual(JSON.parse(opts.body).slug, SLUG, "checks the existing gallery, not minting");
        if (clock.now() === 0) {
          return Response.json({ error: "too many requests" }, { status: 429, headers: { "Retry-After": "48" } });
        }
        return Response.json({ slug: SLUG });
      }
      return Response.json({ ok: true }, { status: 201 });
    },
  });
  page.els.get("file-input").files = [filed("a.jpg")];
  await clock.run(page.fire("file-input", { type: "change" }));
  assert.deepStrictEqual(started, [["POST", 0], ["POST", 48_000], ["PUT", 48_000]]);
  assert.strictEqual(page.els.get("upload-status").textContent, "Uploaded 1 photo.");
}

async function testBlankLanding() {
  const page = makePage({ pathname: "/" });
  await tick();
  assert.strictEqual(page.els.get("empty").hidden, true, "no empty-gallery message on the landing");
  assert.strictEqual(page.els.get("empty").textContent, "");
  assert.strictEqual(page.els.get("gallery-state").hidden, true, "no state on the landing");
}

async function testPrivateProtectWarns() {
  const page = makePage({
    pathname: "/" + SLUG,
    key: KEY,
    listing: { slug: SLUG, public: false, locked: false, photos: [] },
    handler: (url) => {
      if (url.endsWith("/visibility")) {
        return {
          status: 200,
          ok: true,
          statusText: "status 200",
          json: async () => ({ slug: SLUG, public: true }),
        };
      }
      throw new Error("unexpected request " + url);
    },
  });
  await tick();
  await page.fire("lock-btn");
  assert.strictEqual(page.els.get("setpass-dialog").open, true);
  assert.match(page.els.get("setpass-help").textContent, /private/, "warns that a private lock is unreachable");
  assert.strictEqual(page.els.get("setpass-private").hidden, false, "offers Make public");

  await page.fire("setpass-public");
  assert.strictEqual(page.els.get("setpass-private").hidden, true, "Make public hides its own row");
  assert.strictEqual(page.els.get("edit-status").textContent, "", "no redundant success message");
  assert.strictEqual(page.els.get("public-check").checked, true, "checkbox follows the publish");
  assert.match(
    page.els.get("setpass-help").textContent,
    /Enter a password below/,
    "no password is claimed before one is set"
  );
}

async function testLateOwnerCheckCannotResolveCreatePrompt() {
  let finishCheck;
  let ownerSignal;
  const page = makePage({
    pathname: "/" + SLUG,
    listing: { slug: SLUG, public: true, locked: false, photos: [] },
    handler(url, opts) {
      if (url === "/api/galleries/resolve" && JSON.parse(opts.body).slug) {
        ownerSignal = opts.signal;
        return new Promise((resolve) => { finishCheck = resolve; });
      }
      if (url === "/api/galleries/resolve") return Response.json({ slug: SLUG });
      assert.strictEqual(opts.method, "PUT");
      return Response.json({}, { status: 201 });
    },
  });
  let editingSettled = false;
  page.fire("edit-btn").then(() => { editingSettled = true; });
  await waitFor(() => page.els.get("key-dialog").open, "edit owner-key prompt");
  const oldValidation = submitKey(page, KEY);
  await waitFor(() => finishCheck, "owner check in flight");

  // Exercise competing modes on the same dialog without a document reload.
  page.location.pathname = "/";
  page.els.get("file-input").files = [filed("home.jpg")];
  const batch = page.fire("file-input", { type: "change" });
  await waitFor(() => editingSettled, "superseded owner-key waiter settles");
  assert.strictEqual(ownerSignal.aborted, true, "the superseded request is cancelled");
  await page.fire("key-dialog", { type: "close" }); // Native close is queued.
  finishCheck(Response.json({ slug: SLUG }));
  await oldValidation;
  assert.strictEqual(page.els.get("key-dialog").open, true, "late validation leaves replacement open");
  assert.strictEqual(page.els.get("key-title").textContent, "New gallery key");
  assert.strictEqual(page.session.has("gallery-key"), false, "superseded key is not saved");
  assert.strictEqual(page.els.get("editbar").hidden, true, "superseded Edit stays closed");
  await submitKey(page, "Home-Key-1234");
  await batch;
  assert.strictEqual(page.location.assigned, "/" + SLUG);
  assert.strictEqual(page.els.get("upload-btn").disabled, false, "replacement upload settles");
}

async function testWrongCachedKeyReprompts() {
  const wrong = "Wrong-Key-123";
  const right = "Right-Key-789";
  const page = makePage({
    pathname: "/" + SLUG,
    key: wrong,
    listing: { slug: SLUG, public: true, locked: false, photos: [] },
    handler: (url, opts) => {
      if (url === "/api/galleries/resolve") {
        if (opts.headers["X-Gallery-Key"] === right) {
          return { status: 200, ok: true, statusText: "status 200", json: async () => ({ slug: SLUG }) };
        }
        return {
          status: 403,
          ok: false,
          statusText: "status 403",
          json: async () => ({ error: "forbidden" }),
        };
      }
      throw new Error("unexpected request " + url);
    },
  });
  await tick();
  page.start("edit-btn");
  await waitFor(() => page.els.get("key-dialog").open, "wrong cached key prompts");
  assert.match(page.els.get("key-help").textContent, /saved key does not open/);
  assert.strictEqual(page.session.get("gallery-key"), wrong, "cached key survives until replaced");
  await submitKey(page, right);
  await waitFor(() => page.els.get("editbar").hidden === false, "correct key opens edit");
  assert.strictEqual(page.session.get("gallery-key"), right);
}

async function testCopyFallbackWithoutClipboard() {
  const page = makePage({ clipboardApi: false });
  await tick();
  let copied = "";
  page.document.execCommand = (cmd) => {
    const area = page.document.body.children[page.document.body.children.length - 1];
    copied = cmd === "copy" && area ? area.value : "";
    return cmd === "copy";
  };
  page.els.get("key-banner").hidden = false;
  page.els.get("key-banner-key").textContent = "Sunset-Beach-42";
  await page.fire("key-banner-copy");
  assert.strictEqual(copied, "Sunset-Beach-42", "fallback copies the key");
  assert.strictEqual(page.els.get("key-banner-copy").textContent, "Copied");
  assert.deepStrictEqual(page.clipboard, [], "clipboard API not used");
}

async function testLegacySlugURLs() {
  for (const slug of ["happy-otter-7", "oddball-slug-42"]) {
    const page = makePage({
      pathname: "/" + slug,
      listing: { slug, public: true, locked: false, photos: [] },
    });
    await tick();
    assert.strictEqual(page.els.get("gallery-state").hidden, false, slug + " opens the gallery");
    assert.strictEqual(page.els.get("gallery-label").textContent, slug);
    assert.ok(
      page.calls.some((c) => c.url.startsWith("/api/galleries/" + slug + "/photos")),
      slug + " is fetched like a gallery"
    );
  }
}

async function testPrivateGalleryKeyOpens() {
  const key = "Sunset-Beach-42";
  const slug = await mint(key);
  await upload(slug, key, "hidden.png", PNG, "image/png");
  await publish(slug, key, false);
  const page = makePage({ pathname: "/" + slug });
  await waitFor(() => page.els.get("key-dialog").open, "key prompt on a private gallery");
  assert.strictEqual(page.els.get("key-help").textContent, "This gallery is private or does not exist.");
  await submitKey(page, key);
  await waitFor(() => page.els.get("grid").children.length === 1, "private gallery lists with the key");
  assert.strictEqual(page.els.get("gallery-state").textContent, "Private", "private state after the key");
  assert.strictEqual(page.els.get("empty").hidden, true, "photos replace the unavailable message");
  assert.strictEqual(page.session.get("gallery-key"), key, "accepted key is stored");
}

(async () => {
  await testBlockedStorageStillLoads();
  await testLightboxFocusAndLinkCleanup();
  await testLightboxPausesAudio();
  testAccessibleMarkup();
  await testButtonsWithStoredKey();
  await testProtectReflectsLock();
  await testNoHeicNeverLoadsDecoder();
  await testHeicFetchesOnlyWhenVisible();
  await testHeicBudgetAndFallback();
  await testBlankLanding();
  await testPrivateProtectWarns();
  await testLateOwnerCheckCannotResolveCreatePrompt();
  await testWrongCachedKeyReprompts();
  await testCopyFallbackWithoutClipboard();
  await testLegacySlugURLs();
  await testHomeUploadUsesOneSlug();
  await testHomeUploadReportsPartialFailure();
  await testLostReplyDoesNotMintAgain();
  await testUploadRetriesLostChunk();
  await testUploadResumesFromServerOffset();
  await testUploadBusyAndInvalidOffsets();
  await testUploadRateWait();
  await testUploadKeyCheckWaitsForRateLimit();
  await testWrongInviteCodeIsAskedAgain();
  await testEmptyPasswordKeepsDialogOpen();
  await testPasswordCheckErrorsAndCancel();
  await testAdminLoadErrorIsVisible();
  await testAdminLogout();
  await testRejectedFilesDoNotMintGallery();
  if (BASE) {
    await workflowUpload();
    await testDelayedListingSharesUploadPrompt();
    await testVisibilityLostReply();
    await testPasswordUnconfirmedReply();
    await workflowEdit();
    await workflowView();
    await testBoundedTextPreview();
    await testPrivateGalleryKeyOpens();
  } else {
    console.log("client smoke: live workflows skipped (GALLY_BASE_URL unset)");
  }
  console.log("client smoke: ok");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
