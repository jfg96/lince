//! Incremental updates through inotify.
//!
//! inotify is the only change feed an unprivileged process gets on Linux:
//! fanotify's filesystem-wide marks need CAP_SYS_ADMIN, and there is no
//! resumable change journal. So every indexed directory gets its own
//! watch (added before the directory is read, so nothing created during the
//! read is lost) and a queue overflow falls back to the ctime validation in
//! persist::refresh.
//!
//! Watches are a per-user kernel budget (fs.inotify.max_user_watches) shared
//! with every other program, so the watcher stops short of it. Directories it
//! cannot watch are not given up on: they are polled by ctime and watched as
//! soon as there is room again.
//!
//! Handlers are idempotent: an event describing something the index already
//! reflects is a no-op, so events and rescans can overlap safely.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;

use crate::index::{crawl, dir_identity, DirHook, Index, Kind, VROOT};
use crate::persist;

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
    /// Directories refused a watch by crawl workers, until `commit`.
    refused: Mutex<Vec<u32>>,
    /// Directories a rescan removed from the index, until `commit`.
    gone: Mutex<Vec<u32>>,
    /// Directories without a watch (over the budget, or refused by inotify).
    /// Changes in them are only seen by `retry`, so the index cannot promise
    /// freshness until this is empty again.
    unwatched: HashSet<u32>,
    /// `unwatched` in retry order; may hold ids since dropped from the set.
    retry_queue: VecDeque<u32>,
    /// Most watches this daemon takes.
    pub budget: usize,
    /// Duration of the last `retry` pass, for status.
    pub poll_ms: u64,
}

#[derive(Default, Debug)]
pub struct Applied {
    pub events: usize,
    pub overflow: bool,
}

#[derive(Default)]
struct MoveRechecks {
    subtrees: HashSet<u32>,
    replacements: HashMap<u32, HashSet<Vec<u8>>>,
}

impl DirHook for Watcher {
    fn before_read(&self, id: u32, path: &Path) {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return };
        let mut added = self.added.lock().unwrap();
        if self.wd_dir.len() + added.len() >= self.budget {
            drop(added);
            self.refused.lock().unwrap().push(id);
            return;
        }
        let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
        if wd < 0 {
            drop(added);
            self.refused.lock().unwrap().push(id);
            return;
        }
        added.push((wd, id));
    }

    fn removed(&self, dirs: Vec<u32>) {
        self.gone.lock().unwrap().extend(dirs);
    }
}

impl Watcher {
    /// `budget` defaults to 90% of fs.inotify.max_user_watches, leaving the
    /// rest to editors, file managers and the desktop.
    pub fn new(budget: Option<usize>) -> Result<Watcher, String> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(format!("inotify_init1: {}", std::io::Error::last_os_error()));
        }
        Ok(Watcher {
            fd,
            wd_dir: HashMap::new(),
            dir_wd: HashMap::new(),
            added: Mutex::new(Vec::new()),
            refused: Mutex::new(Vec::new()),
            gone: Mutex::new(Vec::new()),
            unwatched: HashSet::new(),
            retry_queue: VecDeque::new(),
            budget: budget.unwrap_or_else(default_budget),
            poll_ms: 0,
        })
    }

    pub fn watch_count(&self) -> usize {
        self.wd_dir.len()
    }

    pub fn unwatched_count(&self) -> usize {
        self.unwatched.len() + self.refused.lock().unwrap().len()
    }

    /// Every directory has a watch, so events reflect every change.
    pub fn fresh(&self) -> bool {
        self.unwatched_count() == 0
    }

    /// Tries again to watch up to `max` directories without a watch, in
    /// round-robin order, then re-reads those of them whose ctime changed:
    /// changes made while unwatched are caught either way, and the ones still
    /// unwatched are polled this way. Returns how many were checked.
    pub fn retry(&mut self, index: &mut Index, threads: usize, max: usize) -> usize {
        self.commit();
        self.refresh_roots(index, threads);
        let mut dirs = Vec::new();
        while dirs.len() < max {
            let Some(d) = self.retry_queue.pop_front() else { break };
            if self.unwatched.remove(&d) && index.is_dir(d) {
                dirs.push(d);
            }
        }
        // Parents first: re-reading one may remove or add its descendants.
        dirs.sort_unstable_by_key(|&d| (index.entries[d as usize].depth, d));
        for &d in &dirs {
            // Watch before the ctime check, so no change falls in between.
            self.before_read(d, &index.path(d));
        }
        self.commit();
        persist::refresh_dirs(index, &dirs, threads, self);
        self.commit();
        dirs.len()
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
        for id in self.refused.get_mut().unwrap().drain(..) {
            if self.unwatched.insert(id) {
                self.retry_queue.push_back(id);
            }
        }
        let gone = std::mem::take(self.gone.get_mut().unwrap());
        self.forget(&gone);
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
            } else {
                unsafe { libc::inotify_rm_watch(self.fd, wd) };
            }
        }
        let remap = |id: u32| Some(map[id as usize]).filter(|&id| id != crate::index::NONE);
        self.unwatched = self.unwatched.iter().filter_map(|&id| remap(id)).collect();
        self.retry_queue = self.retry_queue.iter().filter_map(|&id| remap(id)).filter(|id| self.unwatched.contains(id)).collect();
    }

    fn forget(&mut self, dirs: &[u32]) {
        for d in dirs {
            self.unwatched.remove(d);
            if let Some(wd) = self.dir_wd.remove(d) {
                self.wd_dir.remove(&wd);
                // A directory moved out of the tree keeps its watch alive.
                unsafe { libc::inotify_rm_watch(self.fd, wd) };
            }
        }
    }

    /// Watches follow inodes. Check the configured paths as well, since a
    /// root can disappear or be replaced without an event on a watched parent.
    fn refresh_roots(&mut self, index: &mut Index, threads: usize) {
        let mut changed = Vec::new();
        for root in &mut index.roots {
            let identity = dir_identity(&root.path);
            if identity != root.identity {
                root.identity = identity;
                changed.push(root.id);
            }
        }
        if changed.is_empty() {
            return;
        }
        for &id in &changed {
            for child in index.children(id).collect::<Vec<_>>() {
                self.forget(&index.remove(child));
            }
            self.forget(&[id]);
            index.dir_stamp.remove(&id);
        }
        for &id in &changed {
            self.before_read(id, &index.path(id));
        }
        self.commit();
        persist::refresh_dirs(index, &changed, threads, self);
        self.commit();
    }

    /// `threads` is 1 for IN_CREATE (a new directory is nearly always empty)
    /// and the full pool when a directory moves in from outside the tree.
    /// Adds what is now on disk at `name` in `dir`. False when nothing is
    /// there: it was removed or moved, or a queued event has yet to move one
    /// of its ancestors in the index to where it now is.
    #[must_use]
    fn add(&mut self, index: &mut Index, dir: u32, name: &[u8], threads: usize) -> bool {
        let path = index.path(dir).join(OsStr::from_bytes(name));
        let kind = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => Kind::Dir,
            Ok(m) if m.is_file() => Kind::File,
            Ok(_) => {
                // A rename can put a symlink or special file over an entry
                // without a separate delete event for the old destination.
                self.remove(index, dir, name);
                return true;
            }
            Err(_) => return false,
        };
        let is_dir = kind == Kind::Dir;
        if let Some(existing) = index.child_by_name(dir, name) {
            if index.entries[existing as usize].kind == kind {
                return true;
            }
            let gone = index.remove(existing);
            self.forget(&gone);
        }
        if index.is_excluded(&path, is_dir) {
            return true;
        }
        let id = index.add_child(dir, name, kind);
        if is_dir {
            crawl(index, vec![(id, path)], threads, self);
            self.commit();
        }
        true
    }

    fn remove(&mut self, index: &mut Index, dir: u32, name: &[u8]) {
        if let Some(id) = index.child_by_name(dir, name) {
            let gone = index.remove(id);
            self.forget(&gone);
        }
    }

    /// False like `add` when the destination had to be added and was gone.
    fn rename(
        &mut self,
        index: &mut Index,
        from: (u32, Vec<u8>),
        to: (u32, &[u8]),
        is_dir: bool,
        threads: usize,
        recheck: &mut MoveRechecks,
    ) -> bool {
        // Inotify reports an atomic exchange as two renames. The other
        // name can be occupied even if it was a symlink we never indexed.
        let source = if index.is_dir(from.0) { index.child_by_name(from.0, &from.1) } else { None };
        let existing = index.child_by_name(to.0, to.1);
        let replacements = &mut recheck.replacements;
        let reconcile = existing.is_some()
            || replacements.get(&from.0).is_some_and(|names| names.contains(&from.1))
            || (source.is_some()
                && std::fs::symlink_metadata(index.path(from.0).join(OsStr::from_bytes(&from.1))).is_ok());
        if reconcile {
            replacements.entry(from.0).or_default().insert(from.1.clone());
            replacements.entry(to.0).or_default().insert(to.1.to_vec());
            if is_dir || existing.is_some_and(|id| index.is_dir(id)) {
                // The old directory IDs can now describe the wrong paths.
                // Stop using their watches until both names are rechecked.
                if index.is_dir(from.0) {
                    self.remove(index, from.0, &from.1);
                }
                self.remove(index, to.0, to.1);
                return true;
            }
        }
        let Some(id) = source else {
            return self.add(index, to.0, to.1, threads);
        };
        if let Some(existing) = existing {
            if existing != id {
                let gone = index.remove(existing); // rename(2) replaced it
                self.forget(&gone);
            }
        }
        let path = index.path(to.0).join(OsStr::from_bytes(to.1));
        if index.is_excluded(&path, is_dir) {
            let gone = index.remove(id);
            self.forget(&gone);
            return true;
        }
        if is_dir {
            // If a previously moved ancestor still needs reconciliation,
            // this directory must carry that work to its own destination.
            let moved_dirs = &mut recheck.subtrees;
            let mut ancestor = from.0;
            while !moved_dirs.is_empty() && ancestor != VROOT && !moved_dirs.contains(&ancestor) {
                ancestor = index.entries[ancestor as usize].parent;
            }
            if moved_dirs.contains(&ancestor)
                || index.has_excludes_below(&path)
                || index.has_excludes_below(&index.path(id))
            {
                moved_dirs.insert(id);
            }
        }
        index.rename(id, to.0, to.1);
        true
    }

    /// Reads and applies every queued event. Returns what happened.
    pub fn drain(&mut self, index: &mut Index, threads: usize) -> Applied {
        self.refresh_roots(index, threads);
        let mut applied = Applied::default();
        // A reconciliation can add watches while the file system is active.
        // Drain those events too before answering a query.
        while self.drain_batch(index, threads, &mut applied) {}
        applied
    }

    fn drain_batch(&mut self, index: &mut Index, threads: usize, applied: &mut Applied) -> bool {
        let mut buf = vec![0u8; 64 * 1024];
        let mut moved_from: HashMap<u32, (u32, Vec<u8>)> = HashMap::new();
        let mut recheck = MoveRechecks::default();
        // Adds that found nothing on disk. If an ancestor was renamed after
        // the entry appeared, the rename's events were queued before the
        // failed stat, so this drain applies them; retrying at the end looks
        // in the right place.
        let mut missing: Vec<(u32, Vec<u8>)> = Vec::new();
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
                        if self.unwatched.insert(id) {
                            self.retry_queue.push_back(id);
                        }
                    }
                    continue;
                }
                let Some(&dir) = self.wd_dir.get(&ev.wd) else { continue };
                if index.entries[dir as usize].kind != Kind::Dir {
                    continue;
                }
                let is_dir = ev.mask & libc::IN_ISDIR != 0;
                if ev.mask & libc::IN_CREATE != 0 {
                    if !self.add(index, dir, name, 1) {
                        missing.push((dir, name.to_vec()));
                    }
                } else if ev.mask & libc::IN_DELETE != 0 {
                    self.remove(index, dir, name);
                } else if ev.mask & libc::IN_MOVED_FROM != 0 {
                    moved_from.insert(ev.cookie, (dir, name.to_vec()));
                } else if ev.mask & libc::IN_MOVED_TO != 0 {
                    let found = match moved_from.remove(&ev.cookie) {
                        Some(from) => self.rename(index, from, (dir, name), is_dir, threads, &mut recheck),
                        None => self.add(index, dir, name, threads),
                    };
                    if !found {
                        missing.push((dir, name.to_vec()));
                    }
                }
            }
        }
        // A move whose destination is outside the tree (or not yet read)
        // becomes a removal; if the MOVED_TO shows up later it is an add,
        // which rescans the subtree, so the end state is still right.
        // The removal runs after every later event, and one of them may have
        // put something else at that name (moved to the trash, then created
        // again): what is on disk now is added back, and watched.
        for (_, (dir, name)) in moved_from {
            if index.entries[dir as usize].kind == Kind::Dir {
                self.remove(index, dir, &name);
                let _ = self.add(index, dir, &name, threads);
            }
        }
        for (dir, name) in missing {
            if index.entries[dir as usize].kind == Kind::Dir {
                // Still nothing there: it is gone, and its own events say so.
                let _ = self.add(index, dir, &name, threads);
            }
        }
        let MoveRechecks { subtrees, replacements } = recheck;
        let repaired = !replacements.is_empty();
        if repaired {
            self.reconcile_replacements(index, replacements, threads);
        }
        if subtrees.is_empty() {
            return repaired;
        }
        // Read the final locations once the whole sequence of moves has
        // been applied. Even unchanged ctimes need new exclusion checks.
        let mut stack: Vec<u32> = subtrees.into_iter().collect();
        let mut seen = HashSet::new();
        let mut dirs = Vec::new();
        while let Some(dir) = stack.pop() {
            if index.is_dir(dir) && seen.insert(dir) {
                index.dir_stamp.remove(&dir);
                dirs.push(dir);
                stack.extend(index.children(dir).filter(|&child| index.is_dir(child)));
            }
        }
        dirs.sort_unstable_by_key(|&dir| (index.entries[dir as usize].depth, dir));
        persist::refresh_dirs(index, &dirs, threads, self);
        self.commit();
        true
    }

    fn reconcile_replacements(&mut self, index: &mut Index, replaced: HashMap<u32, HashSet<Vec<u8>>>, threads: usize) {
        let mut parents: Vec<_> = replaced.into_iter().collect();
        parents.sort_unstable_by_key(|&(dir, _)| (index.entries[dir as usize].depth, dir));
        // Release the old directory watches before installing any new ones:
        // a watched inode may now belong to a different name.
        for (dir, names) in &parents {
            if index.is_dir(*dir) {
                for name in names {
                    if index.child_by_name(*dir, name).is_some_and(|id| index.is_dir(id)) {
                        self.remove(index, *dir, name);
                    }
                }
            }
        }
        for (dir, names) in parents {
            if index.is_dir(dir) {
                for name in &names {
                    if !self.add(index, dir, name, threads) {
                        self.remove(index, dir, name);
                    }
                }
            }
        }
    }
}

fn default_budget() -> usize {
    let limit = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(8192);
    limit - limit / 10
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build;
    use crate::index::tests::tempdir;

    fn lookup(index: &Index, rel: &str) -> Option<u32> {
        rel.split('/').try_fold(index.roots[0].id, |dir, part| index.child_by_name(dir, part.as_bytes()))
    }

    fn exchange(a: &Path, b: &Path) {
        let a = CString::new(a.as_os_str().as_bytes()).unwrap();
        let b = CString::new(b.as_os_str().as_bytes()).unwrap();
        let rc = unsafe {
            libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_EXCHANGE)
        };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    }

    #[test]
    fn exchanges_directories_and_keeps_the_new_paths_watched() {
        let dir = tempdir::Dir::new();
        let root = &dir.0;
        for name in ["a", "b"] {
            std::fs::create_dir_all(root.join(name).join("sub")).unwrap();
            std::fs::write(root.join(name).join(format!("{name}.txt")), b"").unwrap();
        }
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(std::slice::from_ref(root), &[], 2, &w).unwrap();
        w.commit();

        exchange(&root.join("a"), &root.join("b"));
        w.drain(&mut index, 2);
        assert!(lookup(&index, "a/b.txt").is_some());
        assert!(lookup(&index, "b/a.txt").is_some());
        assert!(lookup(&index, "a/a.txt").is_none());
        assert!(lookup(&index, "b/b.txt").is_none());
        assert_eq!(w.watch_count(), 5);
        assert!(w.fresh());
        std::fs::write(root.join("b/sub/new.txt"), b"").unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "b/sub/new.txt").is_some());

        // A further move in the same batch can use a watch from one of the
        // exchanged trees. It must be applied against the final layout.
        exchange(&root.join("a"), &root.join("b"));
        std::fs::rename(root.join("a"), root.join("b/sub/relocated")).unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "a").is_none());
        assert!(lookup(&index, "b/b.txt").is_some());
        assert!(lookup(&index, "b/sub/relocated/a.txt").is_some());
        assert!(lookup(&index, "b/sub/relocated/sub/new.txt").is_some());
        assert_eq!(w.watch_count(), 5);
        assert!(w.fresh());
    }

    #[test]
    fn replacing_a_file_with_a_symlink_removes_the_file_entry() {
        let dir = tempdir::Dir::new();
        let root = &dir.0;
        std::fs::write(root.join("original"), b"").unwrap();
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(std::slice::from_ref(root), &[], 1, &w).unwrap();
        w.commit();

        std::os::unix::fs::symlink("elsewhere", root.join("link")).unwrap();
        std::fs::rename(root.join("link"), root.join("original")).unwrap();
        w.drain(&mut index, 1);
        assert!(lookup(&index, "original").is_none());
        assert!(w.fresh());

        std::fs::write(root.join("file"), b"").unwrap();
        w.drain(&mut index, 1);
        exchange(&root.join("file"), &root.join("original"));
        w.drain(&mut index, 1);
        assert!(lookup(&index, "original").is_some());
        assert!(lookup(&index, "file").is_none());
    }

    #[test]
    fn a_replaced_root_is_indexed_and_watched_again() {
        let dir = tempdir::Dir::new();
        let parent = dir.0.join("parent");
        let root = parent.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/old.txt"), b"").unwrap();
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(std::slice::from_ref(&root), &[], 2, &w).unwrap();
        w.commit();

        // Even moving an unwatched ancestor invalidates the root's path.
        std::fs::rename(&parent, dir.0.join("moved")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/new.txt"), b"").unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "sub/old.txt").is_none());
        assert!(lookup(&index, "sub/new.txt").is_some());
        assert!(w.fresh());
        assert_eq!(w.watch_count(), 2);

        std::fs::rename(&root, dir.0.join("gone")).unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "sub").is_none());
        assert!(!w.fresh());
        assert_eq!(w.watch_count(), 0);

        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("back.txt"), b"").unwrap();
        w.retry(&mut index, 2, 1);
        assert!(lookup(&index, "back.txt").is_some());
        assert!(w.fresh());
        std::fs::write(root.join("later.txt"), b"").unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "later.txt").is_some());
    }

    #[test]
    fn moving_a_subtree_applies_its_new_path_exclusions() {
        let dir = tempdir::Dir::new();
        let root = &dir.0;
        std::fs::create_dir_all(root.join("job/private")).unwrap();
        std::fs::create_dir(root.join("inbox")).unwrap();
        std::fs::write(root.join("job/private/notes.txt"), b"").unwrap();
        std::fs::write(root.join("job/secret.log"), b"").unwrap();
        let excludes = ["/inbox/job/private/".into(), "inbox/job/secret.log".into()];
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(std::slice::from_ref(root), &excludes, 2, &w).unwrap();
        w.commit();

        std::fs::rename(root.join("job"), root.join("inbox/job")).unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "inbox/job").is_some());
        assert!(lookup(&index, "inbox/job/private").is_none());
        assert!(lookup(&index, "inbox/job/secret.log").is_none());
        assert_eq!(w.watch_count(), 3);

        std::fs::rename(root.join("inbox/job"), root.join("job")).unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "job/private/notes.txt").is_some());
        assert!(lookup(&index, "job/secret.log").is_some());
        assert_eq!(w.watch_count(), 4);
        std::fs::write(root.join("job/private/later.txt"), b"").unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "job/private/later.txt").is_some());
        assert!(w.fresh());

        // A child can leave its moved parent before reconciliation. Its
        // missing entries still need to be looked up at the final location.
        std::fs::rename(root.join("job"), root.join("inbox/job")).unwrap();
        w.drain(&mut index, 2);
        std::fs::rename(root.join("inbox"), root.join("archive")).unwrap();
        std::fs::rename(root.join("archive/job"), root.join("job")).unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "job/private/later.txt").is_some());
        assert!(lookup(&index, "job/secret.log").is_some());
    }

    #[test]
    fn applies_changes_made_after_the_crawl() {
        let dir = tempdir::Dir::new();
        std::fs::create_dir_all(dir.0.join("a/sub")).unwrap();
        std::fs::write(dir.0.join("a/sub/old.txt"), b"").unwrap();
        std::fs::write(dir.0.join("gone.txt"), b"").unwrap();
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(&[dir.0.clone()], &[], 2, &w).unwrap();
        w.commit();

        std::fs::write(dir.0.join("a/new.txt"), b"").unwrap();
        w.drain(&mut index, 2);
        // A new directory tree: its contents arrive before it is watched.
        std::fs::create_dir_all(dir.0.join("b/c")).unwrap();
        std::fs::write(dir.0.join("b/c/deep.txt"), b"").unwrap();
        std::fs::rename(dir.0.join("a"), dir.0.join("z")).unwrap();
        std::fs::remove_file(dir.0.join("gone.txt")).unwrap();
        let applied = w.drain(&mut index, 2);

        assert!(applied.events == 4 && !applied.overflow);
        assert!(lookup(&index, "z/new.txt").is_some());
        assert!(lookup(&index, "z/sub/old.txt").is_some());
        assert!(lookup(&index, "a").is_none());
        assert!(lookup(&index, "b/c/deep.txt").is_some());
        assert!(lookup(&index, "gone.txt").is_none());
        // The moved subtree keeps its watch under the new name.
        std::fs::write(dir.0.join("z/sub/later.txt"), b"").unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "z/sub/later.txt").is_some());
        assert!(w.fresh());
    }

    #[test]
    fn polls_and_recovers_directories_over_the_budget() {
        let dir = tempdir::Dir::new();
        for d in ["a", "b", "c/d"] {
            std::fs::create_dir_all(dir.0.join(d)).unwrap();
        }
        // Room for the root and one more: the other directories go unwatched.
        let mut w = Watcher::new(Some(2)).unwrap();
        let mut index = build(&[dir.0.clone()], &[], 1, &w).unwrap();
        w.commit();
        assert_eq!(w.watch_count(), 2);
        assert_eq!(w.unwatched_count(), 3);
        assert!(!w.fresh());

        // No events from unwatched directories: polling has to find these.
        for d in ["a", "b", "c/d"] {
            std::fs::write(dir.0.join(d).join("new.txt"), b"").unwrap();
        }
        std::fs::create_dir(dir.0.join("c/d/sub")).unwrap();
        std::fs::write(dir.0.join("c/d/sub/deep.txt"), b"").unwrap();
        w.drain(&mut index, 1);
        w.retry(&mut index, 1, usize::MAX);
        for rel in ["a/new.txt", "b/new.txt", "c/d/new.txt", "c/d/sub/deep.txt"] {
            assert!(lookup(&index, rel).is_some(), "{rel} found by polling");
        }
        assert!(!w.fresh());

        // Same-tick changes are not lost to coarse ctimes: every poll keeps
        // finding what changed since the previous one. Which of "a" and "b"
        // got the spare watch depends on the file system's listing order.
        let polled = ["a", "b"].into_iter().find(|d| w.unwatched.contains(&lookup(&index, d).unwrap())).unwrap();
        for i in 0..20 {
            std::fs::write(dir.0.join(format!("{polled}/quick{i}.txt")), b"").unwrap();
            w.retry(&mut index, 1, usize::MAX);
            assert!(lookup(&index, &format!("{polled}/quick{i}.txt")).is_some(), "quick{i}");
        }
        // One directory per pass: still unwatched ones go to the back, so
        // passes take turns instead of retrying the same one forever.
        for d in ["a", "b", "c/d"] {
            std::fs::write(dir.0.join(d).join("turns.txt"), b"").unwrap();
        }
        w.drain(&mut index, 1);
        for _ in 0..w.unwatched_count() {
            w.retry(&mut index, 1, 1);
        }
        for d in ["a", "b", "c/d"] {
            assert!(lookup(&index, &format!("{d}/turns.txt")).is_some(), "{d} polled in turn");
        }
        std::fs::remove_dir_all(dir.0.join("c/d")).unwrap();
        w.drain(&mut index, 1);
        w.retry(&mut index, 1, usize::MAX);
        assert!(lookup(&index, "c/d").is_none());

        // Room again: everything gets watched and events take over.
        w.budget = 100;
        w.retry(&mut index, 1, usize::MAX);
        assert!(w.fresh(), "{} still unwatched", w.unwatched_count());
        std::fs::write(dir.0.join("a/later.txt"), b"").unwrap();
        std::fs::create_dir(dir.0.join("b/fresh")).unwrap();
        w.drain(&mut index, 1);
        assert!(lookup(&index, "a/later.txt").is_some());
        assert!(lookup(&index, "b/fresh").is_some());
    }

    #[test]
    fn a_rescan_releases_watches_of_directories_it_removes() {
        let dir = tempdir::Dir::new();
        let tree = dir.0.join("tree");
        std::fs::create_dir_all(tree.join("a/sub")).unwrap();
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(&[tree.clone()], &[], 1, &w).unwrap();
        w.commit();
        assert_eq!(w.watch_count(), 3);

        // Out of the tree, so inotify would keep watching it; a lost event
        // leaves the rescan as the only one to notice.
        std::fs::rename(tree.join("a"), dir.0.join("moved")).unwrap();
        persist::refresh(&mut index, 1, &w);
        w.commit();
        assert!(lookup(&index, "a").is_none());
        assert_eq!(w.watch_count(), 1);
    }

    #[test]
    fn keeps_what_replaces_an_entry_moved_out_of_the_tree() {
        let dir = tempdir::Dir::new();
        let tree = dir.0.join("tree");
        std::fs::create_dir_all(tree.join("d")).unwrap();
        std::fs::write(tree.join("a.txt"), b"").unwrap();
        std::fs::write(tree.join("d/old.txt"), b"").unwrap();
        std::fs::write(tree.join("left.txt"), b"").unwrap();
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(&[tree.clone()], &[], 1, &w).unwrap();
        w.commit();

        // All queued before one drain: each MOVED_FROM has no MOVED_TO, and
        // the name is taken again before the removal is applied.
        std::fs::rename(tree.join("a.txt"), dir.0.join("a.txt")).unwrap();
        std::fs::write(tree.join("a.txt"), b"").unwrap();
        std::fs::rename(tree.join("d"), dir.0.join("d")).unwrap();
        std::fs::create_dir(tree.join("d")).unwrap();
        std::fs::write(tree.join("d/new.txt"), b"").unwrap();
        std::fs::rename(tree.join("left.txt"), dir.0.join("left.txt")).unwrap();
        w.drain(&mut index, 1);

        assert!(lookup(&index, "a.txt").is_some());
        assert!(lookup(&index, "d/new.txt").is_some());
        assert!(lookup(&index, "d/old.txt").is_none(), "the directory that left took its contents");
        assert!(lookup(&index, "left.txt").is_none());
        assert_eq!(w.watch_count(), 2, "the old directory's watch is released");
        std::fs::write(tree.join("d/later.txt"), b"").unwrap();
        w.drain(&mut index, 1);
        assert!(lookup(&index, "d/later.txt").is_some(), "the new directory is watched");
    }

    #[test]
    fn keeps_entries_created_just_before_a_parent_rename() {
        let dir = tempdir::Dir::new();
        std::fs::create_dir_all(dir.0.join("a")).unwrap();
        let mut w = Watcher::new(None).unwrap();
        let mut index = build(&[dir.0.clone()], &[], 2, &w).unwrap();
        w.commit();

        // All queued before one drain: the creations are applied while the
        // index still calls the directory "a".
        std::fs::write(dir.0.join("a/new.txt"), b"").unwrap();
        std::fs::create_dir_all(dir.0.join("a/sub")).unwrap();
        std::fs::write(dir.0.join("a/sub/inner.txt"), b"").unwrap();
        std::fs::rename(dir.0.join("a"), dir.0.join("z")).unwrap();
        w.drain(&mut index, 2);

        assert!(lookup(&index, "z/new.txt").is_some());
        assert!(lookup(&index, "z/sub/inner.txt").is_some());
        std::fs::write(dir.0.join("z/sub/later.txt"), b"").unwrap();
        w.drain(&mut index, 2);
        assert!(lookup(&index, "z/sub/later.txt").is_some(), "the new directory is watched");
    }
}
