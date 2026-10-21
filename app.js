"use strict";

const grid = document.getElementById("grid");
const empty = document.getElementById("empty");
const lightbox = document.getElementById("lightbox");
const frame = document.getElementById("frame");
const counter = document.getElementById("counter");
const prevBtn = document.getElementById("prev");
const nextBtn = document.getElementById("next");
const closeBtn = document.getElementById("close");
const fileInput = document.getElementById("file-input");
const authBlock = document.getElementById("auth-block");
const authUsername = document.getElementById("auth-username");
const authPassword = document.getElementById("auth-password");
const loginBtn = document.getElementById("login-btn");
const registerBtn = document.getElementById("register-btn");
const authError = document.getElementById("auth-error");
const userBlock = document.getElementById("user-block");
const userLabel = document.getElementById("user-label");
const logoutBtn = document.getElementById("logout-btn");
const deleteBtn = document.getElementById("delete-btn");
const adminPassword = document.getElementById("admin-password");
const adminUnlockBtn = document.getElementById("admin-unlock-btn");
const adminError = document.getElementById("admin-error");
const adminPanel = document.getElementById("admin-panel");
const adminUsers = document.getElementById("admin-users");

const MAX_BYTES = 20 * 1024 * 1024;
const ALLOWED_EXTS = [".jpg", ".jpeg", ".png", ".gif", ".webp"];
const MIME_BY_EXT = {
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".png": "image/png",
  ".gif": "image/gif",
  ".webp": "image/webp",
};

let photos = [];
let index = 0;
let me = null;
let admin = false;

const uploadBtn = document.getElementById("upload-btn");
const uploadStatus = document.getElementById("upload-status");
const themeBtn = document.getElementById("theme-btn");

function getStatusEl() {
  return uploadStatus;
}

function fileExt(name) {
  const dot = name.lastIndexOf(".");
  return dot >= 0 ? name.slice(dot).toLowerCase() : "";
}

async function loadPhotos() {
  try {
    const res = await fetch("/api/photos");
    if (!res.ok) throw new Error("bad status " + res.status);
    const data = await res.json();
    photos = data.photos || [];
    renderGrid();
  } catch (_) {
    empty.hidden = false;
    empty.textContent = "Could not load photos";
  }
}

function displayName(stored) {
  return stored
    .replace(/^[A-Za-z0-9_-]{3,20}_[0-9a-f]{32}_/, "")
    .replace(/^[0-9a-f]{8}_/, "");
}

function renderGrid() {
  grid.textContent = "";
  empty.hidden = photos.length > 0;
  empty.textContent = "No photos yet";
  photos.forEach((photo, i) => {
    const tile = document.createElement("button");
    tile.className = "tile";
    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = displayName(photo.name);
    img.src = "/photos/" + encodeURIComponent(photo.name);
    tile.appendChild(img);
    tile.addEventListener("click", () => openLightbox(i));
    grid.appendChild(tile);
  });
}

function render() {
  if (!photos.length) { closeLightbox(); return; }
  if (index < 0) index = 0;
  if (index >= photos.length) index = photos.length - 1;
  const photo = photos[index];
  frame.src = "/photos/" + encodeURIComponent(photo.name);
  frame.alt = displayName(photo.name);
  counter.textContent = (index + 1) + " / " + photos.length;
  deleteBtn.hidden = !(admin || (me && photo.owner === me));
}

function openLightbox(i) {
  if (!photos.length) { closeLightbox(); return; }
  index = Math.min(Math.max(i, 0), photos.length - 1);
  lightbox.hidden = false;
  render();
}

function closeLightbox() {
  lightbox.hidden = true;
  frame.removeAttribute("src");
  frame.alt = "";
}

function step(d) {
  if (!photos.length) { closeLightbox(); return; }
  index = (index + d + photos.length) % photos.length;
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

fileInput.addEventListener("change", async () => {
  if (!me) {
    getStatusEl().textContent = "Log in to upload";
    fileInput.value = "";
    return;
  }
  const failures = [];
  for (const file of fileInput.files) {
    const ext = fileExt(file.name);
    if (!ALLOWED_EXTS.includes(ext)) {
      failures.push(file.name + ": unsupported file type");
      continue;
    }
    if (file.size > MAX_BYTES) {
      failures.push(file.name + ": file is too large (20MB max)");
      continue;
    }
    let contentType = file.type;
    if (!contentType) contentType = MIME_BY_EXT[ext] || "";
    const headers = {};
    if (contentType) headers["Content-Type"] = contentType;
    let res;
    try {
      res = await fetch("/api/photos/" + encodeURIComponent(file.name), {
        method: "PUT",
        headers,
        body: file,
      });
    } catch (_) {
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
    await res.json();
  }
  fileInput.value = "";
  await loadPhotos();
  if (!failures.length && photos.length > 0) openLightbox(0);
  const statusEl = getStatusEl();
  statusEl.textContent = failures.length ? failures.join("; ") : "";
});

async function refreshMe() {
  try {
    const res = await fetch("/api/me");
    me = res.ok ? (await res.json()).username : null;
  } catch (_) {
    me = null;
  }
  authBlock.hidden = me !== null;
  userBlock.hidden = me === null;
  userLabel.textContent = me || "";
  uploadBtn.hidden = me === null;
}

async function authAction(path) {
  authError.textContent = "";
  const username = authUsername.value.trim();
  const password = authPassword.value;
  let res;
  try {
    res = await fetch(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ username, password }),
    });
  } catch (_) {
    authError.textContent = "Request failed";
    return;
  }
  if (!res.ok) {
    let msg = res.statusText;
    try {
      const err = await res.json();
      if (err.error) msg = err.error;
    } catch (_) { /* keep status text */ }
    authError.textContent = msg;
    return;
  }
  authPassword.value = "";
  await refreshMe();
  render();
}

loginBtn.addEventListener("click", () => authAction("/api/login"));
registerBtn.addEventListener("click", () => authAction("/api/register"));
logoutBtn.addEventListener("click", async () => {
  await fetch("/api/logout", { method: "POST" });
  me = null;
  admin = false;
  adminPanel.hidden = true;
  await refreshMe();
  render();
});

deleteBtn.addEventListener("click", async () => {
  const photo = photos[index];
  if (!photo) return;
  const res = await fetch("/api/photos/" + encodeURIComponent(photo.name), { method: "DELETE" });
  if (!res.ok) {
    const statusEl = getStatusEl();
    let msg = res.statusText;
    try {
      const err = await res.json();
      if (err.error) msg = err.error;
    } catch (_) { /* keep status text */ }
    statusEl.textContent = "Delete failed: " + msg;
    return;
  }
  await loadPhotos();
  if (!photos.length) closeLightbox();
  else {
    if (index >= photos.length) index = photos.length - 1;
    render();
  }
  if (admin) loadAdminUsers();
});

adminUnlockBtn.addEventListener("click", async () => {
  adminError.textContent = "";
  let res;
  try {
    res = await fetch("/api/admin/unlock", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ password: adminPassword.value }),
    });
  } catch (_) {
    adminError.textContent = "Request failed";
    return;
  }
  if (!res.ok) {
    adminError.textContent = res.status === 404 ? "Admin panel disabled" : "Wrong password";
    return;
  }
  admin = true;
  adminPassword.value = "";
  adminPanel.hidden = false;
  await loadAdminUsers();
  render();
});

async function loadAdminUsers() {
  const res = await fetch("/api/admin/users");
  if (!res.ok) { admin = false; adminPanel.hidden = true; return; }
  const data = await res.json();
  adminUsers.textContent = "";
  for (const u of data.users || []) {
    const li = document.createElement("li");
    li.textContent = u.username + " (" + u.photos + ")";
    adminUsers.appendChild(li);
  }
}

themeBtn.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem("theme", next); } catch (_) { /* private mode */ }
});

refreshMe();
loadPhotos();
