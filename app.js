"use strict";

const MAX_BYTES = 10 * 1024 * 1024 * 1024;

const SLUG_RE = /^(?:oddball-slug-extra-[0-9a-f]{8}|[a-z]+-[a-z]+-[a-z]+-[a-z]+-[0-9a-f]{4})$/;

const galleryLabel = document.getElementById("gallery-label");
const uploadBtn = document.getElementById("upload-btn");
const uploadStatus = document.getElementById("upload-status");
const fileInput = document.getElementById("file-input");
const themeBtn = document.getElementById("theme-btn");
const keyDialog = document.getElementById("key-dialog");
const keyForm = document.getElementById("key-form");
const keyInput = document.getElementById("key-input");
const keyError = document.getElementById("key-error");

// Gallery key: kept in memory + sessionStorage (per-tab). Never in the URL.
let galleryKey = sessionStorage.getItem("gallery-key") || "";
let keyResolver = null;

// Media uploaded to this gallery: the canvas block picker reads it.
let photos = [];

function slugFromPath() {
  const seg = location.pathname.split("/").filter(Boolean)[0] || "";
  return SLUG_RE.test(seg) ? seg : "";
}

// Viewer password for locked galleries: "<slug>:<password>" per tab.
function storedPass(slug) {
  try { return sessionStorage.getItem("gallery-pass:" + slug) || ""; }
  catch (_) { return ""; }
}

function displayName(stored) {
  const i = stored.indexOf("_");
  const j = stored.indexOf("_", i + 1);
  return j >= 0 ? stored.slice(j + 1) : stored;
}

// Media URLs carry the viewer password (query) when the gallery is locked;
// the owner key bypasses the lock server-side so no query is needed then.
function mediaUrl(name) {
  const slug = slugFromPath();
  const base = "/photos/" + encodeURIComponent(name);
  if (galleryKey) return base;
  const p = slug ? storedPass(slug) : "";
  return p ? base + "?pass=" + encodeURIComponent(p) : base;
}

// Same credential as a header (for fetch()).
function passHeader() {
  const slug = slugFromPath();
  const p = slug ? storedPass(slug) : "";
  const headers = {};
  if (galleryKey) headers["X-Gallery-Key"] = galleryKey;
  else if (p) headers["X-Gallery-Pass"] = p;
  return headers;
}

/** Resolve the gallery key: memory/session, else prompt once.
    Landing page (no slug) always prompts: returning here means a new gallery. */
function needKey(forcePrompt) {
  if (!forcePrompt && galleryKey && galleryKey.length >= 6) return Promise.resolve(galleryKey);
  keyError.textContent = "";
  keyInput.value = "";
  resetKeyDialog();
  keyDialog.showModal();
  keyInput.focus();
  return new Promise((resolve) => { keyResolver = resolve; });
}

// Creation-time lock: ticking Private reveals the password field; the value
// is applied via /lock right after the gallery's first upload lands.
let pendingPass = "";
function resetKeyDialog() {
  invitePrompt = false;
  document.getElementById("key-title").textContent = "Gallery key";
  document.getElementById("private-check").checked = false;
  document.getElementById("private-row").hidden = false;
  document.getElementById("key-pass-input").hidden = true;
  document.getElementById("key-pass-input").value = "";
  pendingPass = "";
}

let passResolver = null;
/** Prompt for a locked gallery's viewer password. Resolves true on unlock. */
function needPass(slug) {
  const dialog = document.getElementById("pass-dialog");
  const input = document.getElementById("pass-input");
  const errEl = document.getElementById("pass-error");
  errEl.textContent = "";
  input.value = "";
  dialog.showModal();
  input.focus();
  return new Promise((resolve) => { passResolver = { resolve, slug }; });
}

document.getElementById("pass-form").addEventListener("submit", () => {
  if (!passResolver) return;
  const { resolve, slug } = passResolver;
  const input = document.getElementById("pass-input");
  if (document.activeElement && document.activeElement.value === "cancel") {
    passResolver = null;
    resolve(false);
    return;
  }
  const v = input.value;
  if (!v) {
    document.getElementById("pass-error").textContent = "Password required";
    document.getElementById("pass-dialog").showModal();
    return;
  }
  try { sessionStorage.setItem("gallery-pass:" + slug, v); } catch (_) {}
  passResolver = null;
  resolve(true);
});
document.getElementById("pass-dialog").addEventListener("close", () => {
  if (passResolver) {
    const { resolve } = passResolver;
    passResolver = null;
    resolve(false);
  }
});

// Invite code for NEW galleries: memory-only, asked once per batch when the
// server answers "invite required". Uploading into an existing gallery never
// needs it.
let inviteCode = "";
let invitePrompt = false;
/** Prompt for the invite code (reuses the key dialog UI). */
function needInvite() {
  if (inviteCode) return Promise.resolve(inviteCode);
  invitePrompt = true;
  document.getElementById("key-title").textContent = "Invite code";
  keyError.textContent = "";
  keyInput.value = "";
  keyDialog.showModal();
  keyInput.focus();
  return new Promise((resolve) => { keyResolver = resolve; });
}

document.getElementById("private-check").addEventListener("change", (e) => {
  document.getElementById("key-pass-input").hidden = !e.target.checked;
  if (e.target.checked) document.getElementById("key-pass-input").focus();
});

/** New galleries: 6+ chars, one capital, one symbol. Signing in to an
    existing gallery accepts any 6+ key, so keys made before this rule work. */
function keyPolicyError(v, creating) {
  if (v.length < 6) return "Key must be at least 6 characters";
  if (!creating) return "";
  if (!/[A-Z]/.test(v)) return "Key needs at least one capital letter";
  if (!/[^A-Za-z0-9\s]/.test(v)) return "Key needs at least one symbol (like ! or #)";
  return "";
}

keyForm.addEventListener("submit", (e) => {
  const v = keyInput.value.trim();
  // The invite code is server-configured: only reject an empty one.
  const err = invitePrompt
    ? (v ? "" : "Invite code required")
    : keyPolicyError(v, !slugFromPath());
  if (err) {
    e.preventDefault(); // keep the dialog open (Esc still cancels)
    keyError.textContent = err;
    keyInput.focus();
    return;
  }
  const wantPrivate = document.getElementById("private-check").checked;
  const passVal = document.getElementById("key-pass-input").value;
  if (wantPrivate && !passVal) {
    e.preventDefault();
    keyError.textContent = "Set a viewer password, or untick Private";
    document.getElementById("key-pass-input").focus();
    return;
  }
  pendingPass = wantPrivate ? passVal : "";
  if (invitePrompt) {
    inviteCode = v;
  } else {
    galleryKey = v;
    try { sessionStorage.setItem("gallery-key", v); } catch (_) { /* private mode */ }
  }
  resetKeyDialog();
  if (keyResolver) keyResolver(v);
  keyResolver = null;
});
// Dialog closed via Esc: treat as cancel.
keyDialog.addEventListener("close", () => {
  if (keyResolver) keyResolver("");
  keyResolver = null;
  resetKeyDialog();
});

uploadBtn.addEventListener("click", () => fileInput.click());

fileInput.addEventListener("change", async () => {
  const key = await needKey(!slugFromPath());
  if (!key) { fileInput.value = ""; return; }
  const files = Array.from(fileInput.files);
  fileInput.value = "";
  const failures = [];
  let firstSlug = "";
  let queued = [];
  // Gallery page: tell the server which gallery this targets (scoped auth).
  // Home: omit it, the server mints a fresh gallery for the key.
  const pageSlug = slugFromPath();
  async function putFile(file, extra) {
    const headers = { "X-Gallery-Key": key, ...extra };
    if (pageSlug) headers["X-Gallery-Slug"] = pageSlug;
    if (file.type) headers["Content-Type"] = file.type;
    try {
      return await fetch("/api/photos/" + encodeURIComponent(file.name), {
        method: "PUT",
        headers,
        body: file,
      });
    } catch (_) {
      return null;
    }
  }
  for (const file of files) {
    if (file.size > MAX_BYTES) {
      failures.push(file.name + ": file is too large (10GB max)");
      continue;
    }
    let res = await putFile(file, inviteCode ? { "X-Invite-Code": inviteCode } : null);
    if (!res) {
      failures.push(file.name + ": upload failed");
      continue;
    }
    if (res.status === 403) {
      const err = await res.json().catch(() => null);
      if (err && err.error === "invite required") {
        // Unknown gallery key: one invite prompt covers the whole batch.
        queued.push(file);
        continue;
      }
    }
    if (!res.ok) {
      let msg = res.statusText;
      try {
        const err = await res.json();
        if (err.error) msg = err.error;
      } catch (_) { /* keep status text */ }
      failures.push(file.name + ": " + msg);
      continue;
    }
    const data = await res.json().catch(() => null);
    if (data && data.slug) firstSlug = data.slug;
  }
  if (queued.length) {
    const invite = await needInvite();
    if (!invite) {
      for (const file of queued) failures.push(file.name + ": invite required");
    } else {
      for (const file of queued) {
        const res = await putFile(file, { "X-Invite-Code": invite });
        if (!res) {
          failures.push(file.name + ": upload failed");
          continue;
        }
        if (!res.ok) {
          let msg = res.statusText;
          try {
            const err = await res.json();
            if (err.error) msg = err.error;
          } catch (_) { /* keep status text */ }
          failures.push(file.name + ": " + msg);
          continue;
        }
        const data = await res.json().catch(() => null);
        if (data && data.slug) firstSlug = data.slug;
      }
    }
  }
  if (firstSlug && pendingPass) {
    // Private ticked: lock the new gallery before leaving.
    try {
      await fetch("/api/galleries/" + firstSlug + "/lock", {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
        body: JSON.stringify({ password: pendingPass }),
      });
      try { sessionStorage.setItem("gallery-pass:" + firstSlug, pendingPass); } catch (_) {}
    } catch (_) {}
    pendingPass = "";
  }
  if (firstSlug && !slugFromPath()) {
    // First upload created the gallery: navigate to its public link.
    location.assign("/" + firstSlug);
    return;
  }
  pendingPass = "";
  // New files must show up in the block picker without a reload.
  if (firstSlug) await loadPhotos();
  uploadStatus.textContent = failures.length ? failures.join("; ") : "";
});

themeBtn.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem("theme", next); } catch (_) { /* private mode */ }
});

// --- canvas (the gallery page itself) ---

const canvasEl = document.getElementById("canvas");
const canvasBar = document.getElementById("canvas-bar");
const canvasTools = document.getElementById("canvas-tools");
const canvasEmpty = document.getElementById("canvas-empty");
const canvasStatus = document.getElementById("canvas-status");
const surface = document.getElementById("canvas-surface");
const modeBtn = document.getElementById("canvas-mode");
const addTextBtn = document.getElementById("add-text");
const mediaTray = document.getElementById("media-tray");
const organizeBy = document.getElementById("organize-by");
const organizeBtn = document.getElementById("organize");
const lockBtn = document.getElementById("lock-btn");

let canvasBlocks = [];
// Owners land in the editor; viewers only ever see the rendered canvas.
let canvasEditing = true;

function clamp(v, lo, hi) {
  return Math.min(Math.max(v, lo), hi);
}

function newBlockId() {
  const raw = typeof crypto !== "undefined" && crypto.randomUUID
    ? crypto.randomUUID()
    : String(Date.now()) + Math.random();
  return raw.replace(/[^a-z0-9]/gi, "").toLowerCase().slice(0, 32);
}

function isOwner() {
  return !!galleryKey && !!slugFromPath();
}

/** Below everything currently rendered: measured, so nothing overlaps. */
function nextBlockY() {
  let y = 20;
  for (const el of surface.children) y = Math.max(y, el.offsetTop + el.offsetHeight + 16);
  return Math.min(Math.round(y), 100000);
}

// Only what an <img>/<audio> block can actually play in a browser.
const IMAGE_EXT = [".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".bmp", ".svg", ".ico"];
const AUDIO_EXT = [".mp3", ".wav", ".flac", ".m4a", ".ogg", ".oga", ".opus"];
function blockKindFor(name) {
  const dot = name.lastIndexOf(".");
  const ext = dot >= 0 ? name.slice(dot).toLowerCase() : "";
  if (IMAGE_EXT.includes(ext)) return "image";
  if (AUDIO_EXT.includes(ext)) return "audio";
  return "";
}

// Escape first, then format. Only http(s) links, safe under innerHTML.
function mdLine(s) {
  return s
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;")
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*]+)\*/g, "<em>$1</em>")
    .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" rel="noopener noreferrer nofollow" target="_blank">$1</a>');
}

function markdownNode(text) {
  const div = document.createElement("div");
  div.className = "md";
  div.innerHTML = String(text).split("\n").map(mdLine).join("<br>");
  return div;
}

function brokenMediaEl(b) {
  const d = document.createElement("div");
  d.className = "file-generic";
  d.textContent = displayName(b.src) + " (missing)";
  return d;
}

/** Pointer gestures on a handle: start/move/end with shared start state. */
function dragFrom(handle, onStart, onMove, onEnd) {
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    handle.setPointerCapture(e.pointerId);
    const start = { x: e.clientX, y: e.clientY };
    onStart(start);
    const move = (ev) => onMove(ev, start);
    const end = (ev) => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", end);
      handle.removeEventListener("pointercancel", end);
      onEnd(ev, start);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", end);
    handle.addEventListener("pointercancel", end);
  });
}

/** Continuous placement, rounded so saved documents stay readable. */
function settleBlock(b, place) {
  b.x = Math.round(b.x * 10) / 10;
  b.y = Math.round(b.y);
  b.w = Math.round(b.w * 10) / 10;
  place();
}

/** The block you just grabbed paints on top, and stays there after the drop. */
function raiseBlock(el, b) {
  if (surface.lastElementChild === el) return;
  surface.appendChild(el);
  const i = canvasBlocks.indexOf(b);
  if (i >= 0) canvasBlocks.push(...canvasBlocks.splice(i, 1));
}

function canvasBlockEl(b) {
  const width = () => Math.max(1, surface.getBoundingClientRect().width);
  const el = document.createElement("div");
  el.className = "block";
  el.dataset.id = b.id;
  el.style.left = b.x + "%";
  el.style.top = b.y + "px";
  el.style.width = b.w + "%";
  const place = () => {
    el.style.left = b.x + "%";
    el.style.top = b.y + "px";
    el.style.width = b.w + "%";
  };

  if (canvasEditing) {
    const grip = document.createElement("div");
    grip.className = "grip";
    grip.title = "Drag to move";
    // Only the grip drags: audio controls, links and text stay clickable.
    dragFrom(grip,
      (s) => { s.bx = b.x; s.by = b.y; el.classList.add("dragging"); },
      (ev, s) => {
        b.x = clamp(s.bx + ((ev.clientX - s.x) / width()) * 100, 0, 100 - b.w);
        b.y = clamp(s.by + ev.clientY - s.y, 0, 100000);
        place();
      },
      () => {
        el.classList.remove("dragging");
        settleBlock(b, place);
        raiseBlock(el, b);
        resizeSurface();
        saveCanvas();
      });
    el.appendChild(grip);
  }

  if (b.kind === "text") {
    if (canvasEditing) {
      const ta = document.createElement("textarea");
      ta.rows = 3;
      ta.value = b.text || "";
      ta.addEventListener("input", () => {
        b.text = ta.value;
        autosize(ta);
      });
      ta.addEventListener("change", () => saveCanvas());
      el.appendChild(ta);
    } else {
      el.appendChild(markdownNode(b.text || ""));
    }
  } else if (b.kind === "image") {
    const img = document.createElement("img");
    // Eager: heights are measured for layout (organize, next block position).
    img.loading = "eager";
    img.alt = "";
    img.src = mediaUrl(b.src);
    img.addEventListener("load", resizeSurface);
    img.addEventListener("error", () => { img.replaceWith(brokenMediaEl(b)); resizeSurface(); });
    el.appendChild(img);
  } else {
    const a = document.createElement("audio");
    a.controls = true;
    a.preload = "metadata";
    a.src = mediaUrl(b.src);
    // Blocks never pause each other: no shared playback state, by design.
    el.appendChild(a);
  }

  if (canvasEditing) {
    const resize = document.createElement("div");
    resize.className = "resize";
    resize.title = "Drag to resize width";
    dragFrom(resize,
      (s) => { s.bw = b.w; el.classList.add("dragging"); },
      (ev, s) => { b.w = clamp(s.bw + ((ev.clientX - s.x) / width()) * 100, 5, 100); place(); },
      () => {
        el.classList.remove("dragging");
        settleBlock(b, place);
        raiseBlock(el, b);
        resizeSurface();
        saveCanvas();
      });
    el.appendChild(resize);

    const del = document.createElement("button");
    del.type = "button";
    del.className = "block-del";
    del.textContent = "\u00d7";
    del.setAttribute("aria-label", "Remove block");
    del.addEventListener("click", () => {
      canvasBlocks = canvasBlocks.filter((x) => x !== b);
      el.remove();
      canvasEmpty.hidden = canvasBlocks.length > 0;
      resizeSurface();
      saveCanvas();
    });
    el.appendChild(del);
  }
  return el;
}

function resizeSurface() {
  if (canvasEl.hidden) return;
  let bottom = 0;
  for (const el of surface.children) bottom = Math.max(bottom, el.offsetTop + el.offsetHeight);
  // Blocks are absolute: the surface needs an explicit height to scroll.
  surface.style.height = Math.max(bottom + 40, Math.round(window.innerHeight * 0.6)) + "px";
}

/** Textareas show all their text: layout heights match the content. */
function autosize(ta) {
  ta.style.height = "auto";
  ta.style.height = ta.scrollHeight + "px";
}

/** Resolve once the image is decoded (or failed), so heights are final. */
function imageReady(img) {
  const done = () => img.decode?.().catch(() => {}).then(() => {});
  if (img.complete && img.naturalWidth) return done();
  return new Promise((resolve) => {
    const finish = () => resolve();
    img.addEventListener("load", finish, { once: true });
    img.addEventListener("error", finish, { once: true });
    // Lazy images may never start on their own: nudge a fetch.
    img.loading = "eager";
    const src = img.getAttribute("src");
    img.removeAttribute("src");
    if (src) img.setAttribute("src", src);
    setTimeout(finish, 8000);
  }).then(done);
}

function renderCanvas() {
  surface.textContent = "";
  surface.classList.toggle("editing", canvasEditing);
  for (const b of canvasBlocks) surface.appendChild(canvasBlockEl(b));
  // Mounted, so scrollHeight (hence the real height) is available.
  for (const ta of surface.querySelectorAll("textarea")) autosize(ta);
  canvasEmpty.hidden = canvasBlocks.length > 0;
  canvasEmpty.textContent = canvasEditing ? "Add your first block." : "No canvas yet.";
  canvasTools.hidden = !canvasEditing;
  mediaTray.hidden = !canvasEditing;
  resizeSurface();
}

/** Replace the canvas with a message (no blocks): load errors, locked, etc. */
function canvasMessage(text) {
  canvasBlocks = [];
  canvasEl.hidden = false;
  surface.textContent = "";
  surface.style.height = "";
  canvasEmpty.hidden = false;
  canvasEmpty.textContent = text;
  canvasTools.hidden = true;
  mediaTray.hidden = true;
}

async function loadCanvas(slug) {
  let res;
  try {
    res = await fetch("/api/galleries/" + slug + "/canvas", { headers: passHeader() });
  } catch (_) {
    canvasMessage("Could not load canvas");
    return;
  }
  if (res.status === 401) {
    if (await needPass(slug)) return loadCanvas(slug);
    canvasMessage("This gallery is locked.");
    return;
  }
  if (!res.ok) {
    canvasMessage("Could not load canvas");
    return;
  }
  const data = await res.json().catch(() => null);
  canvasBlocks = data && Array.isArray(data.blocks) ? data.blocks : [];
  canvasEl.hidden = false;
  renderCanvas();
}

async function saveCanvas() {
  const slug = slugFromPath();
  // Viewers and preview mode never write.
  if (!slug || !galleryKey || !canvasEditing) return;
  canvasStatus.textContent = "Saving…";
  try {
    const res = await fetch("/api/galleries/" + slug + "/canvas", {
      method: "PUT",
      headers: { "Content-Type": "application/json", "X-Gallery-Key": galleryKey },
      body: JSON.stringify({ blocks: canvasBlocks }),
    });
    if (!res.ok) {
      let msg = res.statusText;
      try { const e = await res.json(); if (e.error) msg = e.error; } catch (_) { /* keep status */ }
      // Keep the editor content: the owner can fix and save again.
      canvasStatus.textContent = "Save failed: " + msg;
      return;
    }
    canvasStatus.textContent = "";
  } catch (_) {
    canvasStatus.textContent = "Save failed";
  }
}

/** Every uploaded image/audio, one click away. No "add media" picking. */
function renderMediaTray() {
  mediaTray.textContent = "";
  const items = photos.filter((p) => blockKindFor(p.name));
  if (!items.length) {
    const p = document.createElement("span");
    p.className = "tray-empty";
    p.textContent = "No media yet — use Upload.";
    mediaTray.appendChild(p);
    return;
  }
  for (const p of items) {
    const kind = blockKindFor(p.name);
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "tray-item";
    btn.title = "Add " + displayName(p.name);
    btn.setAttribute("aria-label", "Add " + displayName(p.name));
    if (kind === "image") {
      const img = document.createElement("img");
      img.loading = "lazy";
      img.alt = "";
      img.src = mediaUrl(p.name);
      btn.appendChild(img);
    } else {
      const glyph = document.createElement("span");
      glyph.className = "tray-glyph";
      glyph.textContent = "\u266b";
      btn.appendChild(glyph);
      const label = document.createElement("span");
      label.className = "tray-label";
      label.textContent = displayName(p.name);
      btn.appendChild(label);
    }
    btn.addEventListener("click", () => addMediaBlock(p.name, kind));
    mediaTray.appendChild(btn);
  }
}

function addMediaBlock(name, kind) {
  canvasBlocks.push({
    id: newBlockId(),
    kind,
    // Same column geometry Organize produces, so new blocks line up.
    x: 4,
    y: nextBlockY(),
    w: 44,
    src: name,
    created: Date.now(),
  });
  renderCanvas();
  saveCanvas();
}

/** Reading order per block, for "by name". */
function blockLabel(b) {
  return b.kind === "text" ? (b.text || "").split("\n")[0] : displayName(b.src || "");
}

const KIND_ORDER = ["text", "image", "audio"];
const ORGANIZE = {
  date: (a, b) => (a.created || 0) - (b.created || 0),
  name: (a, b) => blockLabel(a).localeCompare(blockLabel(b)),
  kind: (a, b) =>
    KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind) || (a.created || 0) - (b.created || 0),
};

/** Sort the blocks, then pack them into aligned rows: one column on narrow screens. */
async function organizeCanvas(by) {
  if (!canvasBlocks.length) return;
  const wasEditing = canvasEditing;
  canvasBlocks.sort(ORGANIZE[by] || ORGANIZE.date);
  const cols = surface.clientWidth < 700 ? 1 : 2;
  const widthPct = cols === 1 ? 92 : 44;
  const colX = cols === 1 ? [4] : [4, 52];
  const GAP = 16;
  // Text wraps differently in a textarea than in rendered markdown, so both
  // renderings are measured and the taller one wins: no overlap in either mode.
  const measure = async (editing) => {
    canvasEditing = editing;
    renderCanvas();
    const els = [...surface.children];
    els.forEach((el) => { el.style.width = widthPct + "%"; });
    await Promise.all(els.flatMap((el) => [...el.querySelectorAll("img")]).map(imageReady));
    return els.map((el) => el.offsetHeight);
  };
  const viewH = await measure(false);
  const editH = await measure(true);
  const heights = viewH.map((h, i) => Math.max(h, editH[i] || 0));
  const els = [...surface.children];
  let y = 20;
  for (let i = 0; i < els.length; i += cols) {
    const rowH = Math.max(...heights.slice(i, i + cols));
    els.slice(i, i + cols).forEach((el, j) => {
      el.style.left = colX[j] + "%";
      el.style.top = y + "px";
    });
    y += rowH + GAP;
  }
  for (const [i, b] of canvasBlocks.entries()) {
    b.x = parseFloat(els[i].style.left);
    b.y = parseFloat(els[i].style.top);
    b.w = widthPct;
  }
  canvasEditing = wasEditing;
  renderCanvas();
  saveCanvas();
}

/** Non-owners get one button (key entry); owners get the full editor chrome. */
function syncOwnerUI() {
  if (!isOwner()) canvasEditing = false;
  canvasTools.hidden = !canvasEditing;
  mediaTray.hidden = !canvasEditing;
  modeBtn.textContent = canvasEditing ? "Preview" : "Edit";
  modeBtn.setAttribute("aria-pressed", canvasEditing ? "true" : "false");
}

modeBtn.addEventListener("click", async () => {
  if (!isOwner()) {
    const key = await needKey();
    if (!key) return;
    canvasEditing = true;
  } else {
    canvasEditing = !canvasEditing;
  }
  syncOwnerUI();
  renderCanvas();
});

organizeBtn.addEventListener("click", () => organizeCanvas(organizeBy.value));

addTextBtn.addEventListener("click", () => {
  canvasBlocks.push({
    id: newBlockId(),
    kind: "text",
    x: 4,
    y: nextBlockY(),
    w: 44,
    text: "",
    created: Date.now(),
  });
  renderCanvas();
  saveCanvas();
});

lockBtn.addEventListener("click", async () => {
  const slug = slugFromPath();
  if (!slug) return;
  const key = await needKey();
  if (!key) return;
  const input = document.getElementById("setpass-input");
  const errEl = document.getElementById("setpass-error");
  const help = document.getElementById("setpass-help");
  const removeBtn = document.getElementById("setpass-remove");
  errEl.textContent = "";
  input.value = "";
  // Keyless probe: the owner key bypasses the lock, so only an
  // unauthenticated listing 401s when the gallery is protected.
  let locked = false;
  try {
    const probe = await fetch("/api/galleries/" + slug + "/photos");
    locked = probe.status === 401;
  } catch (_) {}
  removeBtn.hidden = !locked;
  help.textContent = locked
    ? "This gallery is protected. Enter a new password to change it, or remove protection below."
    : "Viewers will need this password to open the gallery.";
  document.getElementById("setpass-dialog").showModal();
  input.focus();
});

document.getElementById("setpass-form").addEventListener("submit", async () => {
  const clicked = document.activeElement && document.activeElement.value;
  if (clicked === "cancel") return;
  const slug = slugFromPath();
  const input = document.getElementById("setpass-input");
  const errEl = document.getElementById("setpass-error");
  if (clicked !== "remove" && !input.value) return; // empty = keep, no request
  const key = galleryKey;
  try {
    const res = await fetch("/api/galleries/" + slug + "/lock", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
      body: JSON.stringify({ password: clicked === "remove" ? "" : input.value }),
    });
    if (!res.ok) {
      let msg = res.statusText;
      try { const e = await res.json(); if (e.error) msg = e.error; } catch (_) {}
      errEl.textContent = msg;
      document.getElementById("setpass-dialog").showModal();
      return;
    }
    const data = await res.json().catch(() => null);
    uploadStatus.textContent = data && data.locked ? "Gallery protected." : "Protection removed.";
    renderCanvas();
  } catch (_) {
    errEl.textContent = "Request failed";
    document.getElementById("setpass-dialog").showModal();
  }
});

window.addEventListener("resize", resizeSurface);

/** Gallery metadata for the block picker; also the existence + lock gate. */
async function loadPhotos() {
  const slug = slugFromPath();
  if (!slug) return false;
  galleryLabel.textContent = slug;
  try {
    const res = await fetch("/api/galleries/" + slug + "/photos", { headers: passHeader() });
    if (res.status === 404) {
      // Gallery deleted: back to the landing page.
      location.assign("/");
      return false;
    }
    if (res.status === 401) {
      // Locked: prompt for the viewer password (owner key also accepted).
      if (await needPass(slug)) return loadPhotos();
      canvasMessage("This gallery is locked.");
      return false;
    }
    if (!res.ok) throw new Error("bad status " + res.status);
    const data = await res.json();
    photos = data.photos || [];
    renderMediaTray();
    return true;
  } catch (_) {
    canvasMessage("Could not load gallery");
    return false;
  }
}

async function boot() {
  const slug = slugFromPath();
  if (!slug) {
    // Landing page: uploads mint a gallery, nothing else to show.
    canvasEl.hidden = true;
    canvasBar.hidden = true;
    return;
  }
  canvasBar.hidden = false;
  syncOwnerUI();
  if (await loadPhotos()) await loadCanvas(slug);
}

boot();
