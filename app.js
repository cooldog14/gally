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
const keyHelp = document.getElementById("key-help");
const publicCheck = document.getElementById("public-check");
const editStatus = document.getElementById("edit-status");
const keyBanner = document.getElementById("key-banner");
const keyBannerKey = document.getElementById("key-banner-key");

const MAX_BYTES = 10 * 1024 * 1024 * 1024;

const SLUG_RE = /^(?:oddball-slug-extra-[0-9a-f]{8}|[a-z]+-[a-z]+-[a-z]+-[a-z]+-[0-9a-f]{4})$/;

let photos = [];
let index = 0;
// Whole-gallery visibility; new galleries start private server-side.
let galleryPublic = true;
// Viewer password set? Comes from the listing: an unauthenticated probe gets
// 404 on a private gallery, so it cannot tell whether a lock exists.
let galleryLocked = false;
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

document.getElementById("key-banner-close").addEventListener("click", () => {
  keyBanner.hidden = true;
});

/** One-shot "this is your key" strip: shown once, right after a mint. */
function showKeyBannerOnce(slug) {
  let key = "";
  try {
    key = sessionStorage.getItem("show-key:" + slug) || "";
    if (key) sessionStorage.removeItem("show-key:" + slug);
  } catch (_) { return; }
  if (!key) return;
  keyBannerKey.textContent = key;
  keyBanner.hidden = false;
}

function slugFromPath() {
  const seg = location.pathname.split("/").filter(Boolean)[0] || "";
  return SLUG_RE.test(seg) ? seg : "";
}

async function loadPhotos(retry = false) {
  const slug = slugFromPath();
  if (!slug) {
    // Landing page: no gallery yet. Upload prompts for a key.
    galleryLabel.textContent = "";
    editBtn.hidden = true;
    setEditing(false);
    photos = [];
    publicCheck.checked = true;
    galleryLocked = false;
    renderGrid();
    return;
  }
  galleryLabel.textContent = slug;
  editBtn.hidden = false;
  showKeyBannerOnce(slug);
  try {
    const headers = {};
    let url = "/api/galleries/" + slug + "/photos";
    if (galleryKey) {
      headers["X-Gallery-Key"] = galleryKey;
    }
    // Password as a percent-encoded header, never a URL: fetch() refuses
    // non-ASCII header values, so encodeURIComponent + server-side decode.
    // The listing's Set-Cookie then authorises every media tag.
    const p = storedPass(slug);
    if (p) headers["X-Gallery-Pass"] = encodeURIComponent(p);
    const res = await fetch(url, { headers });
    if (res.status === 404) {
      // Private (or deleted): a key is the only door. Ask once, then leave.
      if (!retry) {
        const key = await needKey(true, "enter");
        if (key) return loadPhotos(true);
      }
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
    galleryPublic = data.public !== false;
    galleryLocked = data.locked === true;
    publicCheck.checked = galleryPublic;
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
  editStatus.textContent = "";
  document.body.classList.toggle("editing", on);
  if (!on) {
    selected.clear();
    // Unmark tiles in place: never rebuild the grid (media would reload).
    for (const tile of grid.children) {
      tile.classList.remove("selected");
      tile.setAttribute("aria-pressed", "false");
    }
  }
  editBar.hidden = !on;
  editBtn.textContent = on ? "Editing…" : "Edit";
  uploadBtn.hidden = on;
  updateEditUI();
}

function updateEditUI() {
  selCount.textContent = selected.size + " selected";
  deleteBtn.disabled = selected.size === 0;
}

editBtn.addEventListener("click", async () => {
  if (editing) {
    setEditing(false);
    return;
  }
  const key = await needKey(false, "enter");
  if (!key) return;
  setEditing(true);
});

doneBtn.addEventListener("click", () => setEditing(false));

// Publish/unpublish the whole gallery. Reverts the tick if the server refuses.
publicCheck.addEventListener("change", async () => {
  const slug = slugFromPath();
  if (!slug) return;
  const want = publicCheck.checked;
  publicCheck.disabled = true;
  editStatus.textContent = "";
  try {
    const res = await fetch("/api/galleries/" + slug + "/visibility", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Gallery-Key": galleryKey },
      body: JSON.stringify({ public: want }),
    });
    if (!res.ok) {
      let msg = res.statusText;
      try { const e = await res.json(); if (e.error) msg = e.error; } catch (_) {}
      throw new Error(msg);
    }
    galleryPublic = want;
    editStatus.textContent = want ? "Gallery is public" : "Gallery is private";
  } catch (e) {
    publicCheck.checked = galleryPublic;
    editStatus.textContent = e.message;
  } finally {
    publicCheck.disabled = false;
  }
});

lockBtn.addEventListener("click", async () => {
  const slug = slugFromPath();
  if (!slug) return;
  const key = await needKey(false, "enter");
  if (!key) return;
  const input = document.getElementById("setpass-input");
  const errEl = document.getElementById("setpass-error");
  const help = document.getElementById("setpass-help");
  const removeBtn = document.getElementById("setpass-remove");
  errEl.textContent = "";
  input.value = "";
  removeBtn.hidden = !galleryLocked;
  help.textContent = galleryLocked
    ? "This gallery is protected. Enter a new password to change it, or remove protection below."
    : "Viewers will need this password to open the gallery.";
  document.getElementById("setpass-dialog").showModal();
  input.focus();
});

document.getElementById("setpass-form").addEventListener("submit", async (e) => {
  const clicked = e.submitter?.value;
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
    try {
      if (clicked === "remove") sessionStorage.removeItem("gallery-pass:" + slug);
      else sessionStorage.setItem("gallery-pass:" + slug, input.value);
    } catch (_) {}
    uploadStatus.textContent = data && data.locked ? "Gallery protected." : "Protection removed.";
    await loadPhotos();
  } catch (_) {
    errEl.textContent = "Request failed";
    document.getElementById("setpass-dialog").showModal();
  }
});

deleteBtn.addEventListener("click", () => {
  if (!selected.size) return;
  pendingGalleryDelete = false;
  confirmText.textContent = "Delete " + selected.size + (selected.size === 1 ? " photo?" : " photos?") + " This cannot be undone.";
  confirmDialog.showModal();
});
// Gallery delete ignores the selection: confirm once, then leave for home.
let pendingGalleryDelete = false;
document.getElementById("delete-gallery-btn").addEventListener("click", () => {
  pendingGalleryDelete = true;
  confirmText.textContent = "Delete this gallery and all " + photos.length + (photos.length === 1 ? " photo?" : " photos?") + " This cannot be undone.";
  confirmDialog.showModal();
});
confirmForm.addEventListener("submit", async (e) => {
  if (!e.submitter || e.submitter.value === "cancel") { pendingGalleryDelete = false; return; }
  if (pendingGalleryDelete) {
    pendingGalleryDelete = false;
    try {
      const res = await fetch("/api/galleries/" + slugFromPath(), {
        method: "DELETE",
        headers: { "X-Gallery-Key": galleryKey },
      });
      if (!res.ok) {
        let msg = res.statusText;
        try { const e = await res.json(); if (e.error) msg = e.error; } catch (_) {}
        uploadStatus.textContent = msg;
        return;
      }
      location.assign("/");
    } catch (_) {
      uploadStatus.textContent = "delete failed";
    }
    return;
  }
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

// Media URLs carry no secret: an owner key or the viewer cookie set by the
// listing authorises <img>/<video>/<audio>, which cannot send a header.
function mediaUrl(name) {
  return "/photos/" + encodeURIComponent(name);
}

// Owner key as a header; viewer password goes on the listing fetch only
// (percent-encoded: fetch() refuses non-ASCII header values).
async function authedFetch(name) {
  const headers = {};
  if (galleryKey) headers["X-Gallery-Key"] = galleryKey;
  return fetch(mediaUrl(name), { headers });
}


// --- HEIC: one fetch, render natively, decode only when the browser can't ---
const heicCache = new Map();    // name -> displayed objectURL (LRU, capped)
const heicBlobs = new Map();    // name -> {blob,url} fetched, not yet displayed
const heicInflight = new Map(); // phase+name -> shared promise (one per photo)

// Cap ≈ two screens of tiles: 36 on desktop grids, 12 on phones.
function heicCacheMax() {
  const cols = Math.max(1, Math.floor((grid.clientWidth || 800) / 200));
  const rows = Math.max(1, Math.ceil((window.innerHeight || 600) / 200));
  const twoScreens = cols * rows * 2;
  return Math.min(36, Math.max(12, twoScreens));
}

function heicOnce(key, fn) {
  let p = heicInflight.get(key);
  if (!p) {
    p = fn();
    heicInflight.set(key, p);
    p.catch(() => {}).then(() => heicInflight.delete(key));
  }
  return p;
}

function heicRemember(name, url) {
  heicCache.delete(name);
  heicCache.set(name, url);
  while (heicCache.size > heicCacheMax()) {
    const [oldest, oldUrl] = heicCache.entries().next().value;
    heicCache.delete(oldest);
    URL.revokeObjectURL(oldUrl);
  }
}

function heicCached(name) {
  const url = heicCache.get(name);
  if (url === undefined) return null;
  heicRemember(name, url); // refresh LRU position
  return url;
}

// 1.3 MB decoder: injected only when a HEIC actually needs decoding.
let heicDecoder = null;
function loadHeicDecoder() {
  if (!heicDecoder) {
    heicDecoder = new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = "./vendor-heic2any.min.js";
      s.onload = () => resolve();
      s.onerror = () => { heicDecoder = null; reject(new Error("decoder unavailable")); };
      document.head.appendChild(s);
    });
  }
  return heicDecoder;
}

// Fetched once per photo; heicBlobs owns the blob URL until it is displayed.
function heicBlob(name) {
  return heicOnce("blob:" + name, async () => {
    const res = await authedFetch(name);
    if (!res.ok) throw new Error("fetch failed");
    const blob = await res.blob();
    const entry = { blob, url: URL.createObjectURL(blob) };
    heicBlobs.delete(name);
    heicBlobs.set(name, entry);
    while (heicBlobs.size > heicCacheMax()) {
      const [oldest, evicted] = heicBlobs.entries().next().value;
      heicBlobs.delete(oldest);
      URL.revokeObjectURL(evicted.url);
    }
    return entry;
  });
}

function heicSource(name) {
  const cached = heicCached(name);
  return cached ? Promise.resolve(cached) : heicBlob(name).then((entry) => entry.url);
}

// Decode the already-fetched blob; cached, so concurrent callers share it.
function heicDecode(name) {
  return heicOnce("jpeg:" + name, async () => {
    const cached = heicCached(name);
    if (cached) return cached;
    await loadHeicDecoder();
    const entry = heicBlobs.get(name) || await heicBlob(name);
    const out = await heic2any({ blob: entry.blob, toType: "image/jpeg", quality: 0.8 });
    const jpeg = URL.createObjectURL(Array.isArray(out) ? out[0] : out);
    if (heicBlobs.get(name) === entry) heicBlobs.delete(name);
    URL.revokeObjectURL(entry.url); // intermediate: this browser can't show it
    heicRemember(name, jpeg);
    return jpeg;
  });
}

// <img> for a HEIC. Tiles start on intersect (one observer, no double trigger);
// `immediate` starts now for the visible lightbox image.
function heicImage(name, onFail, immediate = false) {
  const img = document.createElement("img");
  img.alt = displayName(name);
  img.dataset.heic = name;
  let started = false;
  const start = () => {
    if (started) return;
    started = true;
    heicSource(name).then((url) => {
      if (!img.isConnected) return; // the URL stays owned by heicBlobs/heicCache
      img.onload = () => {
        heicRemember(name, url);
        const owned = heicBlobs.get(name);
        if (owned && owned.url === url) heicBlobs.delete(name); // now heicCache's
      };
      img.onerror = () => {
        img.onerror = null;
        heicDecode(name).then((jpeg) => {
          img.onload = null; // the JPEG is cached by heicDecode, not this URL
          img.src = jpeg;
        }).catch(() => { if (img.isConnected) onFail(); });
      };
      img.src = url;
    }).catch(() => { if (img.isConnected) onFail(); });
  };
  if (immediate || typeof IntersectionObserver === "undefined") {
    start();
  } else {
    const io = new IntersectionObserver((entries, obs) => {
      if (entries.some((e) => e.isIntersecting)) {
        obs.disconnect();
        start();
      }
    }, { rootMargin: "500px" });
    io.observe(img);
  }
  return img;
}
function tileMedia(photo) {
  const kind = mediaKind(photo.name);
  const url = mediaUrl(photo.name);
  if (kind === "heic") {
    const img = heicImage(photo.name, () => {
      const div = document.createElement("div");
      div.className = "file-generic";
      div.textContent = displayName(photo.name);
      div.title = displayName(photo.name) + " (preview not supported)";
      img.replaceWith(div);
    });
    img.loading = "lazy"; // off-screen tiles still defer decode/render
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
        const now = !selected.has(photo.name);
        if (now) selected.add(photo.name);
        else selected.delete(photo.name);
        tile.classList.toggle("selected", now);
        tile.setAttribute("aria-pressed", String(now));
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

// Tile index whose media is currently adopted by the lightbox (-1 = none).
let adoptedTile = -1;

function showInViewport(i) {
  stopViewportMedia();
  viewport.textContent = "";
  adoptedTile = -1;
  const name = photos[i].name;
  const kind = mediaKind(name);
  const url = mediaUrl(name);
  if (kind === "heic") {
    // Native Safari renders the blob; others swap in the decoded JPEG.
    viewport.appendChild(heicImage(name, () => {
      viewport.textContent = "";
      const div = document.createElement("div");
      div.className = "file-generic";
      div.textContent = displayName(name);
      viewport.appendChild(div);
    }, true));
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
    adoptedTile = i;
    if (media.tagName === "VIDEO") { media.controls = true; }
  } else if (media) {
    // Not decoded yet: re-create at full size, swap when ready.
    viewport.appendChild(media);
    adoptedTile = i;
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
  // Put the adopted media back where it came from (by index, not by URL).
  stopViewportMedia();
  if (adoptedTile >= 0) {
    const media = viewport.querySelector("img, video, .file-generic");
    const tile = grid.children[adoptedTile];
    if (media && tile && !tile.querySelector("img, video, .file-generic")) {
      if (media.tagName === "VIDEO") { media.controls = false; media.muted = true; }
      tile.prepend(media);
    }
  }
  adoptedTile = -1;
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

// The key is hand-typed, never generated: the user must remember (or note)
// what they wrote. Keys are 6-128 visible ASCII chars (header-safe);
// new keys also need 12+ chars, a capital and a symbol.

/** Why a *new* key is not acceptable yet ("" = fine). */
function newKeyProblem(v) {
  if (!/^[\x21-\x7e]{12,128}$/.test(v)) return "Key must be 12-128 ASCII characters (no spaces/accents)";
  if (!/\p{Lu}/u.test(v)) return "Key needs a capital letter";
  if (!/[^\p{L}\p{N}]/u.test(v)) return "Key needs a symbol, like ! - _ #";
  return "";
}

/** Resolve the gallery key: memory/session, else prompt once.
    Landing page (no slug) always prompts: returning here means a new gallery. */
let keyMode = "enter";
function needKey(forcePrompt, mode = "enter") {
  if (!forcePrompt && galleryKey && galleryKey.length >= 6) {
    return Promise.resolve(galleryKey);
  }
  resetKeyDialog();
  keyMode = mode;
  const create = mode === "create";
  document.getElementById("key-title").textContent = create ? "New gallery key" : "Gallery key";
  keyHelp.textContent = create
    ? "At least 12 characters, with a capital letter and a symbol (like Sunset-Beach-42). Save it; it is the only way to edit this gallery."
    : "Enter the key for this gallery.";
  keyInput.value = "";
  keyError.textContent = "";
  keyDialog.showModal();
  keyInput.focus();
  return new Promise((resolve) => { keyResolver = resolve; });
}

function resetKeyDialog() {
  invitePrompt = false;
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

document.getElementById("pass-form").addEventListener("submit", (e) => {
  if (!passResolver) return;
  const { resolve, slug } = passResolver;
  const input = document.getElementById("pass-input");
  if (e.submitter?.value === "cancel") {
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
document.getElementById("pass-cancel").addEventListener("click", () => {
  document.getElementById("pass-dialog").close();
});
document.getElementById("setpass-cancel").addEventListener("click", () => {
  document.getElementById("setpass-dialog").close();
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
  resetKeyDialog();
  invitePrompt = true;
  document.getElementById("key-title").textContent = "Invite code";
  keyHelp.textContent = "Ask the gallery owner for the invite code.";
  keyInput.value = "";
  keyError.textContent = "";
  keyDialog.showModal();
  keyInput.focus();
  return new Promise((resolve) => { keyResolver = resolve; });
}

keyForm.addEventListener("submit", (e) => {
  const v = keyInput.value.trim();
  if (invitePrompt) {
    if (!/^[\x21-\x7e]{1,128}$/.test(v)) {
      e.preventDefault();
      keyError.textContent = !v ? "Invite code required" : "Invite code must be ASCII (no spaces/accents)";
      keyInput.focus();
      return;
    }
    inviteCode = v;
    resetKeyDialog();
    if (keyResolver) keyResolver(v);
    keyResolver = null;
    return;
  }
  // Strong rules only for keys we create; an existing key just has to be one.
  const problem = keyMode === "create" ? newKeyProblem(v) : (!/^[\x21-\x7e]{6,128}$/.test(v) ? "Key must be 6-128 ASCII characters (no spaces/accents)" : "");
  if (problem) {
    e.preventDefault(); // keep the dialog open (Esc still cancels)
    keyError.textContent = problem;
    keyInput.focus();
    return;
  }
  galleryKey = v;
  try { sessionStorage.setItem("gallery-key", v); } catch (_) { /* private mode */ }
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

/** Mint one gallery for a whole home upload batch (invite asked once). */
async function newGallery(key) {
  for (;;) {
    const headers = { "Content-Type": "application/json", "X-Gallery-Key": key };
    if (inviteCode) headers["X-Invite-Code"] = inviteCode;
    let res;
    try {
      res = await fetch("/api/galleries/resolve", { method: "POST", headers, body: "{}" });
    } catch (_) {
      return "";
    }
    const data = await res.json().catch(() => null);
    if (res.ok && data && data.slug) return data.slug;
    if (res.status !== 403 || !data || data.error !== "invite required") return "";
    if (!(await needInvite())) return "";
  }
}

fileInput.addEventListener("change", async () => {
  const key = await needKey(!slugFromPath(), slugFromPath() ? "enter" : "create");
  if (!key) { fileInput.value = ""; return; }
  const files = Array.from(fileInput.files);
  fileInput.value = "";
  if (!files.length) return;
  const failures = [];
  // One gallery per batch, picked before the first byte: if one upload's
  // reply is lost, the next file must not mint a second gallery that the
  // browser never navigates to.
  const pageSlug = slugFromPath();
  const target = pageSlug || (await newGallery(key));
  if (!target) {
    uploadStatus.textContent = "Could not create gallery";
    return;
  }
  let stored = 0;
  async function putFile(file) {
    const headers = { "X-Gallery-Key": key, "X-Gallery-Slug": target };
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
    const res = await putFile(file);
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
    stored += 1;
  }
  if (pageSlug) {
    // Gallery page: show the new tiles without a manual reload.
    await loadPhotos(true);
    uploadStatus.textContent = failures.length ? failures.join("; ") : "";
    return;
  }
  if (!stored) {
    // Nothing landed: stay put so the errors stay visible.
    uploadStatus.textContent = failures.join("; ") || "upload failed";
    return;
  }
  // Carry the key to the next page so the banner can show it once (the field
  // itself is never prefilled again).
  try { sessionStorage.setItem("show-key:" + target, key); } catch (_) {}
  location.assign("/" + target);
});

themeBtn.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem("theme", next); } catch (_) { /* private mode */ }
});

loadPhotos();
