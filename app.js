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
const copyLinkBtn = document.getElementById("copy-link-btn");
const keyDialog = document.getElementById("key-dialog");
const keyForm = document.getElementById("key-form");
const keyInput = document.getElementById("key-input");
const keyError = document.getElementById("key-error");
const keyHelp = document.getElementById("key-help");
const keyConfirm = document.getElementById("key-confirm");
const publicCheck = document.getElementById("public-check");
const editStatus = document.getElementById("edit-status");
const keyBanner = document.getElementById("key-banner");
const keyBannerKey = document.getElementById("key-banner-key");
const keyBannerHint = document.getElementById("key-banner-hint");
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
// Whole-gallery visibility; new galleries start private server-side.
let galleryPublic = true;
// Viewer password set? Comes from the listing: an unauthenticated probe gets
// 404 on a private gallery, so it cannot tell whether a lock exists.
let galleryLocked = false;
// Gallery key: kept in memory + sessionStorage (per-tab). Never in the URL.
// Storage can throw when site data is blocked; the page must still load.
let galleryKey = "";
try { galleryKey = sessionStorage.getItem("gallery-key") || ""; } catch (_) {}
let keyResolver = null;
let keyValidator = null;
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

function keyBannerHintText() {
  if (galleryPublic) {
    return galleryLocked
      ? "Anyone with the link needs the viewer password."
      : "Anyone with the link can view this gallery.";
  }
  return "This gallery is private. Tick Public in Edit to share it.";
}

function showKeyBanner(key) {
  keyBannerKey.textContent = key;
  keyBannerHint.textContent = keyBannerHintText();
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

copyLinkBtn.addEventListener("click", async () => {
  const url = (location.href || location.pathname || "").split("#")[0];
  const ok = await copyText(url);
  copyLinkBtn.textContent = ok ? "Copied" : "Copy failed";
  setTimeout(() => { copyLinkBtn.textContent = "Copy link"; }, 1500);
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
  copyLinkBtn.hidden = !slug;
  if (!slug) return;
  galleryState.textContent = (galleryPublic ? "Public" : "Private") + (galleryLocked ? " · Protected" : "");
  galleryState.title = keyBannerHintText();
  if (!keyBanner.hidden) keyBannerHint.textContent = keyBannerHintText();
}

function showLanding() {
  galleryLabel.textContent = "";
  galleryState.hidden = true;
  homeLink.hidden = true;
  copyLinkBtn.hidden = true;
  editBtn.hidden = true;
  setEditing(false);
  photos = [];
  nextCursor = null;
  publicCheck.checked = true;
  galleryLocked = false;
  renderGrid();
  empty.textContent = "Upload photos to start a new private gallery. To open an existing gallery, use its link; a private gallery also needs its key.";
}

function showUnavailable(message) {
  setEditing(false);
  galleryState.hidden = true;
  copyLinkBtn.hidden = true;
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
  galleryLabel.textContent = slug;
  editBtn.hidden = false;
  homeLink.hidden = false;
  copyLinkBtn.hidden = false;
  galleryState.hidden = true;
  try {
    // The listing's Set-Cookie authorises every media tag.
    const res = await fetch(listingUrl(slug), { headers: listingHeaders(slug) });
    if (res.status === 404) {
      // Private (or deleted): ask for the owner key and verify it before it
      // is stored. A wrong key stays in the dialog with an error.
      if (!retry) {
        const key = await needKey(true, "enter", {
          title: "Gallery unavailable",
          help: "This gallery is private or does not exist. Enter the owner key if the owner gave it to you; otherwise ask the owner for a link to a public gallery.",
          validate: async (v) => (await verifyGalleryKey(slug, v)).error || "",
        });
        if (key) return loadPhotos(true);
      }
      showUnavailable("This gallery is private or does not exist. Ask the owner for a public link, or for the owner key.");
      return;
    }
    let data;
    if (res.status === 401) {
      // Locked: prompt for the viewer password (owner key also accepted).
      // Only a public gallery reaches here, so its state is known.
      galleryPublic = true;
      galleryLocked = true;
      updateGalleryState();
      showKeyBannerOnce(slug);
      data = await needPass(slug);
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
    galleryPublic = data.public !== false;
    galleryLocked = data.locked === true;
    publicCheck.checked = galleryPublic;
    updateGalleryState();
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

/** Publish/unpublish the gallery; throws the server message on failure. */
async function setGalleryPublic(want) {
  const slug = slugFromPath();
  if (!slug) return;
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
  publicCheck.checked = want;
  updateGalleryState();
}

// Publish/unpublish the whole gallery. Reverts the tick if the server refuses.
publicCheck.addEventListener("change", async () => {
  const want = publicCheck.checked;
  publicCheck.disabled = true;
  editStatus.textContent = "";
  try {
    await setGalleryPublic(want);
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
  const privateRow = document.getElementById("setpass-private");
  errEl.textContent = "";
  input.value = "";
  removeBtn.hidden = !galleryLocked;
  privateRow.hidden = galleryPublic;
  if (galleryPublic) {
    help.textContent = galleryLocked
      ? "This gallery is protected. Enter a new password to change it, or remove protection below."
      : "Viewers will need this password to open the gallery.";
  } else {
    help.textContent =
      (galleryLocked
        ? "This gallery is protected, but it is private, so only you can open it. "
        : "This gallery is private, so only you can open it. ") +
      "Make it public to share it; the password then gates viewers.";
  }
  document.getElementById("setpass-dialog").showModal();
  input.focus();
});

document.getElementById("setpass-public").addEventListener("click", async () => {
  const errEl = document.getElementById("setpass-error");
  const help = document.getElementById("setpass-help");
  errEl.textContent = "";
  try {
    await setGalleryPublic(true);
    editStatus.textContent = "Gallery is public";
    document.getElementById("setpass-private").hidden = true;
    help.textContent = galleryLocked
      ? "This gallery is public. Viewers will need the password to open it."
      : "This gallery is public. Enter a password below to protect it.";
  } catch (e) {
    errEl.textContent = e.message;
  }
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
    galleryLocked = data && data.locked === true;
    updateGalleryState();
    try {
      if (clicked === "remove") sessionStorage.removeItem("gallery-pass:" + slug);
      else sessionStorage.setItem("gallery-pass:" + slug, input.value);
    } catch (_) {}
    uploadStatus.textContent = galleryLocked ? "Gallery protected." : "Protection removed.";
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
  if (!slug || !nextCursor || loadingMore) return;
  loadingMore = true;
  updateLoadMore();
  try {
    const res = await fetch(listingUrl(slug, nextCursor), { headers: listingHeaders(slug) });
    if (!res.ok) throw new Error("bad status " + res.status);
    const data = await res.json();
    const fresh = data.photos || [];
    const start = photos.length;
    photos.push(...fresh);
    fresh.forEach((photo, i) => addTile(photo, start + i));
    nextCursor = data.next || null;
  } catch (_) {
    uploadStatus.textContent = "Could not load more photos";
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
  const media = viewport.querySelector("video, audio");
  if (media) { try { media.pause(); } catch (_) {} }
}

function restoreThumbnails() {
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
// new keys also need 12+ chars, a capital and a symbol.

/** Why a *new* key is not acceptable yet ("" = fine). */
function newKeyProblem(v) {
  if (!/^[\x21-\x7e]{12,128}$/.test(v)) return "Key must be 12-128 ASCII characters (no spaces/accents)";
  if (!/\p{Lu}/u.test(v)) return "Key needs a capital letter";
  if (!/[^\p{L}\p{N}]/u.test(v)) return "Key needs a symbol, like ! - _ #";
  return "";
}

/** Resolves {ok:true} or {ok:false,status,error}. status 0 = network. */
async function verifyGalleryKey(slug, key, request = fetch) {
  let res;
  try {
    res = await request("/api/galleries/resolve", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Gallery-Key": key },
      body: JSON.stringify({ slug }),
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

/** Return a key the server accepts for `slug`; prompt and verify when needed. */
async function askKeyForGallery(slug, help, request = fetch) {
  let promptHelp = help;
  if (galleryKey) {
    const check = await verifyGalleryKey(slug, galleryKey, request);
    if (check.ok) return galleryKey;
    if (check.status === 429) {
      uploadStatus.textContent = check.error;
      return "";
    }
    // A failed network check keeps the cached key: the action itself will
    // report the real error rather than throwing the key away.
    if (check.status !== 403) return galleryKey;
    // The key may own another gallery; keep it until this prompt accepts one.
    promptHelp = "The saved key does not open this gallery. " + help;
  }
  return needKey(true, "enter", {
    help: promptHelp,
    validate: async (key) => (await verifyGalleryKey(slug, key, request)).error || "",
  });
}

/** Resolve the gallery key: memory/session, else prompt once.
    Landing page (no slug) always prompts: returning here means a new gallery. */
let keyMode = "enter";
function needKey(forcePrompt, mode = "enter", opts = {}) {
  if (!forcePrompt && galleryKey && galleryKey.length >= 6) {
    return Promise.resolve(galleryKey);
  }
  resetKeyDialog();
  keyMode = mode;
  keyValidator = opts.validate || null;
  const create = mode === "create";
  document.getElementById("key-title").textContent =
    opts.title || (create ? "New gallery key" : "Gallery key");
  keyHelp.textContent = opts.help || (create
    ? "At least 12 characters, with a capital letter and a symbol (like Sunset-Beach-42). Save it; it is the only way to edit this gallery. Using the same key on the home page starts a new gallery, never an existing one."
    : "Enter the owner key for this gallery.");
  keyInput.value = "";
  keyError.textContent = "";
  keyDialog.showModal();
  keyInput.focus();
  return new Promise((resolve) => { keyResolver = resolve; });
}

function resetKeyDialog() {
  invitePrompt = false;
  keyValidator = null;
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

keyForm.addEventListener("submit", async (e) => {
  // The dialog closes only on success, so a failed owner-key check can show
  // its error in place.
  e.preventDefault();
  keyError.textContent = "";
  const v = keyInput.value.trim();
  if (invitePrompt) {
    if (!/^[\x21-\x7e]{1,128}$/.test(v)) {
      keyError.textContent = !v ? "Invite code required" : "Invite code must be ASCII (no spaces/accents)";
      keyInput.focus();
      return;
    }
    inviteCode = v;
    resetKeyDialog();
    const resolve = keyResolver;
    keyResolver = null;
    keyDialog.close();
    if (resolve) resolve(v);
    return;
  }
  // Strong rules only for keys we create; an existing key just has to be one.
  const problem = keyMode === "create" ? newKeyProblem(v) : (!/^[\x21-\x7e]{6,128}$/.test(v) ? "Key must be 6-128 ASCII characters (no spaces/accents)" : "");
  if (problem) {
    keyError.textContent = problem;
    keyInput.focus();
    return;
  }
  if (keyValidator) {
    keyConfirm.disabled = true;
    let check = "";
    try {
      check = await keyValidator(v);
    } catch (_) {
      check = "Could not check the key. Try again.";
    } finally {
      keyConfirm.disabled = false;
    }
    if (!keyDialog.open) return; // cancelled while the check ran
    if (check) {
      keyError.textContent = check;
      keyInput.focus();
      return;
    }
  }
  galleryKey = v;
  try { sessionStorage.setItem("gallery-key", v); } catch (_) { /* private mode */ }
  resetKeyDialog();
  const resolve = keyResolver;
  keyResolver = null;
  keyDialog.close();
  if (resolve) resolve(v);
});
document.getElementById("key-cancel").addEventListener("click", () => keyDialog.close());
// Dialog closed via Esc or Cancel: treat as cancel.
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
    // A rejected code must not be cached and re-sent in a tight loop.
    inviteCode = "";
    if (!(await needInvite())) return "";
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
    : await needKey(true, "create");
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
