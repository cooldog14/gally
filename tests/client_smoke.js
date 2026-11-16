// Client smoke test: runs app.js in a stub DOM.
// Legacy checks use stub fetch; the three user workflows drive the real
// server (GALLY_BASE_URL, set by tests/client_smoke.rs) so client and server
// contracts are exercised together. Every button path in index.html is exercised.
"use strict";

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const APP = fs.readFileSync(process.env.APP_JS || path.join(__dirname, "..", "app.js"), "utf8");
const BASE = (process.env.GALLY_BASE_URL || "").replace(/\/+$/, "");

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
  "key-banner",
  "lightbox",
  "load-more",
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
  focus() {}
}

function makePage({ pathname = "/", key = "", listing = null, handler = null, session: store = null } = {}) {
  const els = new Map();
  const calls = [];
  const docListeners = new Map();
  const session = store || new Map();
  let randomCalls = 0;
  const local = new Map();
  const page = { calls, els, session, local, location: null, document: null };

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
    createElement: (tag) => new El(tag, page),
    addEventListener(type, fn) {
      const list = docListeners.get(type) || [];
      list.push(fn);
      docListeners.set(type, list);
    },
  };
  document.documentElement.dataset.theme = "light";
  page.document = document;

  if (key) session.set("gallery-key", key);

  const storage = (map) => ({
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
  });

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
    sessionStorage: storage(session),
    localStorage: storage(local),
    location,
    matchMedia: () => ({ matches: false }),
    fetch: fetchImpl,
    // Deterministic but distinct per call: two concurrent files must not
    // share an upload id, or they would collide on the server's `.part-` temp.
    crypto: {
      getRandomValues(arr) {
        randomCalls += 1;
        for (let i = 0; i < arr.length; i += 1) arr[i] = (randomCalls + i) & 0xff;
        return arr;
      },
    },
    setTimeout: (fn, ms) => setTimeout(fn, ms),
    Image: class {},
    URL: { createObjectURL: () => "blob:x", revokeObjectURL() {} },
    IntersectionObserver: class {
      observe() {}
      disconnect() {}
    },
    console,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(APP, context, { filename: "app.js" });

  async function fire(id, event) {
    const target = els.get(id);
    assert(target, "no element #" + id);
    const type = (event && event.type) || "click";
    assert((target.listeners.get(type) || []).length > 0, "no handler registered for #" + id);
    await target.dispatch(type, event || {});
  }

  page.fire = fire;
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
  const res = await globalThis.fetch(BASE + "/api/galleries/resolve", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
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
  const key = "Sunset-Beach-42";
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
  assert.match(home.els.get("key-error").textContent, /12-128/, "weak key is rejected");
  assert.strictEqual(home.location.assigned, "", "no navigation for a weak key");

  await submitKey(home, key);
  await batch;
  assert.match(home.location.assigned, /^\/[a-z]+-[a-z]+-[a-z]+-[a-z]+-[0-9a-f]{4}$/, "navigates to minted gallery");
  const slug = home.location.assigned.slice(1);
  assert.strictEqual(session.get("show-key:" + slug), key, "banner key survives navigation");
  assert.strictEqual(await photoCount(slug, key), 2, "both files uploaded to one gallery");

  const gallery = makePage({ pathname: "/" + slug, session });
  await waitFor(() => gallery.els.get("grid").children.length === 2, "2 tiles after navigation");
  assert.strictEqual(gallery.els.get("key-banner").hidden, false);
  assert.strictEqual(gallery.els.get("key-banner-key").textContent, key);
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
  const tiles = () => page.els.get("grid").children;
  const sel = () => page.els.get("sel-count").textContent;

  await page.fire("edit-btn");
  assert.strictEqual(page.els.get("editbar").hidden, false, "edit bar opens");
  assert.strictEqual(page.els.get("upload-btn").hidden, true, "upload hidden while editing");
  assert.strictEqual(page.els.get("edit-btn").textContent, "Editing…");

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
  assert.strictEqual(page.els.get("edit-status").textContent, "Gallery is public");
  assert.strictEqual(await galleryStatus(slug), 200, "public gallery lists anonymously");

  page.els.get("public-check").checked = false;
  await page.fire("public-check", { type: "change" });
  assert.strictEqual(page.els.get("edit-status").textContent, "Gallery is private");
  assert.strictEqual(await galleryStatus(slug), 404, "private gallery hides");

  page.els.get("public-check").checked = true;
  await page.fire("public-check", { type: "change" });

  await page.fire("lock-btn");
  assert.strictEqual(page.els.get("setpass-dialog").open, true);
  assert.strictEqual(page.els.get("setpass-remove").hidden, true, "unlocked gallery has nothing to remove");
  page.els.get("setpass-input").value = "Viewer-Pass-9";
  await page.fire("setpass-form", { type: "submit", submitter: { value: "default" } });
  assert.strictEqual(page.els.get("upload-status").textContent, "Gallery protected.");
  assert.strictEqual(await galleryStatus(slug), 401, "protected gallery asks for the password");

  await page.fire("lock-btn");
  assert.strictEqual(page.els.get("setpass-remove").hidden, false, "locked gallery offers Remove");
  await page.fire("setpass-form", { type: "submit", submitter: { value: "remove" } });
  assert.strictEqual(page.els.get("upload-status").textContent, "Protection removed.");
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
  assert.match(page.els.get("confirm-text").textContent, /Delete this gallery and all 2 photos\?/);
  await page.fire("confirm-form", { type: "submit", submitter: { value: "default" } });
  assert.strictEqual(page.location.assigned, "/", "deleted gallery returns home");
  assert.strictEqual(await galleryStatus(slug), 404);
}

// --- workflow 3: view and mess around ---

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
  assert.strictEqual(page.els.get("lightbox").hidden, false);
  assert.strictEqual(counter.textContent, `${imageAt + 1} / 203`);
  const shown = page.els.get("viewport").querySelector("img");
  assert(shown && shown.src.includes("/photos/") && shown.src.endsWith(".png"), "lightbox shows the original");

  await page.fire("next");
  assert.strictEqual(counter.textContent, `${((imageAt + 1) % 203) + 1} / 203`);
  await page.fire("prev");
  assert.strictEqual(counter.textContent, `${imageAt + 1} / 203`);

  await page.fire("close");
  assert.strictEqual(page.els.get("lightbox").hidden, true);

  await page.fireEl(tiles()[0]);
  await page.fire("prev");
  assert.strictEqual(counter.textContent, "203 / 203", "previous wraps to the last photo");
  await page.fireDocument("keydown", { key: "ArrowRight" });
  assert.strictEqual(counter.textContent, "1 / 203");
  await page.fireDocument("keydown", { key: "Escape" });
  assert.strictEqual(page.els.get("lightbox").hidden, true, "Escape closes the viewer");

  await page.fire("theme-btn");
  assert.strictEqual(page.document.documentElement.dataset.theme, "dark");
  assert.strictEqual(page.local.get("theme"), "dark");
  await page.fire("theme-btn");
  assert.strictEqual(page.document.documentElement.dataset.theme, "light");

  // Edit without a stored key: the key dialog prompts, Esc cancels.
  await page.fire("edit-btn");
  await waitFor(() => page.els.get("key-dialog").open, "key prompt for editing");
  assert.match(page.els.get("key-title").textContent, /Gallery key/);
  page.els.get("key-dialog").close();
  await tick();
  assert.strictEqual(page.els.get("editbar").hidden, true, "cancelled edit stays closed");

  await page.fire("edit-btn");
  await waitFor(() => page.els.get("key-dialog").open, "key prompt for editing");
  await submitKey(page, key);
  assert.strictEqual(page.els.get("editbar").hidden, false, "owner key opens edit mode");
  await page.fire("done-btn");

  // Locked gallery: pass dialog cancel, then unlock with the password.
  await setLock(slug, key, "Viewer-Pass-9");
  const locked = makePage({ pathname: "/" + slug });
  await waitFor(() => locked.els.get("pass-dialog").open, "password prompt");
  await locked.fire("pass-cancel");
  await waitFor(
    () => locked.els.get("empty").textContent === "This gallery is locked.",
    "locked gallery message"
  );
  assert.strictEqual(locked.els.get("grid").children.length, 0);

  const unlocking = makePage({ pathname: "/" + slug });
  await waitFor(() => unlocking.els.get("pass-dialog").open, "password prompt");
  unlocking.els.get("pass-input").value = "Viewer-Pass-9";
  await unlocking.fire("pass-form", { type: "submit", submitter: { value: "unlock" } });
  await waitFor(() => unlocking.els.get("grid").children.length === 200, "unlocked listing");

  // Private gallery without a key: prompt, then cancel back home.
  await publish(slug, key, false);
  const hidden = makePage({ pathname: "/" + slug });
  await waitFor(() => hidden.els.get("key-dialog").open, "key prompt on a private gallery");
  hidden.els.get("key-dialog").close();
  await waitFor(() => hidden.location.assigned === "/", "cancel returns home");
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

function homePage(uploadHandler) {
  return makePage({
    pathname: "/",
    handler: uploadHandler,
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

async function testUploadResumesFromServerOffset() {
  const size = UPLOAD_CHUNK + 10;
  let putCount = 0;
  const puts = [];
  const page = homePage((url, opts) => {
    if (url === "/api/galleries/resolve") return { status: 200, ok: true, json: async () => ({ slug: SLUG }) };
    if (opts.method === "PUT") {
      puts.push(opts);
      putCount += 1;
      if (putCount === 1) {
        return {
          status: 409,
          ok: false,
          json: async () => ({ error: "offset mismatch", received: UPLOAD_CHUNK }),
        };
      }
      return { status: 201, ok: true, json: async () => ({ name: "x", slug: SLUG }) };
    }
    throw new Error("unexpected " + url);
  });
  await startBatch(page, [filed("big.jpg", size)]);
  assert.strictEqual(puts.length, 2, "the second chunk was sent");
  assert.strictEqual(puts[0].headers["Content-Range"], `bytes 0-${UPLOAD_CHUNK - 1}/${size}`);
  assert.strictEqual(
    puts[1].headers["Content-Range"],
    `bytes ${UPLOAD_CHUNK}-${size - 1}/${size}`,
    "resumes at the server's committed offset"
  );
  assert.strictEqual(puts[1].headers["X-Upload-Id"], puts[0].headers["X-Upload-Id"]);
  assert.strictEqual(page.location.assigned, "/" + SLUG);
}

(async () => {
  await testButtonsWithStoredKey();
  await testProtectReflectsLock();
  await testNoHeicNeverLoadsDecoder();
  await testHeicFetchesOnlyWhenVisible();
  await testHomeUploadUsesOneSlug();
  await testLostReplyDoesNotMintAgain();
  await testUploadRetriesLostChunk();
  await testUploadResumesFromServerOffset();
  if (BASE) {
    await workflowUpload();
    await workflowEdit();
    await workflowView();
  } else {
    console.log("client smoke: live workflows skipped (GALLY_BASE_URL unset)");
  }
  console.log("client smoke: ok");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
