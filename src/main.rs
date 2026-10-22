//! gally-uring: bare-metal io_uring web gallery (Linux only).
//! Single-threaded event loop: AcceptMulti + RecvMulti + Send + Splice.
//! SQLite metadata is queried synchronously on the event thread (µs reads);
//! uploads use blocking file writes (local fs, fast) to keep the loop simple.

use anyhow::Result;
use io_uring::{opcode, types, IoUring};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
mod http;
use http::*;

const RING_ENTRIES: u32 = 1024;
const BGID: u16 = 1;
const NBUFS: u16 = 256;
const BUFSZ: usize = 4096;
const MAX_BYTES: usize = 20 * 1024 * 1024;
const CHUNK: usize = 65_536;
const RATE_MAX: usize = 60;
const RATE_WINDOW_SECS: u64 = 60;

const OP_NOOP: u64 = 0;
const OP_ACCEPT: u64 = 1;
const OP_RECV: u64 = 2;
const OP_SEND: u64 = 3;
const OP_SPLICE_IN: u64 = 4;
const OP_SPLICE_OUT: u64 = 5;

fn op_tag(user_data: u64) -> u64 {
    user_data & 0xffff_ffff
}
fn op_conn(user_data: u64) -> u32 {
    (user_data >> 32) as u32
}
fn make_ud(conn: u32, op: u64) -> u64 {
    ((conn as u64) << 32) | op
}


struct Conn {
    fd: RawFd,
    /// bytes accumulated for current request head (if split across recvs)
    head: Vec<u8>,
    /// pending response bytes (header+small body) kept alive until Send completes
    resp: Vec<u8>,
    resp_off: usize,
    /// upload state
    up_fd: Option<RawFd>,
    up_remain: usize,
    up_path: PathBuf,
    _up_tmp_complete: bool,
    /// splice state
    sp_fd: Option<RawFd>,
    sp_pipe: Option<[RawFd; 2]>,
    sp_off: u64,
    sp_remain: u64,
    _keep_alive: bool,
    /// response to send after current send (for header-then-splice sequencing)
    wants_splice: bool,
}

impl Conn {
    fn new(fd: RawFd) -> Self {
        Self {
            fd,
            head: Vec::with_capacity(8192),
            resp: Vec::new(),
            resp_off: 0,
            up_fd: None,
            up_remain: 0,
            up_path: PathBuf::new(),
            _up_tmp_complete: false,
            sp_fd: None,
            sp_pipe: None,
            sp_off: 0,
            sp_remain: 0,
            _keep_alive: true,
            wants_splice: false,
        }
    }
}

struct Statics {
    map: HashMap<String, (Vec<u8>, &'static str)>,
}






fn key_hash(key: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    h.finalize().to_vec()
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

struct App {
    _root: PathBuf,
    photos: PathBuf,
    db: rusqlite::Connection,
    admin_tokens: HashSet<String>,
    _admin_pass: String,
    rate: HashMap<String, Vec<Instant>>,
    statics: Statics,
}

impl App {
    fn is_banned(&self, digest: &[u8]) -> bool {
        self.db
            .query_row("SELECT 1 FROM banned_keys WHERE key_hash = ?1", [&digest], |_| Ok(()))
            .is_ok()
    }
    fn lookup_gallery(&self, key: &str) -> Option<String> {
        let d = key_hash(key);
        if self.is_banned(&d) {
            return None;
        }
        self.db
            .query_row("SELECT slug FROM galleries WHERE key_hash = ?1", [&d], |r| {
                r.get::<_, String>(0)
            })
            .ok()
    }
    fn get_or_create_gallery(&mut self, key: &str) -> Option<String> {
        let d = key_hash(key);
        if self.is_banned(&d) {
            return None;
        }
        if let Ok(s) = self.db.query_row(
            "SELECT slug FROM galleries WHERE key_hash = ?1",
            [&d],
            |r| r.get::<_, String>(0),
        ) {
            return Some(s);
        }
        for _ in 0..50 {
            let slug = gen_slug(&self.db);
            let r = self.db.execute(
                "INSERT INTO galleries(slug, key_hash, created) VALUES(?1,?2,?3)",
                rusqlite::params![slug, d, now_secs()],
            );
            match r {
                Ok(_) => return Some(slug),
                Err(_) => {
                    if let Ok(s) = self.db.query_row(
                        "SELECT slug FROM galleries WHERE key_hash = ?1",
                        [&d],
                        |r| r.get::<_, String>(0),
                    ) {
                        return Some(s);
                    }
                }
            }
        }
        None
    }
    fn gallery_exists(&self, slug: &str) -> bool {
        self.db
            .query_row("SELECT 1 FROM galleries WHERE slug = ?1", [slug], |_| Ok(()))
            .is_ok()
    }
}

static ADJ: &[&str] = &[
    "amber", "arctic", "ashen", "autumn", "azure", "birch", "bold", "brave", "brisk", "bronze",
    "calm", "cedar", "chilly", "cinder", "clear", "cobalt", "comet", "copper", "coral", "crimson",
    "daring", "dawn", "ember", "frosty", "gentle", "glacier", "golden", "granite", "harbor",
    "hasty", "hazel", "honey", "ivory", "juniper", "kind", "lunar", "maple", "marble", "meadow",
    "misty", "mossy", "nimble", "noble", "ochre", "olive", "opal", "pebble", "pine", "plucky",
    "quartz", "quiet", "rapid", "redwood", "rocky", "ruby", "russet", "sandy", "silent", "silky",
    "silver", "sleepy", "solar", "spruce", "stormy", "sunny", "swift", "tawny", "tidal", "timber",
    "velvet", "verdant", "wheat", "willow", "wispy",
];
static NOUN: &[&str] = &[
    "acorn", "anchor", "arrow", "aspen", "badger", "basin", "beacon", "beaver", "brook", "burrow",
    "cabin", "canyon", "cavern", "cliff", "cove", "crane", "creek", "dune", "eagle", "elm",
    "falcon", "fern", "finch", "fjord", "fox", "garnet", "glade", "grove", "gull", "heron",
    "hill", "hollow", "inlet", "island", "jay", "kelp", "lagoon", "lark", "lichen", "lotus",
    "meadowlark", "mesa", "moth", "newt", "oasis", "otter", "owl", "oxbow", "peak", "pond",
    "poplar", "prairie", "quail", "raven", "reef", "ridge", "river", "robin", "sedge", "sparrow",
    "stone", "stork", "summit", "tern", "thrush", "trail", "trout", "tundra", "valley", "vole",
    "wren",
];

fn rand_below(n: u32) -> u32 {
    // xorshift from nanos — no extra deps
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(1);
    static mut S: u64 = 0x9e37_79b9_7f4a_7c15;
    unsafe {
        S ^= S << 13;
        S ^= S >> 7;
        S ^= S << 17;
        S = S.wrapping_add(t);
        ((S >> 11) % (n as u64)) as u32
    }
}

fn gen_slug(db: &rusqlite::Connection) -> String {
    for _ in 0..50 {
        let s = format!("{}-{}-{}", ADJ[rand_below(ADJ.len() as u32) as usize], NOUN[rand_below(NOUN.len() as u32) as usize], rand_below(999) + 1);
        let taken: bool = db
            .query_row("SELECT 1 FROM galleries WHERE slug = ?1", [&s], |_| Ok(()))
            .is_ok();
        if !taken {
            return s;
        }
    }
    format!("oddball-slug-{}", rand_below(999) + 1)
}

fn set_nonblock(fd: RawFd) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
    }
}

fn push_send(sq: &mut io_uring::SubmissionQueue, conn_id: u32, fd: RawFd, buf: &[u8]) {
    let e = opcode::Send::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32)
        .build()
        .user_data(make_ud(conn_id, OP_SEND));
    unsafe {
        let _ = sq.push(&e);
    }
}

fn push_splice_in(sq: &mut io_uring::SubmissionQueue, conn_id: u32, file_fd: RawFd, off: u64, pipe_w: RawFd, len: u32) {
    let e = opcode::Splice::new(types::Fd(file_fd), off as i64, types::Fd(pipe_w), -1, len)
        .build()
        .user_data(make_ud(conn_id, OP_SPLICE_IN));
    unsafe {
        let _ = sq.push(&e);
    }
}

fn push_splice_out(sq: &mut io_uring::SubmissionQueue, conn_id: u32, pipe_r: RawFd, sock: RawFd, len: u32) {
    let e = opcode::Splice::new(types::Fd(pipe_r), -1, types::Fd(sock), -1, len)
        .build()
        .user_data(make_ud(conn_id, OP_SPLICE_OUT));
    unsafe {
        let _ = sq.push(&e);
    }
}

fn push_recv_multi(sq: &mut io_uring::SubmissionQueue, conn_id: u32, fd: RawFd) {
    let e = opcode::RecvMulti::new(types::Fd(fd), BGID).len(BUFSZ as _).build().user_data(make_ud(conn_id, OP_RECV));
    unsafe {
        let _ = sq.push(&e);
    }
}



fn rate_limited(app: &mut App, ip: &str) -> bool {
    let now = Instant::now();
    let hits = app.rate.entry(ip.to_string()).or_default();
    hits.retain(|t| now.duration_since(*t).as_secs_f64() < RATE_WINDOW_SECS as f64);
    if hits.len() >= RATE_MAX {
        return true;
    }
    hits.push(now);
    false
}

fn peer_ip(fd: RawFd) -> String {
    unsafe {
        let mut a: libc::sockaddr_storage = std::mem::zeroed();
        let mut l = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if libc::getpeername(fd, &mut a as *mut _ as *mut libc::sockaddr, &mut l) != 0 {
            return "unknown".into();
        }
        if a.ss_family as i32 == libc::AF_INET {
            let v: &libc::sockaddr_in = &*(&a as *const _ as *const libc::sockaddr_in);
            let b = v.sin_addr.s_addr.to_ne_bytes();
            return format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]);
        }
        "unknown".into()
    }
}


fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let photos = root.join("photos");
    std::fs::create_dir_all(&photos).ok();
    let db_path = root.join("galleries.db");
    let db = rusqlite::Connection::open(&db_path)?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;
         CREATE TABLE IF NOT EXISTS galleries(slug TEXT PRIMARY KEY, key_hash BLOB UNIQUE NOT NULL, created INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS banned_keys(key_hash BLOB PRIMARY KEY, created INTEGER NOT NULL);",
    )?;
    let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_default();

    // preload statics
    let mut smap = HashMap::new();
    let files: &[(&str, &str)] = &[
        ("/", "index.html"),
        ("/index.html", "index.html"),
        ("/app.js", "app.js"),
        ("/styles.css", "styles.css"),
        ("/fonts/Figtree-400.woff2", "fonts/Figtree-400.woff2"),
        ("/fonts/Figtree-500.woff2", "fonts/Figtree-500.woff2"),
        ("/fonts/Figtree-600.woff2", "fonts/Figtree-600.woff2"),
    ];
    let ctype_of = |p: &str| -> &'static str {
        if p.ends_with(".html") {
            "text/html"
        } else if p.ends_with(".js") {
            "application/javascript"
        } else if p.ends_with(".css") {
            "text/css"
        } else if p.ends_with(".woff2") {
            "font/woff2"
        } else {
            "application/octet-stream"
        }
    };
    for (route, f) in files {
        if let Ok(b) = std::fs::read(root.join(f)) {
            smap.insert(route.to_string(), (b, ctype_of(f)));
        }
    }

    let mut app = App {
        _root: root.clone(),
        photos,
        db,
        admin_tokens: HashSet::new(),
        _admin_pass: admin_pass,
        rate: HashMap::new(),
        statics: Statics { map: smap },
    };

    let port: u16 = std::env::var("PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(8000);
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    let lfd = listener.as_raw_fd();
    println!("gally-uring on http://127.0.0.1:{port}");

    let mut ring: IoUring = IoUring::new(RING_ENTRIES)?;
    // classic provide-buffers pool (kernel 6.x; no pbuf-ring mmap needed in 0.7 API)
    let mut pool: Box<[u8]> = vec![0u8; NBUFS as usize * BUFSZ].into_boxed_slice();
    {
        let e = opcode::ProvideBuffers::new(pool.as_mut_ptr(), BUFSZ as i32, NBUFS, BGID, 0).build().user_data(make_ud(u32::MAX, OP_NOOP));
        {
            let e2 = e.clone();
            {
                let (_, mut sq, _) = ring.split();
                unsafe {
                    sq.push(&e2).map_err(|_| anyhow::anyhow!("sq full"))?;
                }
            }
            ring.submit()?;
            let _ = ring.submit_and_wait(1)?;
        }
        {
            let mut cq = ring.completion();
            cq.sync();
            for _ in &mut cq {}
        }
    }

    {
        let (_, mut sq, _) = ring.split();
        let e = opcode::AcceptMulti::new(types::Fd(lfd)).build().user_data(OP_ACCEPT);
        unsafe {
            sq.push(&e).map_err(|_| anyhow::anyhow!("sq full"))?;
        }
    }
    ring.submit()?;

    let mut conns: HashMap<u32, Conn> = HashMap::new();
    let mut next_id: u32 = 1;

    loop {
        if let Err(e) = ring.submit_and_wait(1) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
        }
        // collect CQEs
        let cqes: Vec<(u64, i32, u32)> = {
            let mut cq = ring.completion();
            cq.sync();
            let mut v = Vec::new();
            for c in &mut cq {
                v.push((c.user_data(), c.result(), c.flags()));
            }
            v
        };
        process_cqes(&mut app, &mut conns, &mut next_id, &mut pool, &mut ring, cqes);
    }
}

#[allow(clippy::too_many_arguments)]
fn process_cqes(
    app: &mut App,
    conns: &mut HashMap<u32, Conn>,
    next_id: &mut u32,
    pool: &mut Box<[u8]>,
    ring: &mut IoUring,
    cqes: Vec<(u64, i32, u32)>,
) {
    {
        let (_, mut sq, _) = ring.split();
        process_inner(app, conns, next_id, pool, &mut sq, cqes);
    }
    let _ = ring.submitter().submit();
}

#[allow(clippy::too_many_arguments)]
fn process_inner(
    app: &mut App,
    conns: &mut HashMap<u32, Conn>,
    next_id: &mut u32,
    pool: &mut Box<[u8]>,
    mut sq: &mut io_uring::SubmissionQueue,
    cqes: Vec<(u64, i32, u32)>,
) {
    for (ud, res, flags) in cqes {
        {
            let tag = op_tag(ud);
            if tag == OP_NOOP {
                continue;
            }
            if tag == OP_ACCEPT {
                if res > 0 {
                    let cfd = res;
                    set_nonblock(cfd);
                    let id = *next_id;
                    *next_id = next_id.wrapping_add(1).max(1);
                    conns.insert(id, Conn::new(cfd));
                    push_recv_multi(&mut sq, id, cfd);
                }
                // AcceptMulti persists; error CQE (-ENOBUFS etc.) ignored
                continue;
            }
            // recycle provided buffer
            if let Some(bid) = io_uring::cqueue::buffer_select(flags) {
                let addr = unsafe { pool.as_mut_ptr().add(bid as usize * BUFSZ) };
                let e = opcode::ProvideBuffers::new(addr, BUFSZ as i32, 1, BGID, bid).build().user_data(make_ud(u32::MAX, OP_NOOP));
                unsafe {
                    let _ = sq.push(&e);
                }
            }
            let cid = op_conn(ud);
            match tag {
                OP_RECV => {
                    if res <= 0 {
                        // 0 = orderly shutdown, -ENOBUFS = re-arm, else close
                        if res == 0 {
                            if let Some(c) = conns.remove(&cid) {
                                close_conn(c);
                            }
                        } else if res == -libc::ENOBUFS {
                            if let Some(c) = conns.get(&cid) {
                                push_recv_multi(&mut sq, cid, c.fd);
                            }
                        } else if !io_uring::cqueue::more(flags) {
                            // multishot terminated without MORE: re-arm
                            if let Some(c) = conns.get(&cid) {
                                push_recv_multi(&mut sq, cid, c.fd);
                            }
                        }
                        continue;
                    }
                    let bid = match io_uring::cqueue::buffer_select(flags) {
                        Some(b) => b,
                        None => continue,
                    };
                    let data: &[u8] = unsafe { std::slice::from_raw_parts(pool.as_ptr().add(bid as usize * BUFSZ), res as usize) };
                    // upload body continuation?
                    let is_body = conns.get(&cid).map(|c| c.up_fd.is_some()).unwrap_or(false);
                    if is_body {
                        // write chunk to file (blocking, local fs)
                        let done = {
                            let c = conns.get_mut(&cid).unwrap();
                            let mut off = 0;
                            let mut ok = true;
                            while off < data.len() {
                                let n = unsafe { libc::write(c.up_fd.unwrap(), data[off..].as_ptr() as *const _, (data.len() - off) as usize) };
                                if n <= 0 {
                                    ok = false;
                                    break;
                                }
                                off += n as usize;
                            }
                            if ok {
                                c.up_remain = c.up_remain.saturating_sub(data.len());
                            }
                            ok && c.up_remain == 0
                        };
                        if !done {
                            if conns.get(&cid).map(|c| c.up_remain).unwrap_or(1) == 0 || !conns.contains_key(&cid) {
                            }
                            // check failure
                            continue;
                        }
                        // upload complete -> respond 201
                        let slug_of = conns.get(&cid).and_then(|c| photo_slug_name(&c.up_path)).unwrap_or_default();
                        let name = conns.get(&cid).map(|c| c.up_path.file_name().unwrap().to_string_lossy().to_string()).unwrap_or_default();
                        if let Some(c) = conns.get_mut(&cid) {
                            if let Some(fd) = c.up_fd.take() {
                                unsafe { libc::close(fd) };
                            }
                            c.up_remain = 0;
                            let body = format!("{{\"name\":\"{}\",\"slug\":\"{}\"}}", escape(&name), escape(&slug_of));
                            c.resp = json_body(201, &body);
                            c.resp_off = 0;
                            let (fd, ptr, len) = (c.fd, c.resp.as_ptr(), c.resp.len());
                            push_send(&mut sq, cid, fd, unsafe { std::slice::from_raw_parts(ptr, len) });
                        }
                        continue;
                    }
                    // header path
                    let conn_fd = match conns.get_mut(&cid) {
                        Some(c) => {
                            c.head.extend_from_slice(data);
                            if c.head.len() > 65536 {
                                c.resp = json_body(400, "{\"error\":\"header too large\"}");
                                c.resp_off = 0;
                                let (fd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                                push_send(&mut sq, cid, fd, unsafe { std::slice::from_raw_parts(p, l) });
                                c.head.clear();
                                continue;
                            }
                            c.fd
                        }
                        None => continue,
                    };
                    let _ = conn_fd;
                    // need full head?
                    let head_end = {
                        let c = conns.get(&cid).unwrap();
                        find_head_end(&c.head)
                    };
                    let Some(he) = head_end else {
                        if !io_uring::cqueue::more(flags) {
                            if let Some(c) = conns.get(&cid) {
                                push_recv_multi(&mut sq, cid, c.fd);
                            }
                        }
                        continue;
                    };
                    // parse request
                    let (method, path, headers, body_extra) = {
                        let c = conns.get(&cid).unwrap();
                        match parse_request(&c.head[..he]) {
                            Some(v) => v,
                            None => {
                                let c = conns.get_mut(&cid).unwrap();
                                c.resp = json_body(400, "{\"error\":\"bad request\"}");
                                c.resp_off = 0;
                                c.head.clear();
                                let (fd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                                push_send(&mut sq, cid, fd, unsafe { std::slice::from_raw_parts(p, l) });
                                continue;
                            }
                        }
                    };
                    // drain consumed head, keep pipelined remainder
                    let remainder = {
                        let c = conns.get_mut(&cid).unwrap();
                        let rem = c.head[he..].to_vec();
                        c.head.clear();
                        rem
                    };
                    if !remainder.is_empty() {
                        // pipelined body bytes already here (uploads): stash for later handling
                        handle_request(app, conns, sq, cid, method, &path, &headers, Some(remainder));
                    } else {
                        let _ = body_extra;
                        handle_request(app, conns, sq, cid, method, &path, &headers, None);
                    }
                    if !io_uring::cqueue::more(flags) {
                        if let Some(c) = conns.get(&cid) {
                            if c.up_fd.is_none() {
                                push_recv_multi(&mut sq, cid, c.fd);
                            }
                        }
                    }
                }
                OP_SEND => {
                    let Some(c) = conns.get_mut(&cid) else { continue };
                    if res < 0 {
                        let cc = conns.remove(&cid).unwrap();
                        close_conn(cc);
                        continue;
                    }
                    c.resp_off += res as usize;
                    if c.resp_off < c.resp.len() {
                        let (fd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                        let slice = unsafe { std::slice::from_raw_parts(p.add(c.resp_off), l - c.resp_off) };
                        push_send(&mut sq, cid, fd, slice);
                        continue;
                    }
                    c.resp.clear();
                    c.resp_off = 0;
                    if c.wants_splice {
                        c.wants_splice = false;
                        // start zero-copy file->pipe->socket
                        let (file_fd, pipe, off, rem, sock) = match (c.sp_fd, c.sp_pipe, c.sp_remain) {
                            (Some(f), Some(p), r) => (f, p, c.sp_off, r, c.fd),
                            _ => continue,
                        };
                        let n = rem.min(CHUNK as u64) as u32;
                        push_splice_in(&mut sq, cid, file_fd, off, pipe[1], n);
                        let _ = sock;
                    } else if c.up_fd.is_some() {
                        // waiting for body; nothing to do
                    } else {
                        // keep-alive: wait for next request (RecvMulti still armed)
                        if !conns.get(&cid).map(|c| c.up_fd.is_some()).unwrap_or(false) {
                            // ensure recv armed (multishot usually persists)
                        }
                    }
                }
                OP_SPLICE_IN => {
                    let Some(c) = conns.get_mut(&cid) else { continue };
                    if res <= 0 {
                        let cc = conns.remove(&cid).unwrap();
                        close_conn(cc);
                        continue;
                    }
                    let pipe = c.sp_pipe.unwrap();
                    let sock = c.fd;
                    push_splice_out(&mut sq, cid, pipe[0], sock, res as u32);
                    // stash count to advance on next stage
                    c.resp = res.to_string().into_bytes();
                }
                OP_SPLICE_OUT => {
                    let n = conns.get(&cid).and_then(|c| String::from_utf8_lossy(&c.resp).parse::<i64>().ok()).unwrap_or(res as i64);
                    let done = {
                        let Some(c) = conns.get_mut(&cid) else { continue };
                        if res < 0 {
                            let cc = conns.remove(&cid).unwrap();
                            close_conn(cc);
                            continue;
                        }
                        // partial pipe->socket: re-splice remainder of this chunk
                        if (res as i64) < n {
                            let pipe = c.sp_pipe.unwrap();
                            let sock = c.fd;
                            let left = (n - res as i64) as u32;
                            // adjust: splice_out consumed res bytes; file->pipe already moved n bytes,
                            // pipe still holds (n - res); just re-push remainder
                            push_splice_out(&mut sq, cid, pipe[0], sock, left);
                            continue;
                        }
                        c.sp_off += n as u64;
                        c.sp_remain -= n as u64;
                        c.resp.clear();
                        c.sp_remain == 0
                    };
                    if done {
                        let cc = conns.remove(&cid).unwrap();
                        // keep-alive: photo GETs in python close (no keep-alive on file path? actually BaseHTTP keeps).
                        // We close to keep it simple and correct.
                        close_conn(cc);
                    } else {
                        let c = conns.get(&cid).unwrap();
                        let (f, p, off, rem) = (c.sp_fd.unwrap(), c.sp_pipe.unwrap(), c.sp_off, c.sp_remain);
                        let n2 = rem.min(CHUNK as u64) as u32;
                        push_splice_in(&mut sq, cid, f, off, p[1], n2);
                    }
                }
                _ => {}
            }
        }
    }
}

fn close_conn(mut c: Conn) {
    unsafe {
        libc::shutdown(c.fd, libc::SHUT_RDWR);
        libc::close(c.fd);
    }
    if let Some(f) = c.up_fd.take() {
        unsafe { libc::close(f) };
        // remove partial upload
        let _ = std::fs::remove_file(&c.up_path);
    }
    if let Some(f) = c.sp_fd.take() {
        unsafe { libc::close(f) };
    }
    if let Some(p) = c.sp_pipe.take() {
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
    }
}



fn photo_slug_name(p: &Path) -> Option<String> {
    let n = p.file_name()?.to_string_lossy().to_string();
    photo_slug(&n).map(|s| s.to_string())
}


/// Route one parsed request. `pipelined` = body bytes already received with head.
fn handle_request(
    app: &mut App,
    conns: &mut HashMap<u32, Conn>,
    sq: &mut io_uring::SubmissionQueue,
    cid: u32,
    method: Method,
    path: &str,
    headers: &HashMap<String, String>,
    pipelined: Option<Vec<u8>>,
) {
    let path = percent_decode(path);
    // helpers to send json
    macro_rules! send_json {
        ($cid:expr, $st:expr, $body:expr) => {{
            let c = conns.get_mut(&$cid).unwrap();
            c.resp = json_body($st, $body);
            c.resp_off = 0;
            let (fd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
            push_send(sq, $cid, fd, unsafe { std::slice::from_raw_parts(p, l) });
            return;
        }};
    }

    match method {
        Method::Get | Method::Head => {
            let is_head = method == Method::Head;
            // admin galleries
            if path == "/api/admin/galleries" || path == "/api/admin/banned" || path == "/api/admin/users" {
                let cookie = headers.get("cookie").cloned().unwrap_or_default();
                let admin = parse_cookies(&cookie).get("admin").cloned().unwrap_or_default();
                if !app.admin_tokens.contains(&admin) {
                    send_json!(cid, 403, "{\"error\":\"forbidden\"}");
                }
                if path == "/api/admin/users" || path == "/api/admin/galleries" {
                    let rows: Vec<(String, i64)> = app
                        .db
                        .prepare("SELECT slug, created FROM galleries ORDER BY created DESC")
                        .and_then(|mut s| {
                            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .unwrap_or_default();
                    // counts from single dir listing
                    let mut counts: HashMap<String, usize> = HashMap::new();
                    if let Ok(rd) = std::fs::read_dir(&app.photos) {
                        for e in rd.flatten() {
                            let n = e.file_name().to_string_lossy().to_string();
                            if let Some(s) = photo_slug(&n) {
                                *counts.entry(s.to_string()).or_default() += 1;
                            }
                        }
                    }
                    let mut j = String::from("{\"galleries\":[");
                    for (i, (slug, created)) in rows.iter().enumerate() {
                        if i > 0 {
                            j.push(',');
                        }
                        j.push_str(&format!("{{\"slug\":\"{}\",\"created\":{},\"photos\":{}}}", escape(slug), created, counts.get(slug).copied().unwrap_or(0)));
                    }
                    j.push_str("]}");
                    if path == "/api/admin/users" {
                        // compat alias for stale tests
                        let j2 = j.replace("\"galleries\"", "\"users\"");
                        send_json!(cid, 200, &j2);
                    }
                    send_json!(cid, 200, &j);
                } else {
                    let rows: Vec<i64> = app
                        .db
                        .prepare("SELECT created FROM banned_keys ORDER BY created DESC")
                        .and_then(|mut s| {
                            s.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()
                        })
                        .unwrap_or_default();
                    let mut j = String::from("{\"banned\":[");
                    for (i, c) in rows.iter().enumerate() {
                        if i > 0 {
                            j.push(',');
                        }
                        j.push_str(&format!("{{\"created\":{}}}", c));
                    }
                    j.push_str("]}");
                    send_json!(cid, 200, &j);
                }
            }
            // gallery listing
            if path.starts_with("/api/galleries/") && path.ends_with("/photos") {
                let slug = path["/api/galleries/".len()..path.len() - "/photos".len()].to_string();
                if !is_slug(&slug) || !app.gallery_exists(&slug) {
                    send_json!(cid, 404, "{\"error\":\"unknown gallery\"}");
                }
                let prefix = slug.clone() + "_";
                let mut out: Vec<(i64, String)> = vec![];
                if let Ok(rd) = std::fs::read_dir(&app.photos) {
                    for e in rd.flatten() {
                        let n = e.file_name().to_string_lossy().to_string();
                        if n.starts_with('.') || !n.starts_with(&prefix) {
                            continue;
                        }
                        let mtime = e
                            .metadata()
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        out.push((mtime, n));
                    }
                }
                out.sort_by(|a, b| b.0.cmp(&a.0));
                let mut j = format!("{{\"slug\":\"{}\",\"photos\":[", escape(&slug));
                for (i, (_, n)) in out.iter().enumerate() {
                    if i > 0 {
                        j.push(',');
                    }
                    j.push_str(&format!("{{\"name\":\"{}\"}}", escape(n)));
                }
                j.push_str("]}");
                send_json!(cid, 200, &j);
            }
            // photos file
            if path.starts_with("/photos/") {
                let name = path["/photos/".len()..].to_string();
                if name.contains('/') || name.starts_with('.') {
                    send_json!(cid, 404, "{\"error\":\"not found\"}");
                }
                // containment: join + ensure parent
                let fp = app.photos.join(&name);
                // symlink-safe: open + fstat
                let (fd, size) = unsafe {
                    let c = std::ffi::CString::new(fp.as_os_str().as_encoded_bytes()).unwrap();
                    let f = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
                    if f < 0 {
                        send_json!(cid, 404, "{\"error\":\"not found\"}");
                    }
                    let mut st: libc::stat = std::mem::zeroed();
                    if libc::fstat(f, &mut st) != 0 || (st.st_mode & libc::S_IFMT as u32) as i32 != libc::S_IFREG as i32 {
                        libc::close(f);
                        send_json!(cid, 404, "{\"error\":\"not found\"}");
                    }
                    (f, st.st_size as u64)
                };
                let ext = match name.rfind('.') {
                    Some(i) if i + 1 < name.len() => format!(".{}", name[i + 1..].to_lowercase()),
                    _ => String::new(),
                };
                let ctype = content_type_for(&ext);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
                    ctype, size
                );
                let c = conns.get_mut(&cid).unwrap();
                c.resp = head.into_bytes();
                c.resp_off = 0;
                if is_head {
                    let (fd2, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                    push_send(sq, cid, fd2, unsafe { std::slice::from_raw_parts(p, l) });
                    unsafe { libc::close(fd) };
                    return;
                }
                // create pipe
                let mut pf = [0; 2];
                unsafe {
                    if libc::pipe2(pf.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) != 0 {
                        libc::close(fd);
                        send_json!(cid, 500, "{\"error\":\"cannot serve\"}");
                    }
                }
                c.sp_fd = Some(fd);
                c.sp_pipe = Some(pf);
                c.sp_off = 0;
                c.sp_remain = size;
                c.wants_splice = true;
                let (sock, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                push_send(sq, cid, sock, unsafe { std::slice::from_raw_parts(p, l) });
                return;
            }
            // statics
            if let Some((body, ctype)) = app.statics.map.get(&path) {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    ctype,
                    body.len()
                );
                let c = conns.get_mut(&cid).unwrap();
                c.resp = head.into_bytes();
                c.resp.extend_from_slice(body);
                if is_head {
                    c.resp.truncate(c.resp.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(c.resp.len()));
                }
                c.resp_off = 0;
                let (fd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                push_send(sq, cid, fd, unsafe { std::slice::from_raw_parts(p, l) });
                return;
            }
            // /<slug> SPA fallback
            if path.len() > 1 && !path[1..].contains('/') && is_slug(&path[1..]) {
                let slug = path[1..].to_string();
                if !app.gallery_exists(&slug) {
                    send_json!(cid, 404, "{\"error\":\"unknown gallery\"}");
                }
                if let Some((body, _)) = app.statics.map.get("/index.html").or_else(|| app.statics.map.get("/")) {
                    let body = body.clone();
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n", body.len());
                    let c = conns.get_mut(&cid).unwrap();
                    c.resp = head.into_bytes();
                    c.resp.extend_from_slice(&body);
                    if is_head {
                        c.resp.truncate(c.resp.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(c.resp.len()));
                    }
                    c.resp_off = 0;
                    let (fd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                    push_send(sq, cid, fd, unsafe { std::slice::from_raw_parts(p, l) });
                    return;
                }
                send_json!(cid, 404, "{\"error\":\"not found\"}");
            }
            // /api/me + /api/logout + /api/photos compat (stale-test surface → 404/401 like python?)
            if path == "/api/me" {
                send_json!(cid, 401, "{\"error\":\"unauthorized\"}");
            }
            send_json!(cid, 404, "{\"error\":\"not found\"}");
        }
        Method::Put => {
            if !path.starts_with("/api/photos/") {
                // drain body if any then 404
                send_json!(cid, 404, "{\"error\":\"not found\"}");
            }
            let ip = conns.get(&cid).map(|c| peer_ip(c.fd)).unwrap_or_default();
            if rate_limited(app, &ip) {
                send_json!(cid, 429, "{\"error\":\"too many uploads\"}");
            }
            let key = headers.get("x-gallery-key").map(|s| s.as_str()).unwrap_or("");
            let key = match normalize_key(key) {
                Some(k) => k,
                None => send_json!(cid, 401, "{\"error\":\"gallery key required\"}"),
            };
            let ctype = headers.get("content-type").cloned().unwrap_or_default();
            if !ctype.starts_with("image/") {
                send_json!(cid, 400, "{\"error\":\"not an image\"}");
            }
            let length: Option<i64> = headers.get("content-length").and_then(|v| v.trim().parse().ok());
            let Some(clen) = length else { send_json!(cid, 400, "{\"error\":\"missing content-length\"}") };
            if clen <= 0 {
                if clen == 0 {
                    send_json!(cid, 400, "{\"error\":\"empty body\"}");
                }
                send_json!(cid, 400, "{\"error\":\"missing content-length\"}");
            }
            if clen as usize > MAX_BYTES {
                send_json!(cid, 413, "{\"error\":\"file too large\"}");
            }
            let slug = match app.get_or_create_gallery(&key) {
                Some(s) => s,
                None => send_json!(cid, 403, "{\"error\":\"gallery banned\"}"),
            };
            let raw = path["/api/photos/".len()..].to_string();
            let (sanitized, ext) = sanitize_filename(&raw);
            if ![".jpg", ".jpeg", ".png", ".gif", ".webp"].contains(&ext.as_str()) {
                send_json!(cid, 415, "{\"error\":\"unsupported type\"}");
            }
            // create file O_EXCL
            let mut created: Option<(RawFd, PathBuf)> = None;
            for _ in 0..2 {
                let stored = format!("{}_{}_{}", slug, uuid_hex(), sanitized);
                let fp = app.photos.join(&stored);
                let cstr = std::ffi::CString::new(fp.as_os_str().as_encoded_bytes()).unwrap();
                let fd = unsafe { libc::open(cstr.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_WRONLY | libc::O_CLOEXEC, 0o644) };
                if fd >= 0 {
                    created = Some((fd, fp));
                    break;
                } else {
                    let e = unsafe { *libc::__errno_location() };
                    if e == libc::EEXIST {
                        continue;
                    }
                    send_json!(cid, 500, "{\"error\":\"cannot store\"}");
                }
            }
            let Some((fd, fp)) = created else { send_json!(cid, 500, "{\"error\":\"cannot store\"}") };
            let c = conns.get_mut(&cid).unwrap();
            c.up_fd = Some(fd);
            c.up_path = fp;
            c.up_remain = clen as usize;
            // consume pipelined bytes
            if let Some(mut pb) = pipelined {
                let take = pb.len().min(c.up_remain);
                let mut off = 0;
                let mut ok = true;
                while off < take {
                    let n = unsafe { libc::write(fd, pb[off..take].as_ptr() as *const _, take - off) };
                    if n <= 0 {
                        ok = false;
                        break;
                    }
                    off += n as usize;
                }
                pb.drain(..take);
                c.up_remain -= take;
                if !ok {
                    let cc = conns.remove(&cid).unwrap();
                    close_conn(cc);
                    return;
                }
                if c.up_remain == 0 {
                    let slug2 = photo_slug_name(&c.up_path).unwrap_or_default();
                    let name = c.up_path.file_name().unwrap().to_string_lossy().to_string();
                    unsafe { libc::close(c.up_fd.take().unwrap()) };
                    let body = format!("{{\"name\":\"{}\",\"slug\":\"{}\"}}", escape(&name), escape(&slug2));
                    c.resp = json_body(201, &body);
                    c.resp_off = 0;
                    let (sfd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                    push_send(sq, cid, sfd, unsafe { std::slice::from_raw_parts(p, l) });
                    return;
                }
                // leftover pipelined beyond clen: ignore (shouldn't happen)
            }
            // wait for more Recv CQEs
            return;
        }
        Method::Post => {
            let clen: usize = headers.get("content-length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            let body = pipelined.unwrap_or_default();
            if body.len() < clen {
                // body split across recvs — not yet supported for JSON; ask client to retry in one segment
                // (curl/small JSON always arrives in one 4K buffer; pipelined covers it)
                send_json!(cid, 400, "{\"error\":\"bad request\"}");
            }
            let js: SerdeJsonLike = parse_json_obj(&body);
            if path == "/api/galleries/resolve" {
                let key = headers
                    .get("x-gallery-key")
                    .cloned()
                    .and_then(|v| normalize_key(&v))
                    .or_else(|| js.get("key").cloned().and_then(|v| normalize_key(&v)));
                let Some(key) = key else { send_json!(cid, 401, "{\"error\":\"gallery key required\"}") };
                match app.get_or_create_gallery(&key) {
                    Some(s) => send_json!(cid, 200, &format!("{{\"slug\":\"{}\"}}", escape(&s))),
                    None => send_json!(cid, 403, "{\"error\":\"gallery banned\"}"),
                }
            }
            if path == "/api/admin/unlock" {
                let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_default();
                if admin_pass.is_empty() {
                    send_json!(cid, 404, "{\"error\":\"not found\"}");
                }
                let guess = js.get("password").cloned().unwrap_or_default();
                if !const_eq(&guess, &admin_pass) {
                    send_json!(cid, 403, "{\"error\":\"forbidden\"}");
                }
                let tok = uuid_hex() + &uuid_hex();
                app.admin_tokens.insert(tok.clone());
                let c = conns.get_mut(&cid).unwrap();
                let body = "{\"ok\":true}";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nSet-Cookie: admin={}; HttpOnly; SameSite=Lax; Path=/\r\nConnection: keep-alive\r\n\r\n{}",
                    body.len(),
                    tok,
                    body
                );
                c.resp = resp.into_bytes();
                c.resp_off = 0;
                let (fd, p, l) = (c.fd, c.resp.as_ptr(), c.resp.len());
                push_send(sq, cid, fd, unsafe { std::slice::from_raw_parts(p, l) });
                return;
            }
            if path.starts_with("/api/admin/galleries/") && path.ends_with("/delete") {
                let cookie = headers.get("cookie").cloned().unwrap_or_default();
                let admin = parse_cookies(&cookie).get("admin").cloned().unwrap_or_default();
                if !app.admin_tokens.contains(&admin) {
                    send_json!(cid, 403, "{\"error\":\"forbidden\"}");
                }
                let slug = path["/api/admin/galleries/".len()..path.len() - "/delete".len()].to_string();
                if !is_slug(&slug) {
                    send_json!(cid, 404, "{\"error\":\"unknown gallery\"}");
                }
                let digest: Option<Vec<u8>> = app
                    .db
                    .query_row("SELECT key_hash FROM galleries WHERE slug = ?1", [&slug], |r| r.get::<_, Vec<u8>>(0))
                    .ok();
                let Some(digest) = digest else { send_json!(cid, 404, "{\"error\":\"unknown gallery\"}") };
                let _ = app.db.execute("DELETE FROM galleries WHERE slug = ?1", [&slug]);
                let _ = app.db.execute("INSERT OR IGNORE INTO banned_keys(key_hash, created) VALUES(?1,?2)", rusqlite::params![digest, now_secs()]);
                let mut removed = 0;
                if let Ok(rd) = std::fs::read_dir(&app.photos) {
                    for e in rd.flatten() {
                        let n = e.file_name().to_string_lossy().to_string();
                        if n.starts_with(&(slug.clone() + "_")) {
                            if std::fs::remove_file(e.path()).is_ok() {
                                removed += 1;
                            }
                        }
                    }
                }
                send_json!(cid, 200, &format!("{{\"deleted\":\"{}\",\"files\":{}}}", escape(&slug), removed));
            }
            if path == "/api/admin/unban" {
                let cookie = headers.get("cookie").cloned().unwrap_or_default();
                let admin = parse_cookies(&cookie).get("admin").cloned().unwrap_or_default();
                if !app.admin_tokens.contains(&admin) {
                    send_json!(cid, 403, "{\"error\":\"forbidden\"}");
                }
                let key = js.get("key").cloned().and_then(|v| normalize_key(&v));
                let Some(key) = key else { send_json!(cid, 400, "{\"error\":\"gallery key required\"}") };
                let n = app.db.execute("DELETE FROM banned_keys WHERE key_hash = ?1", [key_hash(&key)]).unwrap_or(0);
                if n == 0 {
                    send_json!(cid, 404, "{\"error\":\"key not banned\"}");
                }
                send_json!(cid, 200, "{\"ok\":true}");
            }
            // stale-test compat
            if path == "/api/register" || path == "/api/login" || path == "/api/logout" {
                send_json!(cid, 404, "{\"error\":\"not found\"}");
            }
            send_json!(cid, 404, "{\"error\":\"not found\"}");
        }
        Method::Delete => {
            if path.starts_with("/api/admin/galleries/") && path.ends_with("/delete") {
                // re-dispatch as POST (python does do_POST)
                return handle_request(app, conns, sq, cid, Method::Post, &path, headers, None);
            }
            if !path.starts_with("/api/photos/") {
                send_json!(cid, 404, "{\"error\":\"not found\"}");
            }
            let name = path["/api/photos/".len()..].to_string();
            if name.contains('/') || name.starts_with('.') {
                send_json!(cid, 404, "{\"error\":\"not found\"}");
            }
            let slug = match photo_slug(&name) {
                Some(s) => s.to_string(),
                None => send_json!(cid, 404, "{\"error\":\"not found\"}"),
            };
            if !app.gallery_exists(&slug) {
                send_json!(cid, 404, "{\"error\":\"not found\"}");
            }
            let cookie = headers.get("cookie").cloned().unwrap_or_default();
            let admin = parse_cookies(&cookie).get("admin").cloned().unwrap_or_default();
            if !app.admin_tokens.contains(&admin) {
                let key = headers.get("x-gallery-key").map(|s| s.as_str()).unwrap_or("");
                let key = match normalize_key(key) {
                    Some(k) => k,
                    None => send_json!(cid, 403, "{\"error\":\"forbidden\"}"),
                };
                if app.lookup_gallery(&key).as_deref() != Some(slug.as_str()) {
                    send_json!(cid, 403, "{\"error\":\"forbidden\"}");
                }
            }
            let fp = app.photos.join(&name);
            match std::fs::remove_file(&fp) {
                Ok(()) => send_json!(cid, 200, &format!("{{\"deleted\":\"{}\"}}", escape(&name))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => send_json!(cid, 404, "{\"error\":\"not found\"}"),
                Err(_) => send_json!(cid, 500, "{\"error\":\"cannot delete\"}"),
            }
        }
        Method::Other => {
            send_json!(cid, 404, "{\"error\":\"not found\"}");
        }
    }
}

fn uuid_hex() -> String {
    // 128-bit from xorshift + nanos, hex-encoded (no uuid crate to keep deps lean)
    let mut b = [0u8; 16];
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(1);
    unsafe {
        static mut S2: u64 = 0x2545_f491_4f6c_dd1d;
        S2 ^= S2 << 13;
        S2 ^= S2 >> 7;
        S2 ^= S2 << 17;
        S2 = S2.wrapping_add(t ^ libc::getpid() as u64);
        let mut x = S2;
        for i in 0..16 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x = x.wrapping_add((t >> (i % 8)) | 0x9e37);
            b[i] = (x >> 11) as u8;
        }
    }
    b.iter().map(|v| format!("{:02x}", v)).collect()
}

fn const_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}



