"""Thorough stdlib-only tests for Gally (server.py).

Run from repo root:

    python3 -m unittest discover -s tests -v

What is covered:
  * pure helpers: sanitize_filename, owner_of, parse_cookies,
    session/admin cookies, read_json_body, photo_entries, session expiry
  * HTTP API over a real server on an ephemeral port with an isolated
    photos dir + sqlite DB per test:
      static files, register/login/logout/me, photo list/upload/serve/
      delete, admin unlock + user listing, error/edge cases incl. raw
      socket requests (missing/garbage Content-Length, oversized claim)
"""

import http.client
import io
import json
import os
import re
import socket
import sqlite3
import sys
import tempfile
import threading
import time
import types
import unittest
from http.server import ThreadingHTTPServer
from urllib.parse import quote

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import server


def make_handler(headers=None, body=b""):
    """Fake handler exposing just what parse_cookies/read_json_body need."""
    return types.SimpleNamespace(headers=headers or {}, rfile=io.BytesIO(body))


class ServerTestBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self._old_photos = server.PHOTOS_DIR
        self._old_db = server.DB_PATH
        server.PHOTOS_DIR = os.path.join(self.tmp.name, "photos")
        server.DB_PATH = os.path.join(self.tmp.name, "users.db")
        server.init_db()
        server.ADMIN_TOKENS.clear()
        self._old_admin_pass = os.environ.pop("ADMIN_PASS", None)
        os.makedirs(server.PHOTOS_DIR, exist_ok=True)

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
        self.port = self.srv.server_address[1]
        self._thread = threading.Thread(
            target=self.srv.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True
        )
        self._thread.start()
        self.cookies = {}

    def tearDown(self):
        self.srv.shutdown()
        self.srv.server_close()
        self._thread.join(timeout=10)
        server.PHOTOS_DIR = self._old_photos
        server.DB_PATH = self._old_db
        server.ADMIN_TOKENS.clear()
        if self._old_admin_pass is None:
            os.environ.pop("ADMIN_PASS", None)
        else:
            os.environ["ADMIN_PASS"] = self._old_admin_pass
        self.tmp.cleanup()

    # -- HTTP helpers -------------------------------------------------
    def _req(self, method, path, body=None, headers=None, cookies=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        hs = dict(headers or {})
        jar = self.cookies if cookies is None else cookies
        if jar:
            hs["Cookie"] = "; ".join(f"{k}={v}" for k, v in jar.items())
        if isinstance(body, str):
            body = body.encode()
        conn.request(method, path, body=body, headers=hs)
        resp = conn.getresponse()
        rbody = resp.read()
        status = resp.status
        rheaders = resp.getheaders()
        conn.close()
        if cookies is None:
            for k, v in rheaders:
                if k.lower() == "set-cookie":
                    name_val = v.split(";", 1)[0]
                    n, _, val = name_val.partition("=")
                    n = n.strip()
                    if val:
                        self.cookies[n] = val.strip()
                    else:
                        self.cookies.pop(n, None)
        return status, rheaders, rbody

    def _raw(self, head_text, body=b""):
        """Send a hand-crafted request (for malformed-length cases)."""
        s = socket.create_connection(("127.0.0.1", self.port), timeout=10)
        try:
            s.sendall(head_text.encode() + body)
            s.shutdown(socket.SHUT_WR)
            chunks = []
            while True:
                c = s.recv(65536)
                if not c:
                    break
                chunks.append(c)
        finally:
            s.close()
        raw = b"".join(chunks)
        self.assertTrue(raw, "server sent no response")
        return int(raw.split(b" ", 2)[1]), raw

    @staticmethod
    def _hval(headers, name):
        for k, v in headers:
            if k.lower() == name.lower():
                return v
        return None

    def _json(self, method, path, obj, **kw):
        body = json.dumps(obj).encode()
        return self._req(
            method, path, body=body,
            headers={"Content-Type": "application/json"}, **kw,
        )

    def register(self, username="alice", password="password123"):
        return self._json("POST", "/api/register",
                          {"username": username, "password": password})

    def login(self, username="alice", password="password123"):
        return self._json("POST", "/api/login",
                          {"username": username, "password": password})

    def upload(self, name, data, ctype="image/png"):
        return self._req("PUT", "/api/photos/" + quote(name),
                         body=data, headers={"Content-Type": ctype})


# =====================================================================
# Pure helper tests
# =====================================================================

class HelperTests(ServerTestBase):
    def test_sanitize_basic(self):
        base, ext = server.sanitize_filename("photo.png")
        self.assertEqual((base, ext), ("photo.png", ".png"))

    def test_sanitize_strips_directories(self):
        base, ext = server.sanitize_filename("../../etc/passwd.png")
        self.assertEqual((base, ext), ("passwd.png", ".png"))
        self.assertNotIn("/", base)

    def test_sanitize_replaces_illegal_chars_and_lowercases_ext(self):
        base, ext = server.sanitize_filename("my photo @2.JPG")
        self.assertEqual((base, ext), ("my_photo__2.JPG", ".jpg"))
        self.assertTrue(re.fullmatch(r"[A-Za-z0-9._-]+", base))

    def test_sanitize_no_extension(self):
        base, ext = server.sanitize_filename("README")
        self.assertEqual(ext, "")

    def test_sanitize_dotfile(self):
        base, ext = server.sanitize_filename(".hidden.png")
        # ".hidden.png" has a stem, so it survives sanitize (serving still 404s it)
        self.assertEqual(ext, ".png")
        base2, _ = server.sanitize_filename(".png")
        self.assertNotIn("/", base2)

    def test_sanitize_truncates_long_names(self):
        base, ext = server.sanitize_filename("a" * 200 + ".png")
        self.assertLessEqual(len(base), server.MAX_NAME_LEN)
        self.assertEqual(ext, ".png")
        self.assertTrue(base.endswith(".png"))

    def test_sanitize_empty(self):
        base, ext = server.sanitize_filename("")
        self.assertTrue(base)
        self.assertEqual(ext, "")

    def test_owner_of(self):
        self.assertEqual(server.owner_of("alice_" + "a" * 32 + "_pic.png"), "alice")
        self.assertEqual(server.owner_of("bob-1_" + "f" * 32 + "_x.jpg"), "bob-1")
        for bad in ["plain.png", "ab_" + "a" * 32 + "_x.png",  # user too short
                    "alice_short_x.png", ".hidden", "alice_" + "g" * 32 + "_x.png"]:
            with self.subTest(name=bad):
                self.assertIsNone(server.owner_of(bad))

    def test_parse_cookies(self):
        h = make_handler({"Cookie": f"{server.SESSION_COOKIE}=abc; {server.ADMIN_COOKIE}=def"})
        self.assertEqual(server.parse_cookies(h),
                         {server.SESSION_COOKIE: "abc", server.ADMIN_COOKIE: "def"})
        self.assertEqual(server.parse_cookies(make_handler({})), {})
        self.assertEqual(server.parse_cookies(make_handler({"Cookie": ""})), {})
        # value containing '=' survives
        h2 = make_handler({"Cookie": "x=a=b"})
        self.assertEqual(server.parse_cookies(h2), {"x": "a=b"})

    def test_session_cookie_format(self):
        c = server.session_cookie("tok123")
        self.assertTrue(c.startswith(server.SESSION_COOKIE + "=tok123"))
        self.assertIn("HttpOnly", c)
        self.assertIn("SameSite=Lax", c)
        self.assertIn("Path=/", c)
        self.assertIn("Max-Age=%d" % (server.SESSION_DAYS * 86400), c)
        c0 = server.clear_session_cookie()
        self.assertTrue(c0.startswith(server.SESSION_COOKIE + "="))
        self.assertIn("Max-Age=0", c0)

    def test_admin_cookie_roundtrip(self):
        h = make_handler({})
        self.assertFalse(server.is_admin(h))
        c = server.admin_cookie()
        token = c.split("=", 1)[1].split(";", 1)[0]
        h2 = make_handler({"Cookie": f"{server.ADMIN_COOKIE}={token}"})
        self.assertTrue(server.is_admin(h2))
        self.assertFalse(server.is_admin(make_handler(
            {"Cookie": f"{server.ADMIN_COOKIE}=bogus"})))

    def test_new_session_and_session_user(self):
        token = server.new_session("alice")
        h = make_handler({"Cookie": f"{server.SESSION_COOKIE}={token}"})
        self.assertEqual(server.session_user(h), "alice")

    def test_session_user_unknown_or_missing(self):
        self.assertIsNone(server.session_user(make_handler({})))
        h = make_handler({"Cookie": f"{server.SESSION_COOKIE}=nope"})
        self.assertIsNone(server.session_user(h))

    def test_session_expiry_deletes_row(self):
        token = server.new_session("alice")
        old = int(time.time()) - server.SESSION_DAYS * 86400 - 10
        con = server.db()
        try:
            con.execute("UPDATE sessions SET created = ? WHERE token = ?",
                        (old, token))
            con.commit()
        finally:
            con.close()
        h = make_handler({"Cookie": f"{server.SESSION_COOKIE}={token}"})
        self.assertIsNone(server.session_user(h))
        con = server.db()
        try:
            row = con.execute("SELECT * FROM sessions WHERE token = ?",
                              (token,)).fetchone()
        finally:
            con.close()
        self.assertIsNone(row)

    def test_read_json_body_ok(self):
        raw = json.dumps({"a": 1}).encode()
        h = make_handler({"Content-Length": str(len(raw))}, raw)
        self.assertEqual(server.read_json_body(h), {"a": 1})

    def test_read_json_body_rejects(self):
        raw = b'{"a": 1}'
        cases = [
            ({}, raw, "missing length"),
            ({"Content-Length": "0"}, raw, "zero length"),
            ({"Content-Length": "-5"}, raw, "negative length"),
            ({"Content-Length": "notanint"}, raw, "garbage length"),
            ({"Content-Length": "9000"}, raw, "over limit"),
            ({"Content-Length": "100"}, b"short", "short read"),
            ({"Content-Length": "7"}, b"notjson", "invalid json"),
            ({"Content-Length": "2"}, b"\xff\xfe", "bad utf-8"),
        ]
        for headers, body, label in cases:
            with self.subTest(label=label):
                self.assertIsNone(
                    server.read_json_body(make_handler(headers, body)))

    def test_read_json_body_custom_limit(self):
        raw = b'{"a": 1}'
        h = make_handler({"Content-Length": str(len(raw))}, raw)
        self.assertIsNone(server.read_json_body(h, limit=4))

    def test_photo_entries_ordering_and_filtering(self):
        names = {
            "alice_" + "a" * 32 + "_old.png": 1000,
            "bob_" + "b" * 32 + "_new.png": 2000,
            "legacy-no-owner.png": 1500,
        }
        for n, mtime in names.items():
            p = os.path.join(server.PHOTOS_DIR, n)
            with open(p, "wb") as f:
                f.write(b"x")
            os.utime(p, (mtime, mtime))
        # dotfile, directory, and non-file entries are skipped
        with open(os.path.join(server.PHOTOS_DIR, ".hidden"), "wb") as f:
            f.write(b"x")
        os.mkdir(os.path.join(server.PHOTOS_DIR, "subdir"))
        entries = server.photo_entries()
        self.assertEqual([e["name"] for e in entries],
                         ["bob_" + "b" * 32 + "_new.png",
                          "legacy-no-owner.png",
                          "alice_" + "a" * 32 + "_old.png"])
        by_name = {e["name"]: e["owner"] for e in entries}
        self.assertEqual(by_name["bob_" + "b" * 32 + "_new.png"], "bob")
        self.assertIsNone(by_name["legacy-no-owner.png"])

    def test_photo_entries_missing_dir(self):
        server.PHOTOS_DIR = os.path.join(self.tmp.name, "does-not-exist")
        self.assertEqual(server.photo_entries(), [])

    def test_init_db_idempotent(self):
        server.init_db()  # second call on existing DB must not fail
        con = server.db()
        try:
            tables = {r[0] for r in
                      con.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        finally:
            con.close()
        self.assertIn("users", tables)
        self.assertIn("sessions", tables)


# =====================================================================
# Auth API tests
# =====================================================================

class AuthApiTests(ServerTestBase):
    def test_register_success_sets_session_cookie(self):
        status, headers, body = self.register()
        self.assertEqual(status, 201)
        self.assertEqual(json.loads(body)["username"], "alice")
        cookie = self._hval(headers, "Set-Cookie")
        self.assertIsNotNone(cookie)
        self.assertIn(server.SESSION_COOKIE + "=", cookie)
        self.assertIn(server.SESSION_COOKIE, self.cookies)
        # session works for /api/me
        status, _, body = self._req("GET", "/api/me")
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body)["username"], "alice")

    def test_register_duplicate_409(self):
        self.assertEqual(self.register()[0], 201)
        self.cookies = {}
        status, _, body = self.register()
        self.assertEqual(status, 409)
        self.assertIn("error", json.loads(body))

    def test_register_bad_usernames(self):
        for bad in ["ab", "a" * 21, "has space", "semi;colon", "unié",
                    "", "with/slash", 123, None]:
            with self.subTest(username=bad):
                self.cookies = {}
                status, _, body = self._json(
                    "POST", "/api/register",
                    {"username": bad, "password": "password123"})
                self.assertEqual(status, 400)
                self.assertIn("error", json.loads(body))

    def test_register_username_boundaries_ok(self):
        for ok in ["abc", "a" * 20, "a_b-c9"]:
            with self.subTest(username=ok):
                self.cookies = {}
                status, _, _ = self.register(username=ok)
                self.assertEqual(status, 201)

    def test_register_bad_passwords(self):
        for bad in ["short", "x" * 129, "", 12345, None]:
            with self.subTest(password=repr(bad)):
                self.cookies = {}
                status, _, body = self._json(
                    "POST", "/api/register",
                    {"username": "alice", "password": bad})
                self.assertEqual(status, 400)
                self.assertIn("error", json.loads(body))

    def test_register_password_boundary_8_and_128_ok(self):
        self.cookies = {}
        self.assertEqual(self.register("user8", "12345678")[0], 201)
        self.cookies = {}
        self.assertEqual(self.register("user128", "x" * 128)[0], 201)

    def test_register_malformed_bodies(self):
        # invalid JSON with a declared length
        status, _, body = self._req(
            "POST", "/api/register", body=b"{oops",
            headers={"Content-Type": "application/json"})
        self.assertEqual(status, 400)
        # missing Content-Length entirely (raw socket)
        status, _ = self._raw(
            "POST /api/register HTTP/1.1\r\nHost: x\r\n"
            "Content-Type: application/json\r\nConnection: close\r\n\r\n",
            json.dumps({"username": "u", "password": "password123"}).encode())
        self.assertEqual(status, 400)

    def test_login_success_wrong_password_unknown_user(self):
        self.assertEqual(self.register()[0], 201)
        self.cookies = {}
        status, _, body = self.login()
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body)["username"], "alice")
        self.assertIn(server.SESSION_COOKIE, self.cookies)

        self.cookies = {}
        status, _, body = self.login(password="wrongpass1")
        self.assertEqual(status, 401)

        self.cookies = {}
        status, _, body = self.login(username="nobody")
        self.assertEqual(status, 401)

    def test_me_unauthorized(self):
        status, _, body = self._req("GET", "/api/me")
        self.assertEqual(status, 401)
        self.assertIn("error", json.loads(body))

    def test_logout_clears_session(self):
        self.assertEqual(self.register()[0], 201)
        token_cookie = self.cookies.get(server.SESSION_COOKIE)
        self.assertTrue(token_cookie)
        status, headers, body = self._req("POST", "/api/logout")
        self.assertEqual(status, 200)
        self.assertTrue(json.loads(body)["ok"])
        self.assertNotIn(server.SESSION_COOKIE, self.cookies)
        # old token no longer valid even if replayed
        status, _, _ = self._req(
            "GET", "/api/me",
            cookies={server.SESSION_COOKIE: token_cookie})
        self.assertEqual(status, 401)
        # logout without a session still 200s
        status, _, _ = self._req("POST", "/api/logout")
        self.assertEqual(status, 200)

    def test_session_expiry_over_http(self):
        self.assertEqual(self.register()[0], 201)
        token = self.cookies[server.SESSION_COOKIE]
        old = int(time.time()) - server.SESSION_DAYS * 86400 - 10
        con = sqlite3.connect(server.DB_PATH)
        try:
            con.execute("UPDATE sessions SET created = ? WHERE token = ?",
                        (old, token))
            con.commit()
        finally:
            con.close()
        status, _, _ = self._req("GET", "/api/me")
        self.assertEqual(status, 401)

    def test_password_not_stored_plaintext(self):
        self.assertEqual(self.register(password="s3cret-pw")[0], 201)
        con = sqlite3.connect(server.DB_PATH)
        try:
            row = con.execute(
                "SELECT pw_hash, salt FROM users WHERE username='alice'").fetchone()
        finally:
            con.close()
        self.assertIsNotNone(row)
        self.assertNotIn(b"s3cret-pw", row[0])


# =====================================================================
# Photo API tests
# =====================================================================

PNG = b"\x89PNG\r\n\x1a\n" + b"0" * 256


class PhotoApiTests(ServerTestBase):
    def test_list_empty(self):
        status, _, body = self._req("GET", "/api/photos")
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body), {"photos": []})

    def test_upload_requires_login(self):
        status, _, body = self.upload("pic.png", PNG)
        self.assertEqual(status, 401)

    def test_upload_rejects_non_image(self):
        self.assertEqual(self.register()[0], 201)
        for ctype, label in [("text/plain", "text"),
                             ("application/octet-stream", "octet"),
                             ("", "missing")]:
            with self.subTest(ctype=label):
                hdrs = {"Content-Type": ctype} if ctype else {}
                status, _, _ = self._req(
                    "PUT", "/api/photos/pic.png", body=PNG, headers=hdrs)
                self.assertEqual(status, 400)

    def test_upload_missing_and_garbage_length(self):
        self.assertEqual(self.register()[0], 201)
        cookie = f"Cookie: {server.SESSION_COOKIE}={self.cookies[server.SESSION_COOKIE]}\r\n"
        base = ("PUT /api/photos/pic.png HTTP/1.1\r\nHost: x\r\n"
                + cookie + "Content-Type: image/png\r\nConnection: close\r\n")
        status, _ = self._raw(base + "\r\n", PNG)
        self.assertEqual(status, 400)  # missing Content-Length
        status, _ = self._raw(
            base + "Content-Length: abc\r\n\r\n", PNG)
        self.assertEqual(status, 400)  # garbage Content-Length

    def test_upload_empty_body(self):
        self.assertEqual(self.register()[0], 201)
        status, _, body = self._req(
            "PUT", "/api/photos/pic.png", body=b"",
            headers={"Content-Type": "image/png"})
        self.assertEqual(status, 400)

    def test_upload_too_large_claim(self):
        self.assertEqual(self.register()[0], 201)
        cookie = f"Cookie: {server.SESSION_COOKIE}={self.cookies[server.SESSION_COOKIE]}\r\n"
        head = ("PUT /api/photos/big.png HTTP/1.1\r\nHost: x\r\n" + cookie
                + "Content-Type: image/png\r\nConnection: close\r\n"
                + f"Content-Length: {server.MAX_BYTES + 1}\r\n\r\n")
        status, _ = self._raw(head, b"0123456789")
        self.assertEqual(status, 413)
        self.assertEqual(os.listdir(server.PHOTOS_DIR), [])

    def test_upload_unsupported_extension(self):
        self.assertEqual(self.register()[0], 201)
        status, _, body = self.upload("notes.txt", b"hello",
                                      ctype="image/png")
        self.assertEqual(status, 415)
        self.assertEqual(os.listdir(server.PHOTOS_DIR), [])

    def test_upload_success_and_serve(self):
        self.assertEqual(self.register()[0], 201)
        status, _, body = self.upload("My Photo.PNG", PNG)
        self.assertEqual(status, 201)
        stored = json.loads(body)["name"]
        self.assertRegex(
            stored, r"^alice_[0-9a-f]{32}_My_Photo\.PNG$")
        # exact bytes on disk
        with open(os.path.join(server.PHOTOS_DIR, stored), "rb") as f:
            self.assertEqual(f.read(), PNG)
        # listed with owner
        status, _, body = self._req("GET", "/api/photos")
        photos = json.loads(body)["photos"]
        self.assertEqual(len(photos), 1)
        self.assertEqual(photos[0]["owner"], "alice")
        # served with content type + nosniff
        status, headers, body = self._req("GET", "/photos/" + quote(stored))
        self.assertEqual(status, 200)
        self.assertEqual(body, PNG)
        self.assertEqual(self._hval(headers, "Content-Type"), "image/png")
        self.assertEqual(self._hval(headers, "X-Content-Type-Options"), "nosniff")

    def test_upload_content_types(self):
        self.assertEqual(self.register()[0], 201)
        for name, ctype in [("a.jpg", "image/jpeg"), ("a.jpeg", "image/jpeg"),
                            ("a.gif", "image/gif"), ("a.webp", "image/webp")]:
            with self.subTest(name=name):
                status, _, body = self.upload(name, b"data:" + name.encode(),
                                              ctype="image/jpeg")
                self.assertEqual(status, 201)
                stored = json.loads(body)["name"]
                s2, headers, data = self._req("GET", "/photos/" + quote(stored))
                self.assertEqual(s2, 200)
                self.assertEqual(self._hval(headers, "Content-Type"), ctype)

    def test_upload_sanitizes_traversal_name(self):
        self.assertEqual(self.register()[0], 201)
        status, _, body = self.upload("../../evil.png", PNG)
        self.assertEqual(status, 201)
        stored = json.loads(body)["name"]
        self.assertNotIn("/", stored)
        self.assertTrue(stored.endswith("_evil.png"))
        real = os.path.realpath(os.path.join(server.PHOTOS_DIR, stored))
        self.assertTrue(real.startswith(os.path.realpath(server.PHOTOS_DIR)))

    def test_serve_missing_traversal_dotfile(self):
        for path in ["/photos/nope.png", "/photos/..%2Fserver.py",
                     "/photos/.hidden", "/photos/%2E%2E%2Fsecret"]:
            with self.subTest(path=path):
                status, _, _ = self._req("GET", path)
                self.assertEqual(status, 404)

    def test_serve_unknown_extension_octet_stream(self):
        with open(os.path.join(server.PHOTOS_DIR, "legacy.bin"), "wb") as f:
            f.write(b"\x00\x01")
        status, headers, body = self._req("GET", "/photos/legacy.bin")
        self.assertEqual(status, 200)
        self.assertEqual(body, b"\x00\x01")
        self.assertEqual(self._hval(headers, "Content-Type"),
                         "application/octet-stream")

    def test_delete_owner_success(self):
        self.assertEqual(self.register()[0], 201)
        _, _, body = self.upload("pic.png", PNG)
        stored = json.loads(body)["name"]
        status, _, out = self._req("DELETE", "/api/photos/" + quote(stored))
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(out)["deleted"], stored)
        self.assertFalse(os.path.exists(os.path.join(server.PHOTOS_DIR, stored)))
        status, _, _ = self._req("GET", "/photos/" + quote(stored))
        self.assertEqual(status, 404)

    def test_delete_requires_login(self):
        with open(os.path.join(server.PHOTOS_DIR, "orphan.png"), "wb") as f:
            f.write(PNG)
        status, _, _ = self._req("DELETE", "/api/photos/orphan.png")
        self.assertEqual(status, 401)

    def test_delete_forbidden_for_non_owner(self):
        self.assertEqual(self.register("alice")[0], 201)
        _, _, body = self.upload("pic.png", PNG)
        stored = json.loads(body)["name"]
        alice_jar = dict(self.cookies)
        self.cookies = {}
        self.assertEqual(self.register("bobby")[0], 201)
        status, _, _ = self._req("DELETE", "/api/photos/" + quote(stored))
        self.assertEqual(status, 403)
        # file still there and still listed
        self.cookies = alice_jar
        status, _, _ = self._req("GET", "/photos/" + quote(stored))
        self.assertEqual(status, 200)

    def test_delete_missing_and_traversal(self):
        self.assertEqual(self.register()[0], 201)
        # owned-but-missing name passes the owner check, then 404s on unlink
        missing = "alice_" + "a" * 32 + "_gone.png"
        status, _, _ = self._req("DELETE", "/api/photos/" + missing)
        self.assertEqual(status, 404)
        # ownerless missing file fails the owner check first
        status, _, _ = self._req("DELETE", "/api/photos/nonexistent.png")
        self.assertEqual(status, 403)
        for path in ["/api/photos/..%2F..%2Fx", "/api/photos/.hidden"]:
            with self.subTest(path=path):
                status, _, _ = self._req("DELETE", path)
                self.assertEqual(status, 404)


# =====================================================================
# Static + admin + routing tests
# =====================================================================

class StaticAdminRoutingTests(ServerTestBase):
    def test_static_files(self):
        for path, ctype in [("/", "text/html"), ("/index.html", "text/html"),
                            ("/app.js", "application/javascript"),
                            ("/styles.css", "text/css")]:
            with self.subTest(path=path):
                status, headers, body = self._req("GET", path)
                self.assertEqual(status, 200)
                self.assertTrue(body)
                self.assertEqual(self._hval(headers, "Content-Type"), ctype)

    def test_unknown_routes_404(self):
        cases = [("GET", "/nope"), ("GET", "/api/nope"),
                 ("POST", "/api/nope"), ("POST", "/api/photos/x"),
                 ("PUT", "/api/nope"), ("DELETE", "/api/nope"),
                 ("PUT", "/photos/x")]
        for method, path in cases:
            with self.subTest(f"{method} {path}"):
                hdrs = {"Content-Type": "application/json"} \
                    if method == "POST" else {}
                body = b"{}" if method == "POST" else None
                status, _, _ = self._req(method, path, body=body, headers=hdrs)
                self.assertEqual(status, 404)

    def test_admin_disabled_without_pass(self):
        status, _, _ = self._json("POST", "/api/admin/unlock",
                                  {"password": "anything"})
        self.assertEqual(status, 404)
        status, _, _ = self._req("GET", "/api/admin/users")
        self.assertEqual(status, 403)

    def test_admin_unlock_flow(self):
        os.environ["ADMIN_PASS"] = "s3cret"
        status, _, _ = self._json("POST", "/api/admin/unlock",
                                  {"password": "wrong"})
        self.assertEqual(status, 403)
        status, headers, body = self._json("POST", "/api/admin/unlock",
                                           {"password": "s3cret"})
        self.assertEqual(status, 200)
        self.assertTrue(json.loads(body)["ok"])
        self.assertIn(server.ADMIN_COOKIE, self.cookies)

    def test_admin_users_lists_counts(self):
        os.environ["ADMIN_PASS"] = "s3cret"
        # alice uploads 2, bobby uploads 0
        self.assertEqual(self.register("alice")[0], 201)
        self.upload("one.png", PNG)
        self.upload("two.png", PNG)
        self.cookies = {}
        self.assertEqual(self.register("bobby")[0], 201)
        self.cookies = {}  # anonymous admin client
        self.assertEqual(self._json("POST", "/api/admin/unlock",
                                    {"password": "s3cret"})[0], 200)
        status, _, body = self._req("GET", "/api/admin/users")
        self.assertEqual(status, 200)
        users = {u["username"]: u["photos"] for u in json.loads(body)["users"]}
        self.assertEqual(users, {"alice": 2, "bobby": 0})

    def test_admin_only_cookie_cannot_use_session_endpoints(self):
        os.environ["ADMIN_PASS"] = "s3cret"
        self.assertEqual(self._json("POST", "/api/admin/unlock",
                                    {"password": "s3cret"})[0], 200)
        status, _, _ = self._req("GET", "/api/me")
        self.assertEqual(status, 401)  # admin != logged-in user

    def test_admin_can_delete_any_photo(self):
        os.environ["ADMIN_PASS"] = "s3cret"
        self.assertEqual(self.register("alice")[0], 201)
        _, _, body = self.upload("pic.png", PNG)
        stored = json.loads(body)["name"]
        self.cookies = {}  # fresh anonymous client
        self.assertEqual(self._json("POST", "/api/admin/unlock",
                                    {"password": "s3cret"})[0], 200)
        status, _, out = self._req("DELETE", "/api/photos/" + quote(stored))
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(out)["deleted"], stored)


if __name__ == "__main__":
    unittest.main()
