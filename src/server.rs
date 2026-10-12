//! Unix-socket server: one JSON request per line in, one JSON response per
//! line out. Single-threaded event loop over the listener, clients, inotify
//! and a signalfd; queries are a few milliseconds, so they run inline.
//!
//! Pending inotify events are applied before every query, so a search always
//! reflects changes that completed before it was sent. When a client queues
//! several requests (fast typing), only the newest is answered; the others
//! get {"v":1,"id":N,"superseded":true}.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::index::{self, Index, NoHook};
use crate::query::{self, Request};
use crate::watch::Watcher;
use crate::{persist, threads};

struct Client {
    stream: UnixStream,
    buf: Vec<u8>,
}

fn log(msg: &str) {
    eprintln!("lince: {msg}");
}

fn signal_fd() -> i32 {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC)
    }
}

fn already_running(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Takes the lock that makes this the only daemon on `socket`, held until
/// exit. Without it, two daemons started together would both index and the
/// second would replace the first one's socket. None if another holds it.
fn lock(socket: &Path) -> Result<Option<std::fs::File>, String> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let path = socket.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(Some(file))
}

fn bind(socket: &Path) -> Result<UnixListener, String> {
    // Holding the lock, any socket left here belongs to a daemon that died.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(listener)
}

fn open_index(cfg: &Config, watcher: &mut Option<Watcher>) -> Result<Index, String> {
    let t = Instant::now();
    let hook: &dyn index::DirHook = match watcher { Some(w) => w, None => &NoHook };
    {
        match persist::load(&cfg.snapshot, &cfg.roots, &cfg.excludes) {
            Ok(mut idx) => {
                let loaded_ms = t.elapsed().as_millis();
                if let Some(w) = watcher.as_mut() {
                    w.watch_all(&idx);
                }
                let hook: &dyn index::DirHook = match watcher { Some(w) => w, None => &NoHook };
                let changed = persist::refresh(&mut idx, threads(), hook);
                if let Some(w) = watcher.as_mut() {
                    w.commit();
                }
                log(&format!(
                    "snapshot: {} entries loaded in {loaded_ms} ms, {changed} dirs re-read, ready in {} ms",
                    idx.live,
                    t.elapsed().as_millis()
                ));
                return Ok(idx);
            }
            Err(e) => log(&format!("no usable snapshot ({e}), crawling")),
        }
    }
    let idx = index::build(&cfg.roots, &cfg.excludes, threads(), hook)?;
    if let Some(w) = watcher.as_mut() {
        w.commit();
    }
    log(&format!("crawl: {} entries in {} ms", idx.live, t.elapsed().as_millis()));
    Ok(idx)
}

fn status_json(index: &Index, watcher: &Option<Watcher>) -> String {
    let rss: u64 = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0);
    let degraded = watcher.as_ref().is_none_or(|w| *w.degraded.lock().unwrap());
    serde_json::json!({
        "v": query::PROTOCOL,
        "version": env!("CARGO_PKG_VERSION"),
        "roots": index.roots.iter().map(|r| r.path.to_string_lossy()).collect::<Vec<_>>(),
        "exclude": index.exclude_patterns,
        "entries": index.live - index.roots.len(),
        "dead": index.dead,
        "index_bytes": index.memory_bytes(),
        "rss_kib": rss,
        "watches": watcher.as_ref().map_or(0, |w| w.watch_count()),
        "fresh": !degraded,
        "generation": index.generation,
    })
    .to_string()
}

pub fn serve(cfg: Config) -> Result<(), String> {
    let Some(_lock) = lock(&cfg.socket)? else {
        log(&format!("already running on {}", cfg.socket.display()));
        return Ok(());
    };
    // A daemon from before the lock existed answers without holding it.
    if already_running(&cfg.socket) {
        log(&format!("already running on {}", cfg.socket.display()));
        return Ok(());
    }
    log(&cfg.describe());
    let sfd = signal_fd();
    let mut watcher = if cfg.watch { Some(Watcher::new()?) } else { None };
    let mut index = open_index(&cfg, &mut watcher)?;
    // Bind only once the index can answer: until then clients cannot
    // connect and keep using fd, instead of waiting on a building index.
    let listener = bind(&cfg.socket)?;
    let mut clients: Vec<Client> = Vec::new();
    let mut saved_generation = index.generation;
    let mut last_save = Instant::now();
    let mut dirty_heap = false;

    let save = |index: &Index| {
        if let Err(e) = persist::save(index, &cfg.snapshot) {
            log(&format!("snapshot save failed: {e}"));
        }
    };

    loop {
        // With --exit-with-parent the launcher keeps our stdin open; EOF means
        // it is gone. (PR_SET_PDEATHSIG is tied to the forking thread, and
        // Quickshell forks from a thread that exits right away.)
        let stdin_fd = if cfg.exit_with_parent { 0 } else { -1 };
        let mut fds = vec![
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: sfd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: watcher.as_ref().map_or(-1, |w| w.fd), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stdin_fd, events: libc::POLLIN, revents: 0 },
        ];
        fds.extend(clients.iter().map(|c| libc::pollfd { fd: c.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 }));
        // After a burst of queries, wait a few seconds and hand freed query
        // buffers back to the system instead of keeping the peak resident.
        let timeout = if dirty_heap { 5_000 } else { 60_000 };
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if rc == 0 && dirty_heap {
            #[cfg(target_env = "gnu")]
            unsafe {
                libc::malloc_trim(0);
            }
            dirty_heap = false;
        }
        if rc < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("poll: {}", std::io::Error::last_os_error()));
        }

        let parent_gone = fds[3].revents != 0 && {
            let mut b = [0u8; 64];
            unsafe { libc::read(0, b.as_mut_ptr().cast(), b.len()) <= 0 }
        };
        if fds[1].revents != 0 || parent_gone {
            save(&index);
            let _ = std::fs::remove_file(&cfg.socket);
            log("stopped");
            return Ok(());
        }
        if fds[2].revents != 0 {
            apply_events(&mut index, &mut watcher);
        }
        if fds[0].revents != 0 {
            while let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                clients.push(Client { stream, buf: Vec::new() });
            }
        }

        let mut closed = Vec::new();
        for (i, pfd) in fds[4..].iter().enumerate() {
            if pfd.revents == 0 {
                continue;
            }
            let client = &mut clients[i];
            let mut chunk = [0u8; 16384];
            loop {
                match client.stream.read(&mut chunk) {
                    Ok(0) => {
                        closed.push(i);
                        break;
                    }
                    Ok(n) => client.buf.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        closed.push(i);
                        break;
                    }
                }
            }
            let Some(last_nl) = client.buf.iter().rposition(|&b| b == b'\n') else { continue };
            let pending: Vec<u8> = client.buf.drain(..=last_nl).collect();
            let lines: Vec<&[u8]> = pending.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
            let mut out = String::new();
            dirty_heap = true;
            for (n, line) in lines.iter().enumerate() {
                let newest = n + 1 == lines.len();
                out.push_str(&handle_line(line, newest, &mut index, &mut watcher));
                out.push('\n');
            }
            let _ = client.stream.set_nonblocking(false);
            if client.stream.write_all(out.as_bytes()).is_err() {
                closed.push(i);
            }
            let _ = client.stream.set_nonblocking(true);
        }
        closed.sort_unstable();
        closed.dedup();
        for i in closed.into_iter().rev() {
            clients.remove(i);
        }

        if index.dead > 10_000 && index.dead > index.live / 4 {
            let map = index.compact();
            if let Some(w) = watcher.as_mut() {
                w.remap(&map);
            }
        }
        if index.generation != saved_generation && last_save.elapsed() > Duration::from_secs(600) {
            save(&index);
            saved_generation = index.generation;
            last_save = Instant::now();
        }
    }
}

fn apply_events(index: &mut Index, watcher: &mut Option<Watcher>) {
    let Some(w) = watcher.as_mut() else { return };
    let applied = w.drain(index, threads());
    if applied.overflow {
        // Events were lost: reconcile against the disk, like a journal rescan.
        let t = Instant::now();
        let changed = persist::refresh(index, threads(), &*w);
        w.commit();
        log(&format!("inotify overflow: {changed} dirs re-read in {} ms", t.elapsed().as_millis()));
    }
}

fn handle_line(line: &[u8], newest: bool, index: &mut Index, watcher: &mut Option<Watcher>) -> String {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return format!("{{\"v\":{},\"error\":{}}}", query::PROTOCOL, serde_json::to_string(&e.to_string()).unwrap()),
    };
    if !newest {
        return format!("{{\"v\":{},\"id\":{},\"superseded\":true}}", query::PROTOCOL, req.id);
    }
    apply_events(index, watcher);
    match req.op.as_deref() {
        None | Some("search") => {}
        Some("status") => return status_json(index, watcher),
        Some(other) => {
            return format!("{{\"v\":{},\"id\":{},\"error\":{}}}", query::PROTOCOL, req.id, serde_json::to_string(&format!("unknown op {other}")).unwrap())
        }
    }
    match query::search(index, &req) {
        Ok(mut resp) => {
            resp.fresh = watcher.as_ref().is_some_and(|w| !*w.degraded.lock().unwrap());
            serde_json::to_string(&resp).unwrap()
        }
        Err(e) => format!("{{\"v\":{},\"id\":{},\"error\":{}}}", query::PROTOCOL, req.id, serde_json::to_string(&e).unwrap()),
    }
}

/// Line-oriented client for scripts and benchmarks.
pub fn client(socket: &Path) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        stream.write_all(line.as_bytes()).and_then(|_| stream.write_all(b"\n")).map_err(|e| e.to_string())?;
        let mut resp = String::new();
        reader.read_line(&mut resp).map_err(|e| e.to_string())?;
        print!("{resp}");
    }
    Ok(())
}

/// One request, one response line.
pub fn ask(socket: &Path, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("{}: {e} (is `lince serve` running?)", socket.display()))?;
    let mut line = request.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp).map_err(|e| e.to_string())?;
    serde_json::from_str(&resp).map_err(|e| e.to_string())
}
