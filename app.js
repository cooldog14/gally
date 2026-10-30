"use strict";

const grid = document.getElementById("grid");
const empty = document.getElementById("empty");
const lightbox = document.getElementById("lightbox");
const viewport = document.getElementById("viewport");
const counter = document.getElementById("counter");
const prevBtn = document.getElementById("prev");
const nextBtn = document.getElementById("next");
const closeBtn = document.getElementById("close");
const fileInput = document.getElementById("file-input");
const galleryLabel = document.getElementById("gallery-label");
const keyDialog = document.getElementById("key-dialog");
const keyForm = document.getElementById("key-form");
const keyInput = document.getElementById("key-input");
const keyError = document.getElementById("key-error");

const MAX_BYTES = 10 * 1024 * 1024 * 1024;

const SLUG_RE = /^(?:oddball-slug-extra-[0-9a-f]{8}|[a-z]+-[a-z]+-[a-z]+-[a-z]+-[0-9a-f]{4})$/;

let photos = [];
let index = 0;
// Gallery key: kept in memory + sessionStorage (per-tab). Never in the URL.
let galleryKey = sessionStorage.getItem("gallery-key") || "";
let keyResolver = null;
// Viewer password for locked galleries: "<slug>:<password>" per tab.
function storedPass(slug) {
  try { return sessionStorage.getItem("gallery-pass:" + slug) || ""; }
  catch (_) { return ""; }
}
// Edit mode: tap tiles to select, Delete acts on the selection.
let editing = false;
const selected = new Set();
const selCount = document.getElementById("sel-count");
const uploadBtn = document.getElementById("upload-btn");
const uploadStatus = document.getElementById("upload-status");
const themeBtn = document.getElementById("theme-btn");
const editBtn = document.getElementById("edit-btn");
const editBar = document.getElementById("editbar");
const deleteBtn = document.getElementById("delete-btn");
const lockBtn = document.getElementById("lock-btn");
const doneBtn = document.getElementById("done-btn");
const confirmDialog = document.getElementById("confirm-dialog");
const confirmText = document.getElementById("confirm-text");
const confirmForm = document.getElementById("confirm-form");

function slugFromPath() {
  const seg = location.pathname.split("/").filter(Boolean)[0] || "";
  return SLUG_RE.test(seg) ? seg : "";
}

async function loadPhotos() {
  const slug = slugFromPath();
  if (!slug) {
    // Landing page: no gallery yet. Upload prompts for a key.
    galleryLabel.textContent = "";
    editBtn.hidden = true;
    setEditing(false);
    photos = [];
    renderGrid();
    return;
  }
  galleryLabel.textContent = slug;
  editBtn.hidden = false;
  try {
    const headers = {};
    if (galleryKey) headers["X-Gallery-Key"] = galleryKey;
    else {
      const p = storedPass(slug);
      if (p) headers["X-Gallery-Pass"] = p;
    }
    const res = await fetch("/api/galleries/" + slug + "/photos", { headers });
    if (res.status === 404) {
      // Gallery deleted/emptied away: back to the landing page.
      location.assign("/");
      return;
    }
    if (res.status === 401) {
      // Locked: prompt for the viewer password (owner key also accepted).
      const ok = await needPass(slug);
      if (ok) return loadPhotos();
      photos = [];
      renderGrid();
      empty.hidden = false;
      empty.textContent = "This gallery is locked.";
      return;
    }
    if (!res.ok) throw new Error("bad status " + res.status);
    const data = await res.json();
    photos = data.photos || [];
    // Drop selection for photos that no longer exist.
    for (const name of Array.from(selected)) {
      if (!photos.some((p) => p.name === name)) selected.delete(name);
    }
    renderGrid();
    updateEditUI();
  } catch (_) {
    empty.hidden = false;
    empty.textContent = "Could not load photos";
  }
}

function setEditing(on) {
  editing = on;
  if (!on) selected.clear();
  editBar.hidden = !on;
  editBtn.textContent = on ? "Editing…" : "Edit";
  uploadBtn.hidden = on;
  updateEditUI();
}

function updateEditUI() {
  selCount.textContent = selected.size + " selected";
  deleteBtn.disabled = selected.size === 0;
  renderGrid();
}

editBtn.addEventListener("click", async () => {
  if (editing) {
    setEditing(false);
    return;
  }
  const key = await needKey();
  if (!key) return;
  setEditing(true);
});

doneBtn.addEventListener("click", () => setEditing(false));

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
    await loadPhotos();
  } catch (_) {
    errEl.textContent = "Request failed";
    document.getElementById("setpass-dialog").showModal();
  }
});

deleteBtn.addEventListener("click", () => {
  if (!selected.size) return;
  confirmText.textContent = "Delete " + selected.size + (selected.size === 1 ? " photo?" : " photos?") + " This cannot be undone.";
  confirmDialog.showModal();
});
confirmForm.addEventListener("submit", async () => {
  if (document.activeElement && document.activeElement.value === "cancel") return;
  const names = Array.from(selected);
  const key = galleryKey;
  const failures = [];
  await Promise.all(names.map(async (name) => {
    try {
      const res = await fetch("/api/photos/" + encodeURIComponent(name), {
        method: "DELETE",
        headers: { "X-Gallery-Key": key },
      });
      if (!res.ok) {
        let msg = res.statusText;
        try { const e = await res.json(); if (e.error) msg = e.error; } catch (_) {}
        failures.push(displayName(name) + ": " + msg);
      }
    } catch (_) {
      failures.push(displayName(name) + ": delete failed");
    }
  }));
  uploadStatus.textContent = failures.length ? failures.join("; ") : "";
  await loadPhotos();
});

function displayName(stored) {
  const i = stored.indexOf("_");
  const j = stored.indexOf("_", i + 1);
  return j >= 0 ? stored.slice(j + 1) : stored;
}

function mediaKind(name) {
  const dot = name.lastIndexOf(".");
  const ext = dot >= 0 ? name.slice(dot).toLowerCase() : "";
  if ([".heic", ".heif"].includes(ext)) return "heic";
  if ([".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".bmp", ".svg", ".ico"].includes(ext)) return "image";
  if ([".mp4", ".webm", ".ogv", ".ogm", ".mov"].includes(ext)) return "video";
  if ([".mp3", ".wav", ".flac", ".m4a", ".ogg", ".oga", ".opus"].includes(ext)) return "audio";
  if ([".txt", ".md", ".markdown", ".json", ".csv", ".log", ".xml", ".yaml", ".yml", ""].includes(ext)) return "text";
  return "file";
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

// Same credential as a header (for fetch(), which unlike <img>/<video>
// tags can set headers).
function passHeader() {
  const slug = slugFromPath();
  const p = slug ? storedPass(slug) : "";
  const headers = {};
  if (galleryKey) headers["X-Gallery-Key"] = galleryKey;
  else if (p) headers["X-Gallery-Pass"] = p;
  return headers;
}

async function authedFetch(name) {
  const url = mediaUrl(name);
  const headers = passHeader();
  const q = url.indexOf("?pass=");
  if (q >= 0) return fetch(url.slice(0, q), { headers });
  return fetch(url, { headers });
}


// --- HEIC thumbnails via vendored WASM (Safari decodes natively) ---
const heicCache = new Map(); // name -> objectURL (LRU, capped)
// Cap ≈ two screens of tiles: 36 on desktop grids, 12 on phones.
function heicCacheMax() {
  const cols = Math.max(1, Math.floor((grid.clientWidth || 800) / 200));
  const rows = Math.max(1, Math.ceil((window.innerHeight || 600) / 200));
  const twoScreens = cols * rows * 2;
  return Math.min(36, Math.max(12, twoScreens));
}
let heicNative = null; // null = untested

function heicSupportedNatively() {
  if (heicNative !== null) return Promise.resolve(heicNative);
  // 1px HEIC probe: Safari resolves, others reject.
  const probe = "data:image/heic;base64,";
  return new Promise((resolve) => {
    const img = new Image();
    img.onload = () => { heicNative = true; resolve(true); };
    img.onerror = () => { heicNative = false; resolve(false); };
    img.src = probe;
  });
}

async function heicThumb(name) {
  if (heicCache.has(name)) {
    // Refresh LRU position.
    const url = heicCache.get(name);
    heicCache.delete(name);
    heicCache.set(name, url);
    return url;
  }
  if (typeof heic2any === "undefined") throw new Error("decoder unavailable");
  const res = await authedFetch(name);
  if (!res.ok) throw new Error("fetch failed");
  const blob = await res.blob();
  const out = await heic2any({ blob, toType: "image/jpeg", quality: 0.8 });
  const url = URL.createObjectURL(Array.isArray(out) ? out[0] : out);
  heicCache.set(name, url);
  const max = heicCacheMax();
  while (heicCache.size > max) {
    const [oldest, oldUrl] = heicCache.entries().next().value;
    heicCache.delete(oldest);
    URL.revokeObjectURL(oldUrl);
  }
  return url;
}
function tileMedia(photo) {
  const kind = mediaKind(photo.name);
  const url = mediaUrl(photo.name);
  if (kind === "heic") {
    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = displayName(photo.name);
    img.dataset.heic = photo.name;
    img.src = url; // Safari renders natively; others fall to onerror.
    img.onerror = () => {
      img.onerror = null;
      heicSupportedNatively().then((native) => {
        if (native) return; // transient error, keep broken img.
        heicThumb(photo.name).then((obj) => {
          if (img.isConnected && img.dataset.heic === photo.name) img.src = obj;
          else URL.revokeObjectURL(obj);
        }).catch(() => {
          const div = document.createElement("div");
          div.className = "file-generic";
          div.textContent = displayName(photo.name);
          div.title = displayName(photo.name) + " (preview not supported)";
          img.replaceWith(div);
        });
      });
    };
    // IntersectionObserver: decode only when the tile nears the viewport.
    if (typeof IntersectionObserver !== "undefined") {
      const io = new IntersectionObserver((entries, obs) => {
        if (entries.some((e) => e.isIntersecting)) {
          obs.disconnect();
          if (img.isConnected && !img.complete) { /* native path handles it */ }
          heicSupportedNatively().then((native) => {
            if (!native && img.isConnected) {
              heicThumb(photo.name).then((obj) => {
                if (img.isConnected && img.dataset.heic === photo.name && !img.complete) img.src = obj;
              }).catch(() => {});
            }
          });
        }
      }, { rootMargin: "500px" });
      io.observe(img);
    }
    return img;
  }
  if (kind === "image") {
    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = displayName(photo.name);
    img.src = url;
    return img;
  }
  if (kind === "video") {
    const v = document.createElement("video");
    v.muted = true;
    v.playsInline = true;
    v.preload = "metadata";
    v.src = url;
    v.onerror = () => {
      // Codec not playable (e.g. ProRes MOV): filename tile instead.
      v.onerror = null;
      const div = document.createElement("div");
      div.className = "file-generic";
      div.textContent = displayName(photo.name);
      div.title = displayName(photo.name) + " (video not playable here — download to view)";
      v.replaceWith(div);
    };
    return v;
  }
  const div = document.createElement("div");
  div.className = "file-generic";
  div.textContent = displayName(photo.name);
  div.title = displayName(photo.name);
  return div;
}

function renderGrid() {
  grid.textContent = "";
  empty.hidden = photos.length > 0;
  empty.textContent = "Empty gallery.";
  photos.forEach((photo, i) => {
    const tile = document.createElement("button");
    tile.className = "tile" + (selected.has(photo.name) ? " selected" : "");
    tile.setAttribute("aria-pressed", selected.has(photo.name) ? "true" : "false");
    tile.appendChild(tileMedia(photo));
    const tick = document.createElement("span");
    tick.className = "tick";
    tick.textContent = "\u2713";
    tile.appendChild(tick);
    tile.addEventListener("click", () => {
      if (editing) {
        if (selected.has(photo.name)) selected.delete(photo.name);
        else selected.add(photo.name);
        updateEditUI();
      } else {
        openLightbox(i);
      }
    });
    grid.appendChild(tile);
  });
}

function render() {
  counter.textContent = (index + 1) + " / " + photos.length;
}

/// Show photo i in the overlay by adopting its (already loaded) grid
/// thumbnail: no second fetch, no src swap, no black frame.
function openLightbox(i) {
  if (!photos.length) { closeLightbox(); return; }
  index = Math.min(Math.max(i, 0), photos.length - 1);
  showInViewport(index);
  lightbox.hidden = false;
  render();
}

function showInViewport(i) {
  stopViewportMedia();
  viewport.textContent = "";
  const name = photos[i].name;
  const kind = mediaKind(name);
  const url = mediaUrl(name);
  if (kind === "heic") {
    viewport.textContent = "Converting…";
    heicThumb(name).then((obj) => {
      if (index !== i || photos[i].name !== name) return;
      viewport.textContent = "";
      const img = new Image();
      img.alt = displayName(name);
      img.src = obj;
      viewport.appendChild(img);
    }).catch(() => {
      viewport.textContent = "";
      const div = document.createElement("div");
      div.className = "file-generic";
      div.textContent = displayName(name);
      viewport.appendChild(div);
    });
    return;
  }
  if (kind === "audio") {
    const a = document.createElement("audio");
    a.controls = true;
    a.src = url;
    viewport.appendChild(a);
    return;
  }
  if (kind === "text") {
    viewport.textContent = "Loading…";
    authedFetch(name).then((r) => {
      if (!r.ok) throw new Error("fetch failed");
      return r.text();
    }).then((text) => {
      if (index !== i || photos[i].name !== name) return;
      viewport.textContent = "";
      const pre = document.createElement("pre");
      pre.textContent = text;
      viewport.appendChild(pre);
    }).catch(() => {
      viewport.textContent = "";
      viewport.appendChild(tileMedia(photos[i]));
    });
    return;
  }
  const tile = grid.children[i];
  const media = tile ? tile.querySelector("img, video, .file-generic") : null;
  if (media && (media.tagName !== "IMG" || (media.complete && media.naturalWidth))) {
    viewport.appendChild(media);
    if (media.tagName === "VIDEO") { media.controls = true; }
  } else if (media) {
    // Not decoded yet: re-create at full size, swap when ready.
    viewport.appendChild(media);
  } else {
    viewport.appendChild(tileMedia(photos[i]));
  }
  const v = viewport.querySelector("video");
  if (v) { v.controls = true; v.muted = false; }
  // Non-renderable file: offer a download link under the filename.
  const g = viewport.querySelector(".file-generic");
  if (g && !g.querySelector("a")) {
    const a = document.createElement("a");
    a.href = mediaUrl(photos[i].name);
    a.textContent = "Download";
    g.appendChild(a);
  }
}

function stopViewportMedia() {
  const v = viewport.querySelector("video");
  if (v) { try { v.pause(); } catch (_) {} }
}

function restoreThumbnails() {
  // Move any adopted media back to its tile.
  stopViewportMedia();
  const media = viewport.querySelector("img, video, .file-generic");
  if (media) {
    if (media.tagName === "VIDEO") { media.controls = false; media.muted = true; }
    const src = media.currentSrc || media.src;
    const tile = Array.from(grid.children).find((t) => {
      const o = t.querySelector("img, video, .file-generic");
      return o && src && (o.currentSrc === src || o.src === src);
    });
    if (tile && !tile.querySelector("img, video, .file-generic")) tile.prepend(media);
    else {
      const name = Array.from(photos).find((p) =>
        src && src.endsWith("/photos/" + encodeURIComponent(p.name)))?.name;
      const idx = name ? photos.findIndex((p) => p.name === name) : -1;
      if (idx >= 0 && grid.children[idx] && !grid.children[idx].querySelector("img, video, .file-generic"))
        grid.children[idx].prepend(media);
    }
  }
  viewport.textContent = "";
}

function closeLightbox() {
  restoreThumbnails();
  lightbox.hidden = true;
}

function step(d) {
  if (!photos.length) { closeLightbox(); return; }
  index = (index + d + photos.length) % photos.length;
  restoreThumbnails();
  showInViewport(index);
  render();
}

prevBtn.addEventListener("click", () => step(-1));
nextBtn.addEventListener("click", () => step(1));
closeBtn.addEventListener("click", closeLightbox);
document.addEventListener("keydown", (e) => {
  if (lightbox.hidden) return;
  if (e.key === "ArrowLeft") step(-1);
  else if (e.key === "ArrowRight") step(1);
  else if (e.key === "Escape") closeLightbox();
});

/** Resolve the gallery key: memory/session, else prompt once.
    Landing page (no slug) always prompts: returning here means a new gallery. */
function needKey(forcePrompt) {
  if (!forcePrompt && galleryKey && galleryKey.length >= 12) return Promise.resolve(galleryKey);
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
// server answers "invite required". Contributing to an existing gallery
// never needs it.
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

keyForm.addEventListener("submit", (e) => {
  const v = keyInput.value.trim();
  if (v.length < 12) {
    e.preventDefault(); // keep the dialog open (Esc still cancels)
    keyError.textContent = "Key must be at least 12 characters";
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
  uploadStatus.textContent = failures.length ? failures.join("; ") : "";
});

themeBtn.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem("theme", next); } catch (_) { /* private mode */ }
});

loadPhotos();
