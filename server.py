#!/usr/bin/env python3
"""Stdlib-only static + raw-byte photo server with key-scoped galleries.

Model: whoever uploads picks a secret key (e.g. ``MyPage2``). The key maps
to a public gallery at ``/<adjective>-<noun>-<n>``. Only the key hash is
stored; the gallery slug is random and unguessable-by-key.
"""
import hashlib
import hmac
import json
import os
import re
import secrets
import shutil
import sqlite3
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote

ROOT = os.path.dirname(os.path.abspath(__file__))
PHOTOS_DIR = os.path.join(ROOT, "photos")
MAX_BYTES = 20 * 1024 * 1024
CHUNK = 65536
MAX_NAME_LEN = 100
ALLOWED_EXTS = {".jpg", ".jpeg", ".png", ".gif", ".webp"}
DB_PATH = os.path.join(ROOT, "galleries.db")
KEY_MIN_LEN = 4
KEY_MAX_LEN = 128
KEY_HEADER = "X-Gallery-Key"
ADMIN_TOKENS = set()
ADMIN_COOKIE = "admin"
RATE_MAX = 60
RATE_WINDOW = 60.0
RATE_LOCK = threading.Lock()
RATE_HITS = {}
SLUG_RE = re.compile(r"^[a-z]+-[a-z]+-[1-9][0-9]{0,2}$")
FILE_SLUG_RE = re.compile(r"^([a-z]+-[a-z]+-[0-9]{1,3})_[0-9a-f]{32}_")

ADJECTIVES = [
    "amber", "arctic", "ashen", "autumn", "azure", "birch", "bold",
    "brave", "brisk", "bronze", "calm", "cedar", "chilly", "cinder",
    "clear", "cobalt", "comet", "copper", "coral", "crimson", "daring",
    "dawn", "ember", "frosty", "gentle", "glacier", "golden", "granite",
    "harbor", "hasty", "hazel", "honey", "ivory", "juniper", "kind",
    "lunar", "maple", "marble", "meadow", "misty", "mossy", "nimble",
    "noble", "ochre", "olive", "opal", "pebble", "pine", "plucky",
    "quartz", "quiet", "rapid", "redwood", "rocky", "ruby", "russet",
    "sandy", "silent", "silky", "silver", "sleepy", "solar", "spruce",
    "stormy", "sunny", "swift", "tawny", "tidal", "timber", "velvet",
    "verdant", "wheat", "willow", "wispy",
]
NOUNS = [
    "acorn", "anchor", "arrow", "aspen", "badger", "basin", "beacon",
    "beaver", "brook", "burrow", "cabin", "canyon", "cavern", "cliff",
    "cove", "crane", "creek", "dune", "eagle", "elm", "falcon", "fern",
    "finch", "fjord", "fox", "garnet", "glade", "grove", "gull",
    "heron", "hill", "hollow", "inlet", "island", "jay", "kelp",
    "lagoon", "lark", "lichen", "lotus", "meadowlark", "mesa", "moth",
    "newt", "oasis", "otter", "owl", "oxbow", "peak", "pond", "poplar",
    "prairie", "quail", "raven", "reef", "ridge", "river", "robin",
    "sedge", "sparrow", "stone", "stork", "summit", "tern", "thrush",
    "trail", "trout", "tundra", "valley", "vole", "wren",
]

EXT_CONTENT_TYPES = {
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
    ".gif": "image/gif",
    ".webp": "image/webp",
}
STATIC_FILES = {
    "/": ("index.html", "text/html"),
    "/index.html": ("index.html", "text/html"),
    "/app.js": ("app.js", "application/javascript"),
    "/styles.css": ("styles.css", "text/css"),
    "/fonts/Figtree-400.woff2": ("fonts/Figtree-400.woff2", "font/woff2"),
    "/fonts/Figtree-500.woff2": ("fonts/Figtree-500.woff2", "font/woff2"),
    "/fonts/Figtree-600.woff2": ("fonts/Figtree-600.woff2", "font/woff2"),
}


def json_response(handler, status, obj):
    body = json.dumps(obj).encode()
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


def sanitize_filename(raw):
    base = os.path.basename(raw)
    base = re.sub(r"[^A-Za-z0-9._-]", "_", base)
    if not base or base in (".", ".."):
        base = "file"
    if len(base) > MAX_NAME_LEN:
        name, dot, ext = base.rpartition(".")
        suffix = ("." + ext.lower()) if dot and name else ""
        if len(suffix) >= MAX_NAME_LEN:
            suffix = suffix[:MAX_NAME_LEN]
        keep = MAX_NAME_LEN - len(suffix)
        stem = name if dot and name else base
        base = stem[:keep] + suffix
    name, dot, ext = base.rpartition(".")
    ext = ("." + ext.lower()) if dot and name else ""
    return base, ext


def normalize_key(raw):
    """Return the stripped key, or None when missing/invalid."""
    if not isinstance(raw, str):
        return None
    key = raw.strip()
    if not KEY_MIN_LEN <= len(key) <= KEY_MAX_LEN:
        return None
    return key


def key_hash(key):
    return hashlib.sha256(key.encode("utf-8")).digest()


def request_key(handler):
    return normalize_key(handler.headers.get(KEY_HEADER, ""))


def _consume_exact(handler, length):
    """Read exactly `length` bytes, discarding them. False on short read."""
    remaining = length
    try:
        while remaining > 0:
            chunk = handler.rfile.read(min(CHUNK, remaining))
            if not chunk:
                return False
            remaining -= len(chunk)
    except OSError:
        return False
    return remaining == 0


def init_db():
    con = sqlite3.connect(DB_PATH)
    try:
        con.execute(
            "CREATE TABLE IF NOT EXISTS galleries("
            "slug TEXT PRIMARY KEY, key_hash BLOB UNIQUE NOT NULL, "
            "created INTEGER NOT NULL)"
        )
        con.execute(
            "CREATE TABLE IF NOT EXISTS banned_keys("
            "key_hash BLOB PRIMARY KEY, created INTEGER NOT NULL)"
        )
        con.commit()
    finally:
        con.close()


def db():
    con = sqlite3.connect(DB_PATH)
    con.row_factory = sqlite3.Row
    return con


def new_slug(con):
    """Generate an unused readable slug. Raises on repeated collision."""
    for _ in range(50):
        slug = "%s-%s-%d" % (
            secrets.choice(ADJECTIVES),
            secrets.choice(NOUNS),
            secrets.randbelow(999) + 1,
        )
        row = con.execute(
            "SELECT 1 FROM galleries WHERE slug = ?", (slug,)
        ).fetchone()
        if row is None:
            return slug
    raise RuntimeError("slug space exhausted")


def is_banned(con, digest):
    return con.execute(
        "SELECT 1 FROM banned_keys WHERE key_hash = ?", (digest,)
    ).fetchone() is not None


def lookup_gallery(key):
    """Slug for `key`, or None when unknown/banned."""
    digest = key_hash(key)
    con = db()
    try:
        if is_banned(con, digest):
            return None
        row = con.execute(
            "SELECT slug FROM galleries WHERE key_hash = ?", (digest,)
        ).fetchone()
        return row["slug"] if row else None
    finally:
        con.close()


def get_or_create_gallery(key):
    """Slug for `key`, creating the gallery. None when the key is banned."""
    digest = key_hash(key)
    con = db()
    try:
        if is_banned(con, digest):
            return None
        row = con.execute(
            "SELECT slug FROM galleries WHERE key_hash = ?", (digest,)
        ).fetchone()
        if row:
            return row["slug"]
        for _ in range(50):
            slug = new_slug(con)
            try:
                con.execute(
                    "INSERT INTO galleries(slug, key_hash, created)"
                    " VALUES(?, ?, ?)",
                    (slug, digest, int(time.time())),
                )
                con.commit()
                return slug
            except sqlite3.IntegrityError:
                # Slug taken (race) or key inserted (race): re-read.
                con.rollback()
                row = con.execute(
                    "SELECT slug FROM galleries WHERE key_hash = ?",
                    (digest,),
                ).fetchone()
                if row:
                    return row["slug"]
        raise RuntimeError("slug space exhausted")
    finally:
        con.close()


def gallery_exists(slug):
    con = db()
    try:
        return con.execute(
            "SELECT 1 FROM galleries WHERE slug = ?", (slug,)
        ).fetchone() is not None
    finally:
        con.close()


def parse_cookies(handler):
    raw = handler.headers.get("Cookie", "")
    out = {}
    for part in raw.split(";"):
        name, sep, value = part.strip().partition("=")
        if sep and name:
            out[name.strip()] = value.strip()
    return out


def send_json(handler, status, obj, extra_headers=()):
    body = json.dumps(obj).encode()
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    for name, value in extra_headers:
        handler.send_header(name, value)
    handler.end_headers()
    handler.wfile.write(body)


def is_admin(handler):
    return parse_cookies(handler).get(ADMIN_COOKIE, "") in ADMIN_TOKENS


def admin_cookie():
    token = secrets.token_urlsafe(32)
    ADMIN_TOKENS.add(token)
    return ADMIN_COOKIE + "=" + token + "; HttpOnly; SameSite=Lax; Path=/"


def photo_slug(name):
    m = FILE_SLUG_RE.match(name)
    return m.group(1) if m else None


def gallery_photo_entries(slug):
    prefix = slug + "_"
    try:
        entries = os.listdir(PHOTOS_DIR)
    except OSError:
        return []
    out = []
    for n in entries:
        if n.startswith(".") or not n.startswith(prefix):
            continue
        p = os.path.join(PHOTOS_DIR, n)
        try:
            if not os.path.isfile(p):
                continue
            out.append((os.path.getmtime(p), n))
        except OSError:
            continue
    out.sort(key=lambda t: t[0], reverse=True)
    return [{"name": n} for _, n in out]


def all_gallery_counts():
    """Map slug -> photo count from a single directory listing."""
    try:
        entries = os.listdir(PHOTOS_DIR)
    except OSError:
        return {}
    counts = {}
    for n in entries:
        if n.startswith("."):
            continue
        slug = photo_slug(n)
        if slug:
            counts[slug] = counts.get(slug, 0) + 1
    return counts


def read_json_body(handler, limit=8192):
    try:
        length = int(handler.headers.get("Content-Length", ""))
    except (TypeError, ValueError):
        return None
    if length <= 0 or length > limit:
        return None
    try:
        raw = handler.rfile.read(length)
    except OSError:
        return None
    if len(raw) != length:
        return None
    try:
        return json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        return None


def rate_limited(ip):
    now = time.monotonic()
    with RATE_LOCK:
        hits = RATE_HITS.get(ip, [])
        hits = [t for t in hits if now - t < RATE_WINDOW]
        if len(hits) >= RATE_MAX:
            RATE_HITS[ip] = hits
            return True
        hits.append(now)
        RATE_HITS[ip] = hits
        return False


class Handler(BaseHTTPRequestHandler):
    server_version = "Photos/1.0"
    timeout = 30

    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path.startswith("/api/galleries/") and path.endswith("/photos"):
            slug = unquote(path[len("/api/galleries/"):-len("/photos")])
            if SLUG_RE.match(slug) is None or not gallery_exists(slug):
                json_response(self, 404, {"error": "unknown gallery"})
                return
            json_response(self, 200, {
                "slug": slug,
                "photos": gallery_photo_entries(slug),
            })
            return
        if path == "/api/admin/galleries":
            if not is_admin(self):
                json_response(self, 403, {"error": "forbidden"})
                return
            counts = all_gallery_counts()
            con = db()
            try:
                rows = con.execute(
                    "SELECT slug, created FROM galleries ORDER BY created DESC"
                ).fetchall()
            finally:
                con.close()
            json_response(self, 200, {"galleries": [
                {"slug": r["slug"], "created": r["created"],
                 "photos": counts.get(r["slug"], 0)}
                for r in rows
            ]})
            return
        if path == "/api/admin/banned":
            if not is_admin(self):
                json_response(self, 403, {"error": "forbidden"})
                return
            con = db()
            try:
                rows = con.execute(
                    "SELECT created FROM banned_keys ORDER BY created DESC"
                ).fetchall()
            finally:
                con.close()
            json_response(self, 200, {
                "banned": [{"created": r["created"]} for r in rows],
            })
            return
        if path.startswith("/photos/"):
            name = unquote(path[len("/photos/"):])
            if "/" in name or name.startswith("."):
                json_response(self, 404, {"error": "not found"})
                return
            fpath = os.path.realpath(os.path.join(PHOTOS_DIR, name))
            if not fpath.startswith(os.path.realpath(PHOTOS_DIR) + os.sep):
                json_response(self, 404, {"error": "not found"})
                return
            if not os.path.isfile(fpath):
                json_response(self, 404, {"error": "not found"})
                return
            _, dot, raw_ext = name.rpartition(".")
            ctype = EXT_CONTENT_TYPES.get(("." + raw_ext.lower()) if dot else "", "application/octet-stream")
            try:
                size = os.path.getsize(fpath)
                f = open(fpath, "rb")
            except OSError:
                json_response(self, 404, {"error": "not found"})
                return
            with f:
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(size))
                self.send_header("X-Content-Type-Options", "nosniff")
                self.end_headers()
                shutil.copyfileobj(f, self.wfile, CHUNK)
            return
        if path in STATIC_FILES:
            fname, ctype = STATIC_FILES[path]
            fpath = os.path.join(ROOT, fname)
            try:
                with open(fpath, "rb") as f:
                    body = f.read()
            except OSError:
                json_response(self, 404, {"error": "not found"})
                return
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        slug = unquote(path[1:]) if path.startswith("/") else ""
        if slug and "/" not in slug and SLUG_RE.match(slug):
            if not gallery_exists(slug):
                json_response(self, 404, {"error": "unknown gallery"})
                return
            fpath = os.path.join(ROOT, "index.html")
            try:
                with open(fpath, "rb") as f:
                    body = f.read()
            except OSError:
                json_response(self, 404, {"error": "not found"})
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        json_response(self, 404, {"error": "not found"})

    def do_PUT(self):
        path = self.path.split("?", 1)[0]
        if not path.startswith("/api/photos/"):
            json_response(self, 404, {"error": "not found"})
            return
        raw_length = self.headers.get("Content-Length")
        length = None
        if raw_length is not None:
            try:
                length = int(raw_length)
            except (TypeError, ValueError):
                length = None
        if rate_limited(self.client_address[0]):
            if length and length > 0:
                _consume_exact(self, min(length, MAX_BYTES + CHUNK))
            else:
                self.close_connection = True
            json_response(self, 429, {"error": "too many uploads"})
            return
        key = request_key(self)
        if key is None:
            if length and length > 0:
                _consume_exact(self, min(length, MAX_BYTES + CHUNK))
            else:
                self.close_connection = True
            json_response(self, 401, {"error": "gallery key required"})
            return
        ctype = self.headers.get("Content-Type", "")
        if not ctype.startswith("image/"):
            if length is None or length <= 0:
                self.close_connection = True
            elif not _consume_exact(self, length):
                self.close_connection = True
            json_response(self, 400, {"error": "not an image"})
            return
        if raw_length is None:
            json_response(self, 400, {"error": "missing content-length"})
            return
        if length is None:
            json_response(self, 400, {"error": "missing content-length"})
            return
        if length == 0:
            json_response(self, 400, {"error": "empty body"})
            return
        if length < 0:
            json_response(self, 400, {"error": "missing content-length"})
            return
        if length > MAX_BYTES:
            # Do NOT drain the full claimed body; read at most one
            # chunk so a bogus giant Content-Length cannot slow-loris us.
            try:
                self.rfile.read(min(CHUNK, length))
            except OSError:
                pass
            self.close_connection = True
            json_response(self, 413, {"error": "file too large"})
            return
        slug = get_or_create_gallery(key)
        if slug is None:
            if not _consume_exact(self, length):
                self.close_connection = True
            json_response(self, 403, {"error": "gallery banned"})
            return
        raw = unquote(path[len("/api/photos/"):])
        sanitized, ext = sanitize_filename(raw)
        if ext not in ALLOWED_EXTS:
            if not _consume_exact(self, length):
                self.close_connection = True
            json_response(self, 415, {"error": "unsupported type"})
            return
        try:
            os.makedirs(PHOTOS_DIR, exist_ok=True)
        except OSError:
            if not _consume_exact(self, length):
                self.close_connection = True
            json_response(self, 500, {"error": "cannot store"})
            return
        photos_real = os.path.realpath(PHOTOS_DIR)
        fd = None
        stored = None
        fpath = None
        for _ in range(2):
            stored = slug + "_" + uuid.uuid4().hex + "_" + sanitized
            fpath = os.path.realpath(os.path.join(PHOTOS_DIR, stored))
            if not fpath.startswith(photos_real + os.sep):
                self.close_connection = True
                json_response(self, 400, {"error": "bad request"})
                return
            try:
                fd = os.open(fpath, os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_WRONLY, 0o644)
                break
            except FileExistsError:
                fd = None
                continue
            except OSError:
                if not _consume_exact(self, length):
                    self.close_connection = True
                json_response(self, 500, {"error": "cannot store"})
                return
        if fd is None:
            if not _consume_exact(self, length):
                self.close_connection = True
            json_response(self, 500, {"error": "cannot store"})
            return
        remaining = length
        try:
            while remaining > 0:
                try:
                    chunk = self.rfile.read(min(CHUNK, remaining))
                except OSError:
                    chunk = b""
                if not chunk:
                    self.close_connection = True
                    json_response(self, 400, {"error": "bad request"})
                    return
                remaining -= len(chunk)
                view = memoryview(chunk)
                while view:
                    try:
                        written = os.write(fd, view)
                    except OSError:
                        if remaining > 0 and not _consume_exact(self, remaining):
                            self.close_connection = True
                        json_response(self, 500, {"error": "cannot store"})
                        return
                    view = view[written:]
        finally:
            os.close(fd)
        json_response(self, 201, {"name": stored, "slug": slug})

    def do_POST(self):
        path = self.path.split("?", 1)[0]
        if path == "/api/galleries/resolve":
            key = request_key(self)
            if key is None:
                data = read_json_body(self)
                key = normalize_key((data or {}).get("key", ""))
            if key is None:
                json_response(self, 401, {"error": "gallery key required"})
                return
            slug = get_or_create_gallery(key)
            if slug is None:
                json_response(self, 403, {"error": "gallery banned"})
                return
            json_response(self, 200, {"slug": slug})
            return
        if path == "/api/admin/unlock":
            admin_pass = os.environ.get("ADMIN_PASS", "")
            if not admin_pass:
                json_response(self, 404, {"error": "not found"})
                return
            data = read_json_body(self)
            guess = (data or {}).get("password", "")
            if not isinstance(guess, str) or not hmac.compare_digest(guess, admin_pass):
                json_response(self, 403, {"error": "forbidden"})
                return
            send_json(self, 200, {"ok": True},
                      [("Set-Cookie", admin_cookie())])
            return
        if path.startswith("/api/admin/galleries/") and path.endswith("/delete"):
            if not is_admin(self):
                json_response(self, 403, {"error": "forbidden"})
                return
            slug = unquote(path[len("/api/admin/galleries/"):-len("/delete")])
            if SLUG_RE.match(slug) is None:
                json_response(self, 404, {"error": "unknown gallery"})
                return
            con = db()
            try:
                row = con.execute(
                    "SELECT key_hash FROM galleries WHERE slug = ?", (slug,)
                ).fetchone()
                if row is None:
                    json_response(self, 404, {"error": "unknown gallery"})
                    return
                digest = row["key_hash"]
                con.execute("DELETE FROM galleries WHERE slug = ?", (slug,))
                con.execute(
                    "INSERT OR IGNORE INTO banned_keys(key_hash, created)"
                    " VALUES(?, ?)",
                    (digest, int(time.time())),
                )
                con.commit()
            finally:
                con.close()
            removed = 0
            prefix = slug + "_"
            try:
                names = os.listdir(PHOTOS_DIR)
            except OSError:
                names = []
            for n in names:
                if not n.startswith(prefix):
                    continue
                try:
                    os.unlink(os.path.join(PHOTOS_DIR, n))
                    removed += 1
                except OSError:
                    pass
            json_response(self, 200, {"deleted": slug, "files": removed})
            return
        if path == "/api/admin/unban":
            if not is_admin(self):
                json_response(self, 403, {"error": "forbidden"})
                return
            data = read_json_body(self)
            key = normalize_key((data or {}).get("key", ""))
            if key is None:
                json_response(self, 400, {"error": "gallery key required"})
                return
            con = db()
            try:
                cur = con.execute(
                    "DELETE FROM banned_keys WHERE key_hash = ?",
                    (key_hash(key),),
                )
                con.commit()
                if cur.rowcount == 0:
                    json_response(self, 404, {"error": "key not banned"})
                    return
            finally:
                con.close()
            json_response(self, 200, {"ok": True})
            return
        json_response(self, 404, {"error": "not found"})

    def do_DELETE(self):
        path = self.path.split("?", 1)[0]
        if path.startswith("/api/admin/galleries/") and path.endswith("/delete"):
            self.do_POST()
            return
        if not path.startswith("/api/photos/"):
            json_response(self, 404, {"error": "not found"})
            return
        name = unquote(path[len("/api/photos/"):])
        if "/" in name or name.startswith("."):
            json_response(self, 404, {"error": "not found"})
            return
        slug = photo_slug(name)
        if slug is None or not gallery_exists(slug):
            json_response(self, 404, {"error": "not found"})
            return
        if not is_admin(self):
            key = request_key(self)
            if key is None or lookup_gallery(key) != slug:
                json_response(self, 403, {"error": "forbidden"})
                return
        fpath = os.path.realpath(os.path.join(PHOTOS_DIR, name))
        if not fpath.startswith(os.path.realpath(PHOTOS_DIR) + os.sep):
            json_response(self, 404, {"error": "not found"})
            return
        try:
            os.unlink(fpath)
        except FileNotFoundError:
            json_response(self, 404, {"error": "not found"})
            return
        except OSError:
            json_response(self, 500, {"error": "cannot delete"})
            return
        json_response(self, 200, {"deleted": name})


def main():
    os.makedirs(PHOTOS_DIR, exist_ok=True)
    init_db()
    if not os.environ.get("ADMIN_PASS"):
        print("admin panel disabled: set ADMIN_PASS to enable", file=sys.stderr)
    raw_port = os.environ.get("PORT", "8000")
    try:
        port = int(raw_port)
    except (TypeError, ValueError):
        print(f"invalid PORT {raw_port!r}: must be an integer 1-65535", file=sys.stderr)
        raise SystemExit(2)
    if not 1 <= port <= 65535:
        print(f"invalid PORT {port}: must be 1-65535", file=sys.stderr)
        raise SystemExit(2)
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"photos on http://127.0.0.1:{port}")
    server.serve_forever()


if __name__ == "__main__":
    main()
