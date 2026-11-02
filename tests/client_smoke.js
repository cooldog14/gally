// Client smoke test: loads app.js against a stub DOM and clicks the buttons.
// Run by tests/client_smoke.rs. Fails on any thrown error / wrong request.
"use strict";

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const APP = fs.readFileSync(process.env.APP_JS || path.join(__dirname, "..", "app.js"), "utf8");

function makePage({ pathname = "/", key = "", listing = null, handler }) {
  const listeners = new Map();
  const els = new Map();
  const calls = [];
  const location = {
    pathname,
    assigned: "",
    assign(url) {
      location.assigned = url;
    },
  };

  function el(id) {
    if (els.has(id)) return els.get(id);
    const e = {
      id,
      hidden: false,
      disabled: false,
      textContent: "",
      value: "",
      checked: false,
      files: [],
      dataset: {},
      children: [],
      clientWidth: 800,
      classList: { toggle() {}, add() {}, remove() {} },
      setAttribute() {},
      getAttribute() {
        return null;
      },
      appendChild(c) {
        e.children.push(c);
        return c;
      },
      prepend(c) {
        e.children.unshift(c);
        return c;
      },
      querySelector() {
        return null;
      },
      showModal() {},
      close() {},
      focus() {},
      addEventListener(ev, fn) {
        const m = listeners.get(id) || {};
        (m[ev] = m[ev] || []).push(fn);
        listeners.set(id, m);
      },
    };
    els.set(id, e);
    return e;
  }

  const response = (status, body) => ({
    status,
    ok: status >= 200 && status < 300,
    statusText: "status " + status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  });

  const session = new Map();
  if (key) session.set("gallery-key", key);
  const sessionStorage = {
    getItem: (k) => (session.has(k) ? session.get(k) : null),
    setItem: (k, v) => session.set(k, String(v)),
    removeItem: (k) => session.delete(k),
  };

  const fetch = async (url, opts = {}) => {
    calls.push({ url, opts });
    if (url.endsWith("/photos") && opts.method === undefined && listing) {
      return response(200, listing);
    }
    return handler(url, opts, calls);
  };

  const context = {
    document: {
      getElementById: el,
      createElement: (tag) => el(tag),
      addEventListener() {},
      documentElement: { dataset: {} },
      body: { classList: { toggle() {} } },
    },
    window: { innerHeight: 600, addEventListener() {} },
    sessionStorage,
    localStorage: sessionStorage,
    location,
    matchMedia: () => ({ matches: false }),
    fetch,
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

  async function fire(id, event = {}) {
    const hs = ((listeners.get(id) || {})[event.type || "click"] || []).slice();
    assert(hs.length > 0, "no handler registered for #" + id);
    for (const h of hs) await h(event);
  }

  return { els, calls, fire, session, location };
}

const tick = async () => {
  for (let i = 0; i < 5; i += 1) await new Promise((r) => setImmediate(r));
};

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

function homePage(uploadHandler) {
  return makePage({
    pathname: "/",
    key: "",
    handler: uploadHandler,
  });
}

const filed = (name) => ({ name, type: "image/jpeg", size: 10 });

async function startBatch(page, files) {
  page.els.get("file-input").files = files;
  const done = page.fire("file-input", { type: "change" });
  await tick();
  // Home uploads always prompt: answer the key dialog.
  page.els.get("key-input").value = "Home-Key-1234";
  await page.fire("key-form", { type: "submit", preventDefault() {} });
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
      puts.push(opts);
      putCount += 1;
      if (putCount === 1) throw new TypeError("network lost");
      return { status: 201, ok: true, json: async () => ({ name: "x", slug: SLUG }) };
    }
    throw new Error("unexpected " + url);
  });
  await startBatch(page, [filed("a.jpg"), filed("b.jpg")]);
  assert.strictEqual(resolves.length, 1, "mints exactly one gallery per batch");
  assert.strictEqual(puts.length, 2);
  for (const p of puts) assert.strictEqual(p.headers["X-Gallery-Slug"], SLUG, "second file keeps the batch slug");
  assert.strictEqual(page.location.assigned, "/" + SLUG);
}

(async () => {
  await testButtonsWithStoredKey();
  await testProtectReflectsLock();
  await testHomeUploadUsesOneSlug();
  await testLostReplyDoesNotMintAgain();
  console.log("client smoke: ok");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
