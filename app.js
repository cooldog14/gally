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
const galleryState = document.getElementById("gallery-state");
const homeLink = document.getElementById("home-link");
const keyDialog = document.getElementById("key-dialog");
const keyForm = document.getElementById("key-form");
const keyInput = document.getElementById("key-input");
const keyError = document.getElementById("key-error");
const keyHelp = document.getElementById("key-help");
const keyConfirm = document.getElementById("key-confirm");
const publicCheck = document.getElementById("public-check");
const makePublicBtn = document.getElementById("setpass-public");
const editStatus = document.getElementById("edit-status");
const keyBanner = document.getElementById("key-banner");
const keyBannerKey = document.getElementById("key-banner-key");
const keyBannerCopy = document.getElementById("key-banner-copy");
const loadMoreBtn = document.getElementById("load-more");

const MAX_BYTES = 10 * 1024 * 1024 * 1024;
const UPLOAD_CHUNK = 8 * 1024 * 1024;
const UPLOAD_RETRIES = 5;

/** 32 lowercase hex characters: the server keys the resumable upload on it. */
function randomUploadId() {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Transient failures worth another attempt with the same upload id. */
function retryableStatus(status) {
  return status === 408 || status === 425 || status === 429 || status >= 500;
}

let uploadRetryAt = 0;
async function uploadRequest(url, options) {
  let delay;
  while ((delay = uploadRetryAt - performance.now()) > 0) {
    uploadStatus.textContent = "Upload rate limit reached. Retrying in " + Math.ceil(delay / 1000) + " seconds…";
    await sleep(Math.ceil(delay));
  }
  const res = await fetch(url, options);
  if (res.status === 429) {
    const raw = res.headers.get("Retry-After") || "";
    const n = /^\d+$/.test(raw) ? Number(raw) : NaN;
    // Fall back to the server's one-minute window for older servers.
    const seconds = Number.isSafeInteger(n) && n <= 2_147_483 ? n : 60;
    uploadRetryAt = Math.max(uploadRetryAt, performance.now() + seconds * 1000);
  }
  return res;
}

async function resolveForUpload(url, options) {
  for (let retries = 0; ; retries += 1) {
    const res = await uploadRequest(url, options);
    if (res.status !== 429 || retries === UPLOAD_RETRIES) return res;
    await res.json().catch(() => null);
  }
}

const SLUG_RE = /^(?:oddball-slug-extra-[0-9a-f]{8}|[a-z]+-[a-z]+-[a-z]+-[a-z]+-[0-9a-f]{4}|[a-z]+-[a-z]+-[0-9]{1,3})$/;

let photos = [];
let index = 0;
// Server-side pagination: `next` cursor from the last listing page.
let nextCursor = null;
let loadingMore = false;
// null means the current visibility is not known.
let galleryPublic = null;
let visibilityPending = false;
let visibilityVersion = 0;
let listingVersion = 0;
// Viewer password set? Comes from the listing: an unauthenticated probe gets
// 404 on a private gallery, so it cannot tell whether a lock exists.
let galleryLocked = false;
// Gallery key: kept in memory + sessionStorage (per-tab). Never in the URL.
// Storage can throw when site data is blocked; the page must still load.
let galleryKey = "";
try { galleryKey = sessionStorage.getItem("gallery-key") || ""; } catch (_) {}
let keyPrompt = null;
const ownerKeyRequests = new Map();
// Viewer password for locked galleries: "<slug>:<password>" per tab.
function storedPass(slug) {
  try { return sessionStorage.getItem("gallery-pass:" + slug) || ""; }
  catch (_) { return ""; }
}
// Edit mode: tap tiles to select, Delete acts on the selection.
let editing = false;
const selected = new Set();
// One upload batch at a time; Upload stays disabled until it settles.
let uploading = false;
const selCount = document.getElementById("sel-count");
const uploadBtn = document.getElementById("upload-btn");
const uploadStatus = document.getElementById("upload-status");
const themeBtn = document.getElementById("theme-btn");
const editBtn = document.getElementById("edit-btn");
const editBar = document.getElementById("editbar");
const deleteBtn = document.getElementById("delete-btn");
const lockBtn = document.getElementById("lock-btn");
const showKeyBtn = document.getElementById("show-key-btn");
const doneBtn = document.getElementById("done-btn");
const confirmDialog = document.getElementById("confirm-dialog");
const confirmText = document.getElementById("confirm-text");
const confirmForm = document.getElementById("confirm-form");

document.getElementById("key-banner-close").addEventListener("click", () => {
  keyBanner.hidden = true;
});

function galleryVisibilityHint() {
  if (galleryPublic === null) return "Visibility unknown. Reload to check who can view this gallery.";
  if (galleryPublic) {
    return galleryLocked
      ? "Anyone with the link needs the viewer password."
      : "Anyone with the link can view this gallery.";
  }
  return "This gallery is private. Tick Public in Edit to share it.";
}

function showKeyBanner(key) {
  keyBannerKey.textContent = key;
  keyBanner.hidden = false;
}

/** One-shot "this is your key" strip: shown once, right after a mint. */
function showKeyBannerOnce(slug) {
  let key = "";
  try {
    key = sessionStorage.getItem("show-key:" + slug) || "";
    if (key) sessionStorage.removeItem("show-key:" + slug);
  } catch (_) { return; }
  if (key) showKeyBanner(key);
}

/** Copy without assuming a secure context: LAN HTTP has no Clipboard API. */
async function copyText(text) {
  if (typeof navigator !== "undefined" && navigator.clipboard && navigator.clipboard.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch (_) { /* fall through to the textarea path */ }
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.appendChild(area);
  let ok = false;
  try {
    area.select();
    ok = document.execCommand("copy");
  } catch (_) {}
  area.remove();
  return ok;
}

keyBannerCopy.addEventListener("click", async () => {
  const ok = await copyText(keyBannerKey.textContent);
  keyBannerCopy.textContent = ok ? "Copied" : "Copy failed";
  setTimeout(() => { keyBannerCopy.textContent = "Copy key"; }, 1500);
});

showKeyBtn.addEventListener("click", () => {
  if (!galleryKey) return;
  showKeyBanner(galleryKey);
  if (keyBanner.scrollIntoView) keyBanner.scrollIntoView({ block: "nearest" });
});

function slugFromPath() {
  const seg = location.pathname.split("/").filter(Boolean)[0] || "";
  return SLUG_RE.test(seg) ? seg : "";
}

function updateGalleryState() {
  const slug = slugFromPath();
  galleryState.hidden = !slug;
  homeLink.hidden = !slug;
  if (!slug) return;
  publicCheck.checked = galleryPublic === true;
  publicCheck.indeterminate = galleryPublic === null;
  publicCheck.disabled = visibilityPending || galleryPublic === null;
  makePublicBtn.disabled = publicCheck.disabled;
  galleryState.textContent = galleryPublic === null ? "Visibility unknown"
    : (galleryPublic ? "Public" : "Private") + (galleryLocked ? " · Protected" : "");
  galleryState.title = galleryVisibilityHint();
  updateProtectUI();
}

function showLanding() {
  galleryLabel.textContent = "";
  galleryState.hidden = true;
  homeLink.hidden = true;
  editBtn.hidden = true;
  setEditing(false);
  photos = [];
  nextCursor = null;
  galleryLocked = false;
  renderGrid();
  empty.hidden = true;
  empty.textContent = "";
}

function showUnavailable(message) {
  setEditing(false);
  galleryState.hidden = true;
  editBtn.hidden = true;
  homeLink.hidden = false;
  photos = [];
  nextCursor = null;
  renderGrid();
  empty.textContent = message;
}

const PAGE_SIZE = 200;

/** Password as a percent-encoded header, never a URL: fetch() refuses
    non-ASCII header values, so encodeURIComponent + server-side decode. */
function listingHeaders(slug, pass = storedPass(slug)) {
  const headers = {};
  if (galleryKey) headers["X-Gallery-Key"] = galleryKey;
  if (pass) headers["X-Gallery-Pass"] = encodeURIComponent(pass);
  return headers;
}

function listingUrl(slug, before) {
  let url = "/api/galleries/" + slug + "/photos?limit=" + PAGE_SIZE;
  if (before) url += "&before=" + encodeURIComponent(before);
  return url;
}

async function loadPhotos(retry = false) {
  const slug = slugFromPath();
  if (!slug) {
    showLanding();
    return;
  }
  const request = ++listingVersion;
  const version = visibilityVersion;
  const current = () => request === listingVersion && slug === slugFromPath();
  const currentVisibility = () => version === visibilityVersion && !visibilityPending;
  galleryLabel.textContent = slug;
  editBtn.hidden = false;
  homeLink.hidden = false;
  publicCheck.indeterminate = galleryPublic === null;
  publicCheck.disabled = visibilityPending || galleryPublic === null;
  try {
    // The listing's Set-Cookie authorises every media tag.
    const headers = listingHeaders(slug);
    const res = await fetch(listingUrl(slug), { headers });
    if (!current()) return;
    if (res.status === 404) {
      if (!currentVisibility()) return;
      // Private (or deleted): join an upload or edit already asking for its key.
      if (!retry) {
        const key = await askKeyForGallery(slug,
          "This gallery is private or does not exist.",
          fetch, { title: "Gallery unavailable", rejectedKey: headers["X-Gallery-Key"] });
        if (!current() || !currentVisibility()) return;
        if (key) return loadPhotos(true);
      }
      showUnavailable("This gallery is private or does not exist.");
      return;
    }
    let data;
    if (res.status === 401) {
      if (!currentVisibility()) return;
      const ownerRequest = ownerKeyRequests.get(slug);
      if (!retry && ownerRequest) {
        const key = await ownerRequest;
        if (!current() || !currentVisibility()) return;
        if (key) return loadPhotos(true);
      }
      // Locked: prompt for the viewer password (owner key also accepted).
      // Only a public gallery reaches here, so its state is known.
      galleryPublic = true;
      galleryLocked = true;
      updateGalleryState();
      showKeyBannerOnce(slug);
      data = await needPass(slug);
      if (!current() || !currentVisibility()) return;
      if (!data) {
        photos = [];
        nextCursor = null;
        renderGrid();
        empty.hidden = false;
        empty.textContent = "This gallery is locked.";
        return;
      }
    } else {
      if (!res.ok) throw new Error("bad status " + res.status);
      data = await res.json();
    }
    if (!current()) return;
    // A listing started before or during a write cannot establish visibility.
    if (currentVisibility()) {
      galleryPublic = typeof data.public === "boolean" ? data.public : null;
      galleryLocked = data.locked === true;
      updateGalleryState();
    }
    showKeyBannerOnce(slug);
    photos = data.photos || [];
    nextCursor = data.next || null;
    // Drop selection for photos that no longer exist.
    for (const name of Array.from(selected)) {
      if (!photos.some((p) => p.name === name)) selected.delete(name);
    }
    renderGrid();
    updateEditUI();
    // A home batch that partially failed leaves this one-shot note for the
    // gallery page it navigates to.
    let uploadFailures = "";
    try {
      uploadFailures = sessionStorage.getItem("upload-failed:" + slug) || "";
      if (uploadFailures) sessionStorage.removeItem("upload-failed:" + slug);
    } catch (_) { /* private mode */ }
    if (uploadFailures) {
      uploadStatus.textContent = "Some files were not uploaded: " + uploadFailures;
    }
  } catch (_) {
    if (!current() || !currentVisibility()) return;
    nextCursor = null;
    updateLoadMore();
    galleryState.hidden = true;
    empty.hidden = false;
    empty.textContent = "Could not load photos. Reload to try again.";
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
  const slug = slugFromPath();
  if (!slug) return;
  const key = await askKeyForGallery(slug, "Enter the owner key to edit this gallery.");
  if (!key) return;
  setEditing(true);
});

doneBtn.addEventListener("click", () => setEditing(false));

/** Write once; after an ambiguous result, report only what an owner read confirms. */
async function setGalleryPublic(want) {
  const slug = slugFromPath();
  const key = galleryKey;
  if (!slug || !key || visibilityPending || galleryPublic === null) return;
  visibilityPending = true;
  visibilityVersion += 1;
  updateGalleryState();
  try {
    let res;
    try {
      res = await fetch("/api/galleries/" + slug + "/visibility", {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
        body: JSON.stringify({ public: want }),
      });
    } catch (_) { /* The server may have committed before the connection failed. */ }
    const data = await res?.json().catch(() => null);
    if (res?.ok && data?.slug === slug && data.public === want) {
      galleryPublic = want;
      return;
    }
    if (res && res.status >= 400 && res.status < 500 && res.status !== 408) {
      throw new Error(data?.error || "The server refused the visibility change. Try again.");
    }

    galleryPublic = null;
    updateGalleryState();
    let listing;
    try {
      const read = await fetch("/api/galleries/" + slug + "/photos?limit=1", {
        headers: { "X-Gallery-Key": key },
        cache: "no-store",
      });
      if (read.ok) listing = await read.json();
    } catch (_) { /* Remain unknown when the owner read also fails. */ }
    if (listing?.slug !== slug || typeof listing.public !== "boolean" || typeof listing.locked !== "boolean") {
      throw new Error(galleryVisibilityHint());
    }
    galleryPublic = listing.public;
    galleryLocked = listing.locked;
    if (galleryPublic === want) return;
    throw new Error("Visibility change was not confirmed. The gallery is " +
      (galleryPublic ? "public" : "private") + ". Try again.");
  } finally {
    visibilityPending = false;
    visibilityVersion += 1;
    updateGalleryState();
  }
}

publicCheck.addEventListener("change", async () => {
  const want = publicCheck.checked;
  editStatus.textContent = "";
  try {
    await setGalleryPublic(want);
  } catch (e) {
    editStatus.textContent = e.message;
  }
});

function updateProtectUI() {
  const help = document.getElementById("setpass-help");
  document.getElementById("setpass-remove").hidden = !galleryLocked;
  document.getElementById("setpass-private").hidden = galleryPublic !== false;
  if (galleryPublic === null) {
    help.textContent = galleryVisibilityHint();
  } else if (galleryPublic) {
    help.textContent = galleryLocked
      ? "This gallery is protected. Enter a new password to change it, or remove protection below."
      : "This gallery is public. Enter a password below to protect it.";
  } else {
    help.textContent =
      (galleryLocked
        ? "This gallery is protected, but it is private, so only you can open it. "
        : "This gallery is private, so only you can open it. ") +
      "Make it public to share it; the password then gates viewers.";
  }
}

lockBtn.addEventListener("click", async () => {
  const slug = slugFromPath();
  if (!slug || !(galleryKey || await askKeyForGallery(slug, "Enter the owner key for this gallery."))) return;
  const input = document.getElementById("setpass-input");
  document.getElementById("setpass-error").textContent = "";
  input.value = "";
  updateProtectUI();
  document.getElementById("setpass-dialog").showModal();
  input.focus();
});

makePublicBtn.addEventListener("click", async () => {
  const errEl = document.getElementById("setpass-error");
  errEl.textContent = "";
  editStatus.textContent = "";
  try {
    if (await setGalleryPublic(true)) editStatus.textContent = "Gallery is public";
  } catch (e) {
    errEl.textContent = e.message;
  }
});

document.getElementById("setpass-form").addEventListener("submit", async (e) => {
  const clicked = e.submitter?.value;
  if (clicked === "cancel") return;
  const slug = slugFromPath();
  const expectedLocked = clicked !== "remove";
  const value = document.getElementById("setpass-input").value;
  const errEl = document.getElementById("setpass-error");
  if (expectedLocked && !value) return; // empty = keep, no request
  const dialog = document.getElementById("setpass-dialog");
  const key = galleryKey;
  let res;
  try {
    res = await fetch("/api/galleries/" + slug + "/lock", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
      body: JSON.stringify({ password: expectedLocked ? value : "" }),
    });
  } catch (_) { /* The request may have committed before the connection failed. */ }
  if (res && res.status >= 400 && res.status < 500 && res.status !== 408) {
    const data = await res.json().catch(() => null);
    errEl.textContent = data?.error || res.statusText;
    dialog.showModal();
    return;
  }
  const data = res?.ok ? await res.json().catch(() => null) : null;
  if (data?.slug === slug && data?.locked === expectedLocked) {
    galleryLocked = expectedLocked;
    updateGalleryState();
    try {
      if (expectedLocked) sessionStorage.setItem("gallery-pass:" + slug, value);
      else sessionStorage.removeItem("gallery-pass:" + slug);
    } catch (_) {}
    uploadStatus.textContent = galleryLocked ? "Gallery protected." : "Protection removed.";
    await loadPhotos();
    return;
  }
  const unconfirmed = "Password change was not confirmed. Try again.";
  uploadStatus.textContent = unconfirmed;
  await loadPhotos();
  uploadStatus.textContent = unconfirmed;
  errEl.textContent = unconfirmed;
  updateProtectUI();
  dialog.showModal();
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
  confirmText.textContent = "Delete this gallery and all its photos? This cannot be undone.";
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

// Only formats the server generates previews for. Everything else (SVG, AVIF,
// HEIC, …) must load its original/special path directly, not 404 then fall back.
const THUMB_EXTS = new Set([".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".ico"]);
function thumbUrl(name) {
  const dot = name.lastIndexOf(".");
  const ext = dot >= 0 ? name.slice(dot).toLowerCase() : "";
  return THUMB_EXTS.has(ext) ? "/photos/thumb/" + encodeURIComponent(name) + ".jpg" : "";
}

// Owner key as a header; viewer password goes on the listing fetch only
// (percent-encoded: fetch() refuses non-ASCII header values).
async function authedFetch(name, { range, signal } = {}) {
  const headers = {};
  if (galleryKey) headers["X-Gallery-Key"] = galleryKey;
  if (range) headers.Range = range;
  return fetch(mediaUrl(name), { headers, signal });
}


// --- HEIC: one fetch, render natively, decode only when the browser can't ---
const HEIC_PREVIEW_BYTES = 8 * 1024 * 1024;
const HEIC_CACHE_BYTES = 4 * HEIC_PREVIEW_BYTES;
const heicCache = new Map();
let heicCacheBytes = 0;
let heicWork = Promise.resolve();

async function readHeicPreview(name) {
  const res = await authedFetch(name, { signal: AbortSignal.timeout(30_000) });
  if (!res.ok || !res.body || Number(res.headers.get("Content-Length")) > HEIC_PREVIEW_BYTES) {
    res.body?.cancel().catch(() => {});
    throw new Error("HEIC preview unavailable");
  }
  const reader = res.body.getReader();
  const chunks = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return new Blob(chunks, { type: "image/heic" });
    size += value.byteLength;
    if (size > HEIC_PREVIEW_BYTES) {
      reader.cancel().catch(() => {});
      throw new Error("HEIC preview too large");
    }
    chunks.push(value);
  }
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

async function renderHeicPreview(name, img) {
  const cached = heicCache.get(name);
  if (cached) {
    heicCache.delete(name);
    heicCache.set(name, cached);
    img.src = cached.url;
    await img.decode();
    return;
  }
  let blob = await readHeicPreview(name);
  if (!img.isConnected) return;
  let url = URL.createObjectURL(blob);
  try {
    img.src = url;
    try {
      await img.decode();
    } catch (error) {
      if (!img.isConnected) throw error;
      await loadHeicDecoder();
      if (!img.isConnected) return;
      const out = await heic2any({ blob, toType: "image/jpeg", quality: 0.8 });
      blob = Array.isArray(out) ? out[0] : out;
      if (!img.isConnected) return;
      if (blob.size > HEIC_PREVIEW_BYTES) throw new Error("HEIC preview too large");
      URL.revokeObjectURL(url);
      url = URL.createObjectURL(blob);
      img.src = url;
      await img.decode();
    }
    heicCache.set(name, { url, size: blob.size });
    heicCacheBytes += blob.size;
    url = null;
    while (heicCacheBytes > HEIC_CACHE_BYTES || heicCache.size > 12) {
      const [oldest, entry] = heicCache.entries().next().value;
      heicCache.delete(oldest);
      heicCacheBytes -= entry.size;
      URL.revokeObjectURL(entry.url);
    }
  } finally {
    if (url !== null) URL.revokeObjectURL(url);
  }
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
    // Hold one slot from the fetch through native or fallback decoding.
    heicWork = heicWork.then(async () => {
      if (img.isConnected) await renderHeicPreview(name, img);
    }).catch(() => {
      if (img.isConnected) onFail(img);
    });
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
    return heicImage(photo.name, (img) => {
      const div = document.createElement("div");
      div.className = "file-generic";
      div.textContent = displayName(photo.name);
      div.title = displayName(photo.name) + " (open to download original)";
      img.replaceWith(div);
    });
  }
  if (kind === "image") {
    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = displayName(photo.name);
    // Supported formats get the server preview (old photos have none, so fall
    // back to the original exactly once); others load the original directly.
    const preview = thumbUrl(photo.name);
    if (preview) {
      img.src = preview;
      img.onerror = () => {
        img.onerror = null;
        img.src = url;
      };
    } else {
      img.src = url;
    }
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

function addTile(photo, i) {
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
}

function renderGrid() {
  grid.textContent = "";
  empty.hidden = photos.length > 0;
  empty.textContent = "Empty gallery.";
  photos.forEach(addTile);
  updateLoadMore();
}

function updateLoadMore() {
  loadMoreBtn.hidden = !slugFromPath() || !nextCursor || loadingMore;
}

/** Append the next page without rebuilding existing tiles or selection. */
async function loadMore() {
  const slug = slugFromPath();
  const version = listingVersion;
  const listing = photos;
  const cursor = nextCursor;
  if (!slug || !cursor || loadingMore) return;
  const current = () => slug === slugFromPath() && version === listingVersion &&
    listing === photos && cursor === nextCursor;
  loadingMore = true;
  updateLoadMore();
  try {
    const res = await fetch(listingUrl(slug, cursor), { headers: listingHeaders(slug) });
    if (!res.ok) throw new Error("bad status " + res.status);
    const data = await res.json();
    if (!current()) return;
    const fresh = data.photos || [];
    const start = photos.length;
    photos.push(...fresh);
    fresh.forEach((photo, i) => addTile(photo, start + i));
    nextCursor = data.next || null;
  } catch (_) {
    if (current()) uploadStatus.textContent = "Could not load more photos";
  } finally {
    loadingMore = false;
    updateLoadMore();
  }
}

loadMoreBtn.addEventListener("click", loadMore);

function render() {
  counter.textContent = (index + 1) + " / " + photos.length;
}

/// Show photo i in the overlay by adopting its (already loaded) grid
/// thumbnail: no second fetch, no src swap, no black frame.
function openLightbox(i) {
  if (!photos.length) { closeLightbox(); return; }
  index = Math.min(Math.max(i, 0), photos.length - 1);
  showInViewport(index);
  if (!lightbox.open) {
    focusReturn = document.activeElement;
    lightbox.showModal();
  }
  closeBtn.focus();
  render();
}

// Tile index whose media is currently adopted by the lightbox (-1 = none).
let adoptedTile = -1;
let focusReturn = null;
let viewportVersion = 0;

function showInViewport(i) {
  const version = ++viewportVersion;
  stopViewportMedia();
  viewport.textContent = "";
  adoptedTile = -1;
  const name = photos[i].name;
  const kind = mediaKind(name);
  const url = mediaUrl(name);
  if (kind === "heic") {
    viewport.appendChild(heicImage(name, (img) => {
      const div = document.createElement("div");
      div.className = "file-generic";
      div.textContent = displayName(name);
      const link = document.createElement("a");
      link.href = url;
      link.download = displayName(name);
      link.textContent = "Download original";
      div.appendChild(link);
      img.replaceWith(div);
    }, true));
    return;
  }
  if (kind === "image") {
    // Grid tiles now show a small thumb: the lightbox must load the original.
    const img = document.createElement("img");
    img.alt = displayName(name);
    img.src = url;
    viewport.appendChild(img);
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
    (async () => {
      let text;
      let truncated = false;
      try {
        const cap = 65_536;
        const res = await authedFetch(name, { range: "bytes=0-65535" });
        if (res.status !== 206) {
          res.body?.cancel().catch(() => {});
        } else if (res.body) {
          const reader = res.body.getReader();
          const bytes = new Uint8Array(cap);
          let length = 0;
          while (length < cap) {
            const { value, done } = await reader.read();
            if (done) break;
            const chunk = value.subarray(0, cap - length);
            bytes.set(chunk, length);
            length += chunk.length;
          }
          if (length === cap) reader.cancel().catch(() => {});
          const total = res.headers.get("Content-Range")?.match(/\/(\d+)$/);
          truncated = total ? Number(total[1]) > cap : length === cap;
          text = new TextDecoder().decode(bytes.subarray(0, length));
        }
      } catch (_) { /* An interrupted preview still offers the original. */ }
      if (!lightbox.open || version !== viewportVersion) return;
      const panel = document.createElement("div");
      panel.className = "text-preview";
      if (text !== undefined) {
        if (truncated) {
          const label = document.createElement("span");
          label.textContent = "First 64 KiB";
          panel.appendChild(label);
        }
        const pre = document.createElement("pre");
        pre.textContent = text;
        panel.appendChild(pre);
      } else {
        panel.textContent = "Preview unavailable.";
      }
      const link = document.createElement("a");
      link.href = url;
      link.download = displayName(name);
      link.textContent = "Download original";
      panel.appendChild(link);
      viewport.textContent = "";
      viewport.appendChild(panel);
    })();
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
  const media = viewport.querySelector("video, audio");
  if (media) { try { media.pause(); } catch (_) {} }
}

function restoreThumbnails() {
  viewportVersion += 1;
  // Put the adopted media back where it came from (by index, not by URL).
  stopViewportMedia();
  if (adoptedTile >= 0) {
    const media = viewport.querySelector("img, video, .file-generic");
    const tile = grid.children[adoptedTile];
    if (media && tile && !tile.querySelector("img, video, .file-generic")) {
      const link = media.querySelector("a");
      if (link) link.remove();
      if (media.tagName === "VIDEO") { media.controls = false; media.muted = true; }
      tile.prepend(media);
    }
  }
  adoptedTile = -1;
  viewport.textContent = "";
}

function closeLightbox() {
  if (lightbox.open) lightbox.close();
}

lightbox.addEventListener("close", () => {
  restoreThumbnails();
  if (focusReturn) {
    try { focusReturn.focus(); } catch (_) {}
    focusReturn = null;
  }
});

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
  if (!lightbox.open) return;
  if (e.key === "ArrowLeft") step(-1);
  else if (e.key === "ArrowRight") step(1);
  else if (e.key === "Escape") { e.preventDefault(); closeLightbox(); }
});

// The key is hand-typed, never generated: the user must remember (or note)
// what they wrote. Keys are 6-128 visible ASCII chars (header-safe);
// new keys also need a capital letter and a symbol.

/** Why a *new* key is not acceptable yet ("" = fine). */
function newKeyProblem(v) {
  if (!/\p{Lu}/u.test(v)) return "Key needs a capital letter";
  if (!/[^\p{L}\p{N}]/u.test(v)) return "Key needs a symbol, like ! - _ #";
  return "";
}

/** Resolves {ok:true} or {ok:false,status,error}. status 0 = network. */
async function verifyGalleryKey(slug, key, request, signal) {
  let res;
  try {
    res = await request("/api/galleries/resolve", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
      body: JSON.stringify({ slug }),
      signal,
    });
  } catch (_) {
    return { ok: false, status: 0, error: "Could not check the key. Check your connection and try again." };
  }
  if (res.ok) return { ok: true };
  if (res.status === 403) return { ok: false, status: 403, error: "That key does not open this gallery." };
  if (res.status === 429) return { ok: false, status: 429, error: "Too many attempts. Wait a minute and try again." };
  let msg = res.statusText;
  try { const e = await res.json(); if (e.error) msg = e.error; } catch (_) {}
  return { ok: false, status: res.status, error: "Could not check the key: " + msg };
}

/** Share the owner's key check and prompt for callers targeting the same gallery. */
function askKeyForGallery(slug, help, request = fetch, { title, rejectedKey } = {}) {
  if (ownerKeyRequests.has(slug)) return ownerKeyRequests.get(slug);
  const pending = Promise.resolve().then(async () => {
    if (slug !== slugFromPath()) return "";
    const saved = galleryKey;
    if (saved && saved !== rejectedKey) {
      const check = await verifyGalleryKey(slug, saved, request);
      if (slug !== slugFromPath()) return "";
      if (check.ok) return saved;
      if (check.status === 429) {
        uploadStatus.textContent = check.error;
        return "";
      }
      // Keep the cached key after a failed network check; the action can report the error.
      if (check.status !== 403) return saved;
    }
    return needKey("enter", {
      title,
      help: saved ? "The saved key does not open this gallery. " + help : help,
      validate: async (key, signal) => (await verifyGalleryKey(slug, key, request, signal)).error || "",
    });
  }).finally(() => ownerKeyRequests.delete(slug));
  ownerKeyRequests.set(slug, pending);
  return pending;
}

function finishKeyPrompt(prompt, value) {
  if (!prompt || keyPrompt !== prompt) return;
  keyPrompt = null;
  if (!value) prompt.controller?.abort();
  keyInput.value = "";
  if (keyDialog.open) keyDialog.close();
  prompt.resolve(value);
}

function needKey(mode, { title, help, validate, error = "" } = {}) {
  // Create, invite, and other galleries cannot consume the old prompt's answer.
  finishKeyPrompt(keyPrompt, "");
  const prompt = { mode, validate, controller: validate ? new AbortController() : null, resolve: null };
  const pending = new Promise((resolve) => { prompt.resolve = resolve; });
  keyPrompt = prompt;
  document.getElementById("key-title").textContent =
    title || (mode === "create" ? "New gallery key" : "Gallery key");
  keyHelp.textContent = help || "At least 6 characters, with a capital letter and a symbol.";
  keyInput.value = "";
  keyError.textContent = error;
  keyConfirm.disabled = false;
  keyDialog.showModal();
  keyInput.focus();
  return pending;
}

const passDialog = document.getElementById("pass-dialog");
const passInput = document.getElementById("pass-input");
const passError = document.getElementById("pass-error");
const passConfirm = document.getElementById("pass-confirm");
let passPrompt = null;

/** Resolve to the listing that granted the viewer cookie, or null on cancel. */
function needPass(slug) {
  passError.textContent = "";
  passInput.value = "";
  passConfirm.disabled = false;
  passDialog.showModal();
  passInput.focus();
  return new Promise((resolve) => { passPrompt = { resolve, slug, controller: new AbortController() }; });
}

document.getElementById("pass-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const prompt = passPrompt;
  if (!prompt || passConfirm.disabled) return;
  const v = passInput.value;
  if (!v) {
    passError.textContent = "Enter the gallery password.";
    passInput.focus();
    return;
  }
  passError.textContent = "";
  passConfirm.disabled = true;
  let data;
  let error = "Could not check the password. Try again.";
  try {
    const res = await fetch(listingUrl(prompt.slug), {
      headers: listingHeaders(prompt.slug, v),
      signal: prompt.controller.signal,
    });
    if (res.status === 401) error = "Incorrect password. Check it and try again.";
    else if (res.status === 429) error = "Too many attempts. Wait and try again.";
    else if (res.ok) {
      const listing = await res.json().catch(() => null);
      // Only the listing this gallery granted may close the prompt and set the cookie.
      if (listing?.slug === prompt.slug && Array.isArray(listing.photos)) data = listing;
    }
  } catch (_) {
    error = "Could not check the password. Check your connection and try again.";
  }
  if (passPrompt !== prompt || !passDialog.open) return;
  passConfirm.disabled = false;
  if (!data) {
    passError.textContent = error;
    passInput.focus();
    return;
  }
  try { sessionStorage.setItem("gallery-pass:" + prompt.slug, v); } catch (_) {}
  passPrompt = null;
  passInput.value = "";
  passDialog.close();
  prompt.resolve(data);
});
document.getElementById("pass-cancel").addEventListener("click", () => passDialog.close());
document.getElementById("setpass-cancel").addEventListener("click", () => {
  document.getElementById("setpass-dialog").close();
});
passDialog.addEventListener("close", () => {
  if (passDialog.open || !passPrompt) return;
  const { resolve, controller } = passPrompt;
  passPrompt = null;
  controller.abort();
  passInput.value = "";
  resolve(null);
});

keyForm.addEventListener("submit", async (e) => {
  // The dialog closes only on success, so a failed owner-key check can show
  // its error in place.
  e.preventDefault();
  const prompt = keyPrompt;
  if (!prompt || !keyDialog.open || keyConfirm.disabled) return;
  keyError.textContent = "";
  const v = keyInput.value.trim();
  if (prompt.mode === "invite") {
    if (!/^[\x20-\x7e]{1,128}$/.test(v)) {
      keyError.textContent = !v ? "Invite code required" : "Use 1–128 printable ASCII characters.";
      keyInput.focus();
      return;
    }
    finishKeyPrompt(prompt, v);
    return;
  }
  const problem = !/^[\x21-\x7e]{6,128}$/.test(v)
    ? "Key must be 6-128 ASCII characters (no spaces/accents)"
    : (prompt.mode === "create" ? newKeyProblem(v) : "");
  if (problem) {
    keyError.textContent = problem;
    keyInput.focus();
    return;
  }
  if (prompt.validate) {
    keyConfirm.disabled = true;
    let check = "";
    try {
      check = await prompt.validate(v, prompt.controller.signal);
    } catch (_) {
      check = "Could not check the key. Try again.";
    }
    if (keyPrompt !== prompt || !keyDialog.open) return;
    keyConfirm.disabled = false;
    if (check) {
      keyError.textContent = check;
      keyInput.focus();
      return;
    }
  }
  galleryKey = v;
  try { sessionStorage.setItem("gallery-key", v); } catch (_) { /* private mode */ }
  finishKeyPrompt(prompt, v);
});
document.getElementById("key-cancel").addEventListener("click", () => finishKeyPrompt(keyPrompt, ""));
keyDialog.addEventListener("cancel", (e) => {
  e.preventDefault();
  finishKeyPrompt(keyPrompt, "");
});
keyDialog.addEventListener("close", () => {
  // The browser queues close events, so this may be from a replaced prompt.
  if (!keyDialog.open) finishKeyPrompt(keyPrompt, "");
});

uploadBtn.addEventListener("click", () => fileInput.click());

/** Mint one gallery for a whole home upload batch. */
async function newGallery(key) {
  let inviteCode = "";
  for (;;) {
    const headers = { "Content-Type": "application/json", "X-Gallery-Key": key };
    if (inviteCode) headers["X-Invite-Code"] = inviteCode;
    let res;
    try {
      res = await resolveForUpload("/api/galleries/resolve", { method: "POST", headers, body: "{}" });
    } catch (_) {
      throw new Error("Could not create the gallery. Check your connection and try again.");
    }
    const data = await res.json().catch(() => null);
    if (res.ok && data?.slug) return data.slug;
    if (res.status === 429) {
      throw new Error("Upload rate limit reached. Wait and try again.");
    }
    if (res.status !== 403 || data?.error !== "invite required") {
      throw new Error("Could not create the gallery. Try again.");
    }
    inviteCode = await needKey("invite", {
      title: "Invite code",
      help: "Enter the invite code shared by the site operator.",
      error: inviteCode ? "Incorrect invite code. Check it and try again." : "",
    });
    if (!inviteCode) return "";
  }
}

// A stable upload id lets a lost or failed request resume committed bytes.
async function putFile(file, headersBase) {
  const headersFor = { ...headersBase };
  if (file.type) headersFor["Content-Type"] = file.type;
  const id = randomUploadId();
  let offset = 0;
  let furthest = 0;
  let failures = 0;
  for (;;) {
    const end = Math.min(offset + UPLOAD_CHUNK, file.size);
    const headers = {
      ...headersFor,
      "X-Upload-Id": id,
      "Content-Range": `bytes ${offset}-${end - 1}/${file.size}`,
    };
    let res = null;
    try {
      res = await uploadRequest("/api/photos/" + encodeURIComponent(file.name), {
        method: "PUT",
        headers,
        body: file.slice(offset, end),
      });
    } catch (_) { /* network failure: retried below */ }
    if (res?.status === 201) return res;
    if (res?.status === 202 || res?.status === 409) {
      const data = await (res.status === 409 ? res.clone() : res).json().catch(() => null);
      // A saved file or row for this id cannot be resumed with different bytes.
      if (res.status === 409 && data?.error === "upload id already used") return res;
      const received = data?.received;
      // A busy writer's temp length may include bytes it still has to roll back.
      const authoritative = res.status === 202 ? received === end : data?.error === "offset mismatch";
      if (authoritative && Number.isInteger(received) && received >= 0 && received < file.size) {
        offset = received;
        // A rewind must not replenish the budget for the same bytes.
        if (offset > furthest) {
          furthest = offset;
          failures = 0;
          continue;
        }
        if (res.status === 202) continue;
      }
    } else if (res?.status === 410) {
      // The temp expired; restart with the same upload id.
      offset = 0;
    } else if (res && !retryableStatus(res.status)) {
      return res;
    }
    if (++failures > UPLOAD_RETRIES) return res;
    if (res?.status !== 429) await sleep(500 * 2 ** Math.min(failures - 1, 4));
  }
}

fileInput.addEventListener("change", async () => {
  const files = Array.from(fileInput.files);
  fileInput.value = "";
  if (!files.length) return;
  if (uploading) {
    uploadStatus.textContent = "Upload already in progress.";
    return;
  }
  const failures = [];
  // Reject before minting: a batch that stores nothing must not leave an
  // empty gallery behind.
  const accepted = files.filter((file) => {
    if (file.size === 0) {
      failures.push(file.name + ": empty file");
      return false;
    }
    if (file.size > MAX_BYTES) {
      failures.push(file.name + ": file is too large (10GB max)");
      return false;
    }
    return true;
  });
  const pageSlug = slugFromPath();
  if (!accepted.length) {
    uploadStatus.textContent = failures.join("; ");
    return;
  }
  uploading = true;
  uploadBtn.disabled = true;
  try {
    await uploadBatch(pageSlug, accepted, failures);
  } finally {
    uploading = false;
    uploadBtn.disabled = false;
  }
});

async function uploadBatch(pageSlug, accepted, failures) {
  const key = pageSlug
    ? await askKeyForGallery(pageSlug, "Enter the owner key for this gallery to upload.", resolveForUpload)
    : await needKey("create");
  if (!key) return;
  // One gallery per batch, picked before the first byte.
  let target;
  try {
    target = pageSlug || (await newGallery(key));
  } catch (e) {
    uploadStatus.textContent = e.message;
    return;
  }
  if (!target) return;
  let stored = 0;
  let started = 0;
  const total = accepted.length;
  const headersBase = { "X-Gallery-Key": key, "X-Gallery-Slug": target };
  async function uploadWorker() {
    for (;;) {
      const file = accepted.shift();
      if (!file) return;
      started += 1;
      uploadStatus.textContent = "Uploading " + started + " of " + total + " — " + file.name;
      const res = await putFile(file, headersBase);
      if (res?.status !== 201) {
        let failure = "Upload was not confirmed. Try again.";
        if (res?.status === 429) {
          failure = "Upload rate limit reached. Wait and try again.";
        } else if (res?.status === 409) {
          // Other 409s stay generic; only a definite id conflict is actionable.
          const err = await res.clone().json().catch(() => null);
          if (err?.error === "upload id already used") {
            failure = "This upload ID is already used. Select the file again to start a new upload.";
          }
        } else if (res && !res.ok) {
          failure = res.statusText || failure;
          try {
            const err = await res.json();
            if (err.error) failure = err.error;
          } catch (_) { /* keep status text */ }
        }
        failures.push(file.name + ": " + failure);
        continue;
      }
      stored += 1;
    }
  }
  // The server allows 4 concurrent uploads per IP; 3 leaves room for the
  // listing request that follows the batch.
  uploadStatus.textContent = "Uploading 0 of " + total + "…";
  await Promise.all(Array.from({ length: Math.min(3, accepted.length) }, uploadWorker));
  if (pageSlug) {
    // Gallery page: show the new tiles without a manual reload.
    await loadPhotos(true);
    uploadStatus.textContent = failures.length
      ? (stored ? "Uploaded " + stored + " of " + total + ". " : "") + failures.join("; ")
      : "Uploaded " + stored + (stored === 1 ? " photo." : " photos.");
    return;
  }
  if (!stored) {
    // Nothing landed: stay put so the errors stay visible.
    uploadStatus.textContent = failures.join("; ") || "Upload failed.";
    return;
  }
  // Carry the key and any failure report to the next page. The key banner
  // shows the key once; the gallery page shows the failures once.
  try { sessionStorage.setItem("show-key:" + target, key); } catch (_) {}
  if (failures.length) {
    try { sessionStorage.setItem("upload-failed:" + target, failures.join("; ")); } catch (_) {}
  }
  location.assign("/" + target);
}

themeBtn.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem("theme", next); } catch (_) { /* private mode */ }
});

loadPhotos();
