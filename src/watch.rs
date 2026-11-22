//! File watching for `run --watch`, `build --watch` and `test --watch`.
//!
//! One portable API, two native backends, no polling of file mtimes:
//!
//! - Linux: inotify, exactly the pre-port semantics — one fd, per-file and
//!   per-directory watches, non-blocking batch reads.
//! - macOS: kqueue `EVFILT_VNODE` — one kqueue fd, one vnode watch per path.
//!   kqueue reports events on the watched handle but not *which* entry in a
//!   directory changed, so the mac backend watches the files themselves and
//!   self-heals: an atomic save (delete+rename to the same path) fires
//!   NOTE_DELETE/NOTE_RENAME on the old handle, the path is re-registered
//!   against the new handle immediately, and editing continues to be caught.
//!
//! The watcher fd integrates with libc::poll alongside the client socket in
//! both backends, and reads are non-blocking: a post-run drain must return
//! immediately when nothing is pending, otherwise the next real edit would be
//! swallowed.

use std::collections::{HashMap, HashSet};

/// Watch triggers: writes, mtime/attribute bumps, creates, deletes, renames.
#[cfg(target_os = "linux")]
const INOTIFY_WATCH_MASK: u32 = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_CREATE
    | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE
    | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;

/// kqueue vnode triggers for the same idea. NOTE_LINK covers exercises like
/// a dep being replaced over the same path (unlink + fresh create).
#[cfg(target_os = "macos")]
const KQUEUE_VNODE_FFLAGS: u32 = libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB
    | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_LINK;

#[cfg(target_os = "linux")]
type Backend = InotifyBackend;
#[cfg(target_os = "macos")]
type Backend = KqueueBackend;

pub struct FileWatcher {
    inner: Backend,
}

impl FileWatcher {
    /// A watcher, or None when the platform backend is unavailable.
    pub fn new() -> Option<FileWatcher> {
        let inner = Backend::new()?;
        Some(FileWatcher { inner })
    }

    /// Watch one path (file or directory) by itself.
    pub fn watch_path(&mut self, path: &str) -> bool {
        self.inner.watch_path(path)
    }

    /// Watch a file plus its parent directory (the test-watch semantics:
    /// dir watches catch atomic saves and brand-new files, per-file watches
    /// catch plain in-place writes).
    pub fn watch_file(&mut self, path: &str) -> bool {
        let mut ok = self.inner.watch_path(path);
        if let Some(dir) = std::path::Path::new(path).parent() {
            let dir = dir.to_string_lossy().into_owned();
            if !dir.is_empty() {
                ok |= self.inner.watch_path(&dir);
            }
        }
        ok
    }

    /// Raw fd for libc::poll (inotify fd / kqueue fd; both POLLRDHUP-clean).
    pub fn fd(&self) -> i32 {
        self.inner.fd()
    }

    /// Discard all pending events. Non-blocking.
    pub fn drain(&mut self) {
        self.inner.drain();
    }

    /// Non-blocking read of one pending event batch. Whether anything
    /// relevant fired (the run-watch shape: no name mapping needed).
    pub fn poll_event(&mut self) -> bool {
        self.inner.poll_event()
    }

    /// Non-blocking read of one pending event batch, mapped back to the
    /// watched test files the events refer to (the test-watch shape).
    /// On Linux, directory events carry an entry name; on macOS, any event
    /// on a file's own vnode or its parent directory counts as a change to
    /// that file.
    pub fn poll_changed(&mut self, files: &HashSet<String>) -> Vec<String> {
        self.inner.poll_changed(files)
    }

    pub fn close(self) {
        self.inner.close();
    }
}

#[cfg(target_os = "linux")]
struct InotifyBackend {
    fd: i32,
    watched: HashSet<String>,
    wd_dirs: HashMap<i32, String>,
}

#[cfg(target_os = "linux")]
impl InotifyBackend {
    fn new() -> Option<InotifyBackend> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
        if fd < 0 {
            return None;
        }
        Some(InotifyBackend { fd, watched: HashSet::new(), wd_dirs: HashMap::new() })
    }

    fn watch_path(&mut self, path: &str) -> bool {
        if !self.watched.insert(path.to_string()) {
            return false;
        }
        let Some(c) = std::ffi::CString::new(path).ok() else { return false };
        let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), INOTIFY_WATCH_MASK) };
        if wd >= 0 {
            self.wd_dirs.insert(wd, path.to_string());
            true
        } else {
            false
        }
    }

    fn fd(&self) -> i32 {
        self.fd
    }

    fn drain(&mut self) {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break; // fully drained (EAGAIN or error; fd is non-blocking)
            }
            if (n as usize) < buf.len() {
                break;
            }
        }
    }

    /// Walk one pending event batch and pull out the affected paths.
    fn read_paths(&mut self) -> Vec<String> {
        let mut buf = [0u8; 8192];
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return vec![];
        }
        let n = n as usize;
        let mut paths = Vec::new();
        let mut off = 0usize;
        while off + 16 <= n {
            let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
            let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
            let _cookie = u32::from_ne_bytes(buf[off + 8..off + 12].try_into().unwrap());
            let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
            off += 16;
            let name_end = (off + len).min(n);
            let raw = &buf[off..name_end];
            off += len;
            let name = raw.split(|b| *b == 0).next().unwrap_or(&[]);
            if name.is_empty() && mask & libc::IN_ISDIR != 0 {
                continue; // directory event without an entry name
            }
            let Some(dir) = self.wd_dirs.get(&wd) else { continue };
            if name.is_empty() {
                paths.push(dir.clone());
            } else {
                paths.push(format!(
                    "{}/{}",
                    dir.trim_end_matches('/'),
                    String::from_utf8_lossy(name)
                ));
            }
        }
        paths
    }

    fn poll_event(&mut self) -> bool {
        !self.read_paths().is_empty()
    }

    fn poll_changed(&mut self, files: &HashSet<String>) -> Vec<String> {
        let changed: HashSet<String> = self
            .read_paths()
            .into_iter()
            .filter(|p| files.contains(p))
            .collect();
        changed.into_iter().collect()
    }

    fn close(self) {
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(target_os = "macos")]
struct KqueueBackend {
    kq: i32,
    /// open vnode handle -> watched path
    fd_paths: HashMap<i32, String>,
}

#[cfg(target_os = "macos")]
impl KqueueBackend {
    fn new() -> Option<KqueueBackend> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return None;
        }
        Some(KqueueBackend { kq, fd_paths: HashMap::new() })
    }

    fn register(&self, fd: i32) -> bool {
        // libc 0.2 does not export EV_SET on apple targets; build the kevent
        // manually (ident, EVFILT_VNODE, ADD|CLEAR, vnode fflags).
        let ev = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: (libc::EV_ADD | libc::EV_CLEAR) as libc::c_ushort,
            fflags: KQUEUE_VNODE_FFLAGS,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) >= 0 }
    }

    /// Watch order is guaranteed for one path: file first, then its parent
    /// directory (matches watch_file), so a plain poll_changed() maps events
    /// to exact files first and falls back to the dir-owns-children rule.
    fn watch_path(&mut self, path: &str) -> bool {
        if self.fd_paths.iter().any(|(_, p)| p == path) {
            return false; // already watched
        }
        let c = match std::ffi::CString::new(path) {
            Ok(c) => c,
            Err(_) => return false,
        };
        // O_EVTONLY: no write access, no follow.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY) };
        if fd < 0 {
            return false;
        }
        if !self.register(fd) {
            unsafe { libc::close(fd) };
            return false;
        }
        self.fd_paths.insert(fd, path.to_string());
        true
    }

    /// Re-register a path whose watch handle became stale (atomic saves:
    /// the old handle still names the deleted inode).
    fn rearm_path(&mut self, path: &str) {
        let stale: Vec<i32> = self
            .fd_paths
            .iter()
            .filter(|(_, p)| p.as_str() == path)
            .map(|(fd, _)| *fd)
            .collect();
        for fd in stale {
            self.fd_paths.remove(&fd);
            unsafe { libc::close(fd) };
        }
        self.watch_path(path);
    }

    fn fd(&self) -> i32 {
        self.kq
    }

    fn drain(&mut self) {
        let mut events: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
        loop {
            let n = unsafe {
                libc::kevent(
                    self.kq,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    &libc::timespec { tv_sec: 0, tv_nsec: 0 },
                )
            };
            if n <= 0 {
                break;
            }
            for ev in &events[..n as usize] {
                // A stale handle (deleted/renamed inode): re-arm the path so
                // the next generation of edits is still caught.
                if ev.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                    if let Some(path) = self
                        .fd_paths
                        .get(&(ev.ident as libc::c_int))
                        .cloned()
                    {
                        self.rearm_path(&path);
                    }
                }
            }
            if (n as usize) < events.len() {
                break;
            }
        }
    }

    /// Read one pending event batch; returns the watched paths affected.
    fn read_paths(&mut self) -> Vec<String> {
        let mut events: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::kevent(
                self.kq,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                &libc::timespec { tv_sec: 0, tv_nsec: 0 },
            )
        };
        if n <= 0 {
            return vec![];
        }
        let mut paths = Vec::new();
        for ev in &events[..n as usize] {
            let fflags = ev.fflags;
            if let Some(path) = self.fd_paths.get(&(ev.ident as libc::c_int)) {
                paths.push(path.clone());
            }
            if fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                if let Some(path) = self.fd_paths.get(&(ev.ident as libc::c_int)).cloned() {
                    self.rearm_path(&path);
                }
            }
        }
        paths
    }

    fn poll_event(&mut self) -> bool {
        !self.read_paths().is_empty()
    }

    fn poll_changed(&mut self, files: &HashSet<String>) -> Vec<String> {
        let mut changed: HashSet<String> = HashSet::new();
        for p in self.read_paths() {
            if files.contains(&p) {
                changed.insert(p);
                continue;
            }
            // kqueue directory events carry no entry name: any event on a
            // directory this file was registered under counts as change.
            let prefix = format!("{}/", p.trim_end_matches('/'));
            for f in files {
                if f.starts_with(&prefix) {
                    changed.insert(f.clone());
                }
            }
        }
        changed.into_iter().collect()
    }

    fn close(self) {
        for fd in self.fd_paths.keys() {
            unsafe { libc::close(*fd) };
        }
        unsafe { libc::close(self.kq) };
    }
}
