//! Incremental updates through inotify.
//!
//! inotify is the only change feed an unprivileged process gets on Linux:
//! fanotify's filesystem-wide marks need CAP_SYS_ADMIN, and there is no
//! resumable change journal. So every indexed directory gets its own
//! watch (added before the directory is read, so nothing created during the
//! read is lost) and a queue overflow falls back to the ctime validation in
//! persist::refresh.
//!
//! Handlers are idempotent: an event describing something the index already
//! reflects is a no-op, so events and rescans can overlap safely.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;

use crate::index::{crawl, DirHook, Index, Kind};

const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW
    | libc::IN_EXCL_UNLINK;

pub struct Watcher {
    pub fd: i32,
    wd_dir: HashMap<i32, u32>,
    dir_wd: HashMap<u32, i32>,
    added: Mutex<Vec<(i32, u32)>>,
    /// Set when inotify refused a watch (usually max_user_watches): the index
    /// can no longer promise freshness and clients should know.
    pub degraded: Mutex<bool>,
}

#[derive(Default, Debug)]
pub struct Applied {
    pub events: usize,
    pub overflow: bool,
}

impl DirHook for Watcher {
    fn before_read(&self, id: u32, path: &Path) {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return };
        let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
        if wd < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOSPC) {
                *self.degraded.lock().unwrap() = true;
            }
            return;
        }
        self.added.lock().unwrap().push((wd, id));
    }
}

impl Watcher {
    pub fn new() -> Result<Watcher, String> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(format!("inotify_init1: {}", std::io::Error::last_os_error()));
        }
        Ok(Watcher {
            fd,
            wd_dir: HashMap::new(),
            dir_wd: HashMap::new(),
            added: Mutex::new(Vec::new()),
            degraded: Mutex::new(false),
        })
    }

    pub fn watch_count(&self) -> usize {
        self.wd_dir.len()
    }

    /// Moves watches registered by crawl workers into the lookup tables.
    pub fn commit(&mut self) {
        for (wd, id) in self.added.get_mut().unwrap().drain(..) {
            if let Some(old) = self.wd_dir.insert(wd, id) {
                if old != id {
                    self.dir_wd.remove(&old);
                }
            }
            self.dir_wd.insert(id, wd);
        }
    }

    /// Watches every directory already in the index (after loading a snapshot).
    pub fn watch_all(&mut self, index: &Index) {
        for dir in index.live_dirs().collect::<Vec<_>>() {
            self.before_read(dir, &index.path(dir));
        }
        self.commit();
    }

    pub fn remap(&mut self, map: &[u32]) {
        let old: Vec<(i32, u32)> = self.wd_dir.drain().collect();
        self.dir_wd.clear();
        for (wd, id) in old {
            let new = map[id as usize];
            if new != crate::index::NONE {
                self.wd_dir.insert(wd, new);
                self.dir_wd.insert(new, wd);
            }
        }
    }

    fn forget(&mut self, dirs: &[u32]) {
        for d in dirs {
            if let Some(wd) = self.dir_wd.remove(d) {
                self.wd_dir.remove(&wd);
                // A directory moved out of the tree keeps its watch alive.
                unsafe { libc::inotify_rm_watch(self.fd, wd) };
            }
        }
    }

    /// `threads` is 1 for IN_CREATE (a new directory is nearly always empty)
    /// and the full pool when a directory moves in from outside the tree.
    fn add(&mut self, index: &mut Index, dir: u32, name: &[u8], is_dir: bool, threads: usize) {
        let path = index.path(dir).join(OsStr::from_bytes(name));
        let kind = if is_dir {
            Kind::Dir
        } else {
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_file() => Kind::File,
                _ => return, // symlink, fifo… or already gone
            }
        };
        if let Some(existing) = index.child_by_name(dir, name) {
            if index.entries[existing as usize].kind == kind {
                return;
            }
            let gone = index.remove(existing);
            self.forget(&gone);
        }
        if index.is_excluded(&path, is_dir) {
            return;
        }
        let id = index.add_child(dir, name, kind);
        if kind == Kind::Dir {
            crawl(index, vec![(id, path)], threads, self);
            self.commit();
        }
    }

    fn remove(&mut self, index: &mut Index, dir: u32, name: &[u8]) {
        if let Some(id) = index.child_by_name(dir, name) {
            let gone = index.remove(id);
            self.forget(&gone);
        }
    }

    fn rename(&mut self, index: &mut Index, from: (u32, Vec<u8>), to: (u32, &[u8]), is_dir: bool, threads: usize) {
        let Some(id) = index.child_by_name(from.0, &from.1) else {
            return self.add(index, to.0, to.1, is_dir, threads);
        };
        if let Some(existing) = index.child_by_name(to.0, to.1) {
            if existing != id {
                let gone = index.remove(existing); // rename(2) replaced it
                self.forget(&gone);
            }
        }
        let path = index.path(to.0).join(OsStr::from_bytes(to.1));
        if index.is_excluded(&path, is_dir) {
            let gone = index.remove(id);
            self.forget(&gone);
            return;
        }
        index.rename(id, to.0, to.1);
    }

    /// Reads and applies every queued event. Returns what happened.
    pub fn drain(&mut self, index: &mut Index, threads: usize) -> Applied {
        let mut applied = Applied::default();
        let mut buf = vec![0u8; 64 * 1024];
        let mut moved_from: HashMap<u32, (u32, Vec<u8>, bool)> = HashMap::new();
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut off = 0usize;
            while off < n as usize {
                let ev: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf[off..].as_ptr().cast()) };
                let name_start = off + std::mem::size_of::<libc::inotify_event>();
                let raw = &buf[name_start..name_start + ev.len as usize];
                let name = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
                off = name_start + ev.len as usize;
                applied.events += 1;
                if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                    applied.overflow = true;
                    continue;
                }
                if ev.mask & libc::IN_IGNORED != 0 {
                    if let Some(id) = self.wd_dir.remove(&ev.wd) {
                        self.dir_wd.remove(&id);
                    }
                    continue;
                }
                let Some(&dir) = self.wd_dir.get(&ev.wd) else { continue };
                if index.entries[dir as usize].kind != Kind::Dir {
                    continue;
                }
                let is_dir = ev.mask & libc::IN_ISDIR != 0;
                if ev.mask & libc::IN_CREATE != 0 {
                    self.add(index, dir, name, is_dir, 1);
                } else if ev.mask & libc::IN_DELETE != 0 {
                    self.remove(index, dir, name);
                } else if ev.mask & libc::IN_MOVED_FROM != 0 {
                    moved_from.insert(ev.cookie, (dir, name.to_vec(), is_dir));
                } else if ev.mask & libc::IN_MOVED_TO != 0 {
                    match moved_from.remove(&ev.cookie) {
                        Some((from_dir, from_name, _)) => {
                            self.rename(index, (from_dir, from_name), (dir, name), is_dir, threads)
                        }
                        None => self.add(index, dir, name, is_dir, threads),
                    }
                }
            }
        }
        // A move whose destination is outside the tree (or not yet read)
        // becomes a removal; if the MOVED_TO shows up later it is an add,
        // which rescans the subtree, so the end state is still right.
        for (_, (dir, name, _)) in moved_from {
            if index.entries[dir as usize].kind == Kind::Dir {
                self.remove(index, dir, &name);
            }
        }
        applied
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}
