//! The state directory: one path resolver, one lock routine, one atomic write.
//!
//! The directory is private (0700, files 0600) because the socket is a direct
//! line into claude's stdin and the spawn record holds the panel's environment.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Files belonging to one run of one session, removed when it ends. The lock and
/// the cursor owner are absent on purpose: each is removed by the process that
/// holds it, never by anybody else.
pub(crate) const RUN_EXTS: [&str; 4] = ["status.json", "spawn.json", "events.jsonl", "cursor"];

pub(crate) fn home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("CLAUDE_RELAYD_HOME") {
        // Absolute, or a relative value would silently give every working
        // directory its own state directory, and with it its own claude.
        return std::path::absolute(&h).with_context(|| {
            format!("resolve CLAUDE_RELAYD_HOME {}", PathBuf::from(&h).display())
        });
    }
    match std::env::var_os("HOME") {
        Some(h) => Ok(PathBuf::from(h).join(".claude").join("relayd")),
        None => bail!("resolve the state directory: neither CLAUDE_RELAYD_HOME nor HOME is set"),
    }
}

/// Creates `dir` 0700 and refuses one owned by another user. Everything private
/// this tool writes goes through here, including the socket's fallback home.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    let meta = fs::metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    // SAFETY: geteuid always succeeds.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        bail!("use {}: it is owned by uid {}", dir.display(), meta.uid());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("tighten {} to 0700", dir.display()))?;
    }
    Ok(())
}

pub(crate) fn ensure_home() -> Result<PathBuf> {
    let dir = home()?;
    ensure_private_dir(&dir)?;
    Ok(dir)
}

pub(crate) fn file(dir: &Path, id: &str, ext: &str) -> PathBuf {
    dir.join(format!("{id}.{ext}"))
}

/// A unix socket path must fit in `sun_path` (108 bytes). Both sides derive the
/// fallback the same way, so a long state directory cannot split them, and the
/// fallback lives in a directory of ours rather than loose in a shared `/tmp`.
pub(crate) fn socket(dir: &Path, id: &str) -> PathBuf {
    let direct = file(dir, id, "sock");
    if direct.as_os_str().len() <= 100 {
        return direct;
    }
    // The whole path, not the directory and the id concatenated: those run
    // together, so two different sessions could hash to one socket.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in direct.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // SAFETY: geteuid always succeeds.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("claude-relayd-{uid}"))
        .join(format!("{hash:016x}.sock"))
}

/// A pid on its own is not an identity: pids are recycled. Pairing it with the
/// process start time makes a stale file impossible to mistake for a live one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Holder {
    pub(crate) pid: i32,
    pub(crate) start: u64,
}

impl Holder {
    pub(crate) fn current() -> Result<Holder> {
        // SAFETY: getpid always succeeds.
        let pid = unsafe { libc::getpid() };
        match proc_start(pid) {
            Some(start) => Ok(Holder { pid, start }),
            None => bail!("read the start time of pid {pid}"),
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Holder> {
        let mut it = raw.split_whitespace();
        let pid = it.next()?.parse().ok()?;
        let start = it.next()?.parse().ok()?;
        Some(Holder { pid, start })
    }

    pub(crate) fn text(&self) -> String {
        format!("{} {}", self.pid, self.start)
    }

    /// True while this exact process still occupies the pid, a zombie included.
    /// This is what a claim file asks: an unreaped holder may still own what it
    /// claimed, and refusing to take it from them is the safe direction.
    pub(crate) fn is_live(&self) -> bool {
        proc_start(self.pid).is_some_and(|s| s == self.start)
    }

    /// True only while the process is genuinely running. A zombie has exited, and
    /// reading one as a living extension host would call a dead window a closed
    /// tab and end the conversation this tool exists to keep.
    pub(crate) fn is_running(&self) -> bool {
        proc_stat(self.pid).is_some_and(|(state, start)| start == self.start && state != 'Z')
    }
}

/// The state and start time of a process, read after the comm field so a name
/// containing spaces or brackets cannot shift the offsets.
fn proc_stat(pid: i32) -> Option<(char, u64)> {
    let line = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = line.rsplit_once(") ")?.1;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

pub(crate) fn proc_start(pid: i32) -> Option<u64> {
    proc_stat(pid).map(|(_, start)| start)
}

/// The live holder of a pid file, or `None` when the file is missing, garbled or
/// left behind by a process that is gone.
pub(crate) fn live_holder(path: &Path) -> Option<Holder> {
    let raw = fs::read_to_string(path).ok()?;
    let h = Holder::parse(raw.trim())?;
    h.is_live().then_some(h)
}

/// A pid file this process owns and removes on the way out. What excludes a
/// rival is the kernel lock on the open file, not the text in it: it is taken
/// atomically and released however the holder dies, so there is no stale lock to
/// clear. The text stays because `list` and `kill` need a pid to report.
pub(crate) struct Claim {
    path: PathBuf,
    /// Held for the lock the kernel keeps on it; closing releases the claim.
    _locked: File,
}

impl Drop for Claim {
    fn drop(&mut self) {
        remove_quietly(&self.path);
    }
}

/// Removes a file that may already be gone, reporting anything else.
pub(crate) fn remove_quietly(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("claude-relayd: failed to remove {}: {e}", path.display());
    }
}

/// True while `f` is still the file `path` names. `flock` locks an open file,
/// not a name, so a holder that unlinked its own file on the way out can leave
/// us locking an inode nobody can reach.
fn still_named(f: &File, path: &Path) -> bool {
    match (f.metadata(), fs::metadata(path)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// Takes `path` for this process. `Ok(None)` means somebody else owns it; that
/// is never an error here, it is the one-claude-per-session-id guarantee doing
/// its job.
///
/// `flock` decides it, because only the kernel can make taking a claim atomic
/// with proving the previous one is gone. Checking a pid file and then unlinking
/// it is two steps: two processes reading one dead holder both unlink, and the
/// second unlinks the live claim the first had just written.
pub(crate) fn claim(path: &Path, me: Holder) -> Result<Option<Claim>> {
    // Bounded, because a rival unlinking its own claim between our open and our
    // lock is the only way round, and it cannot repeat indefinitely.
    for _ in 0..8 {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        // SAFETY: flock on a descriptor this process just opened.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            return match e.kind() {
                std::io::ErrorKind::WouldBlock => Ok(None),
                _ => Err(e).with_context(|| format!("lock {}", path.display())),
            };
        }
        if !still_named(&f, path) {
            continue;
        }
        let mut raw = String::new();
        f.read_to_string(&mut raw)
            .with_context(|| format!("read {}", path.display()))?;
        // A holder that took this file without the kernel lock: an older build,
        // still running, still owning whatever this names. Refusing it is what
        // makes replacing the binary under a live session safe.
        if Holder::parse(raw.trim()).is_some_and(|h| h != me && h.is_live()) {
            return Ok(None);
        }
        f.set_len(0).with_context(|| format!("clear {}", path.display()))?;
        f.rewind().with_context(|| format!("rewind {}", path.display()))?;
        f.write_all(me.text().as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        return Ok(Some(Claim {
            path: path.to_path_buf(),
            _locked: f,
        }));
    }
    Ok(None)
}

/// Writes through a temporary name in the same directory and renames into place,
/// so a half-written status or cursor file is never observable.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(dir) = path.parent() else {
        bail!("write {}: it has no parent directory", path.display());
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    // SAFETY: getpid always succeeds.
    let tmp = dir.join(format!(".{name}.{}.tmp", unsafe { libc::getpid() }));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    drop(f);
    fs::rename(&tmp, path).with_context(|| format!("rename {} into place", tmp.display()))
}

pub(crate) fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Appends to a log kept across runs, created 0600.
pub(crate) fn append_log(path: &Path) -> Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// Creates or truncates a private per-run file.
pub(crate) fn create_private(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}
