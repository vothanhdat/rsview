//! The streaming (piped-stdin) input path: a growing byte store plus the
//! background pipe reader that feeds it.
//!
//! A pipe can't be mmap'd, so [`StreamStore`] spills the arriving bytes to an
//! unlinked temp file and mmaps *that* — the document lives in evictable,
//! file-backed page cache instead of resident RAM, so RSS stays ~flat however
//! large the stream (the same property the file path has). Where spilling isn't
//! available it falls back to an in-RAM `Vec`. [`spawn_reader`] pulls the pipe
//! on a background thread and streams chunks to the UI loop over a channel.

use crate::source::Source;
use memmap2::{Mmap, MmapOptions};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::{
    mpsc::{self, Receiver},
    Arc, Weak,
};
use std::thread;

/// Throttle for the streaming re-parse: rebuild the tree at most this often as
/// bytes arrive, so a fast pipe doesn't reparse on every tiny chunk.
pub const STREAM_REBUILD_MS: u128 = 100;

/// The growing byte store behind a streamed (piped) document.
///
/// A pipe can't be mmap'd directly, but its bytes can be spilled to a temp file
/// and *that* mmap'd — so the document lives in evictable, file-backed page
/// cache instead of resident anonymous RAM, and RSS stays ~flat however large
/// the stream (the same property the file path has). The temp file is unlinked
/// the instant it's opened: it has no name on disk, the open fd keeps the inode
/// alive, and the OS reclaims the space when jsonview exits — cleanly, even on a
/// panic or `SIGKILL`. Where spilling isn't available (non-unix, or no writable
/// temp dir) it falls back to an in-RAM `Vec`, the fully-resident behaviour.
pub enum StreamStore {
    /// Spilled to an unlinked temp file, mmap'd and re-mapped as it grows. Old
    /// mappings stay valid because a stream only ever appends.
    Spilled {
        /// Read+append handle to the unlinked file; keeps the inode alive.
        file: File,
        /// Current mapping over the first `len` bytes, refreshed on growth.
        map: Option<Mmap>,
        /// Bytes durably written so far (`<= file size`; a partial failed write
        /// is never counted, so the mapped prefix is always consistent).
        len: usize,
    },
    /// In-RAM fallback: the whole document is resident.
    Ram(Vec<u8>),
}

impl StreamStore {
    /// Spill to a temp file where possible, else buffer in RAM.
    pub fn new() -> StreamStore {
        #[cfg(unix)]
        if let Some(s) = Self::try_spill() {
            return s;
        }
        StreamStore::Ram(Vec::new())
    }

    /// Create an unlinked temp file to spill into. `None` if no temp file could
    /// be opened (read-only temp dir, etc.) — the caller falls back to RAM.
    #[cfg(unix)]
    fn try_spill() -> Option<StreamStore> {
        use std::fs::OpenOptions;
        let dir = std::env::temp_dir();
        // A per-process name; unlinked immediately, so a collision only happens
        // if a prior run was killed in the microseconds before its own unlink —
        // try a few suffixes to be safe.
        for n in 0..8 {
            let path = dir.join(format!("jsonview-stream-{}-{}.json", std::process::id(), n));
            if let Ok(file) = OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .open(&path)
            {
                // Unlink now: no directory entry to clean up, so the space is
                // reclaimed on exit however jsonview dies. The fd (and any mmap of
                // it) keeps the bytes readable meanwhile.
                let _ = std::fs::remove_file(&path);
                return Some(StreamStore::Spilled {
                    file,
                    map: None,
                    len: 0,
                });
            }
        }
        None
    }

    /// Append a freshly-read chunk. Best-effort on a spill write failure (disk
    /// full): `len` simply doesn't advance, so the mapped prefix stays valid and
    /// the view just stops growing rather than crashing.
    pub fn append(&mut self, chunk: &[u8]) {
        match self {
            StreamStore::Spilled { file, len, .. } => {
                if file.write_all(chunk).is_ok() {
                    *len += chunk.len();
                }
            }
            StreamStore::Ram(v) => v.extend_from_slice(chunk),
        }
    }

    /// Refresh the mapping so [`bytes`](Self::bytes) covers everything appended
    /// so far. Cheap and zero-copy (mmap sets up page tables lazily); a failure
    /// keeps the previous, shorter map and is retried next tick.
    pub fn sync(&mut self) {
        if let StreamStore::Spilled { file, map, len } = self {
            if *len > 0 && map.as_ref().map_or(0, |m| m.len()) != *len {
                if let Ok(m) = unsafe { MmapOptions::new().len(*len).map(&*file) } {
                    *map = Some(m);
                }
            }
        }
    }

    /// The document bytes parsed/rendered this frame.
    pub fn bytes(&self) -> &[u8] {
        match self {
            StreamStore::Spilled { map, len, .. } => match map {
                Some(m) => &m[..(*len).min(m.len())],
                None => &[],
            },
            StreamStore::Ram(v) => v,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            StreamStore::Spilled { len, .. } => *len,
            StreamStore::Ram(v) => v.len(),
        }
    }

    /// Hand a search/filter worker an owned `Source` over the bytes arrived so
    /// far, reusing the last one when the store hasn't grown since.
    ///
    /// The worker outlives this frame, so it needs an owned source. For a spill
    /// that's a fresh mmap of the same file — zero-copy; for the RAM fallback
    /// it's an unavoidable buffer copy. Either way, because a stream only
    /// appends, a source of length N stays a byte-identical prefix of any longer
    /// store: while `cache` still points at a live source of the current length
    /// we hand back the same `Arc` (search relaunches once per keystroke, so
    /// this collapses per-character work to per-growth). The cache is weak, so
    /// the source is freed the moment the last worker drops it.
    pub fn snapshot(&self, cache: &mut Weak<Source>) -> Arc<Source> {
        let len = self.len();
        if let Some(snap) = cache.upgrade() {
            if snap.len() == len {
                return snap;
            }
        }
        let snap = match self {
            StreamStore::Spilled { file, len, .. } if *len > 0 => {
                match unsafe { MmapOptions::new().len(*len).map(file) } {
                    Ok(m) => Arc::new(Source::Mapped(m)),
                    Err(_) => Arc::new(Source::Buffered(self.bytes().to_vec())),
                }
            }
            _ => Arc::new(Source::Buffered(self.bytes().to_vec())),
        };
        *cache = Arc::downgrade(&snap);
        snap
    }
}

/// Point fd 0 back at the real terminal so crossterm reads keys from it (the
/// reader thread keeps its own dup'd handle to the pipe for the JSON).
///
/// crossterm's own fallback — open `/dev/tty` when stdin isn't a tty — is fatal
/// on macOS: a `/dev/tty` fd can't be registered with kqueue (it returns
/// EINVAL), so the key-event reader never initializes and the first keypress
/// errors out. The actual terminal device *can* be registered, so resolve its
/// path via `ttyname()` on stdout/stderr (still ttys here — only stdin was
/// piped), open it read-write, and dup it over stdin. Then `isatty(0)` is true,
/// crossterm uses fd 0 directly, and kqueue accepts it. Linux's `/dev/tty` is
/// pollable so this is redundant there, but it's harmless.
#[cfg(unix)]
fn reattach_terminal_to_stdin() {
    unsafe {
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            if libc::isatty(fd) != 1 {
                continue;
            }
            let name = libc::ttyname(fd);
            if name.is_null() {
                continue;
            }
            let f = libc::open(name, libc::O_RDWR);
            if f >= 0 {
                libc::dup2(f, libc::STDIN_FILENO);
                if f != libc::STDIN_FILENO {
                    libc::close(f);
                }
                return;
            }
        }
    }
}

/// Hand back a readable handle to the piped stdin for the background reader.
///
/// On unix the reader needs its own fd because we hand fd 0 back to the
/// terminal (see `reattach_terminal_to_stdin`): dup the pipe first, then
/// reattach. On Windows, crossterm reads keys from the console (not stdin), so
/// the reader can just take stdin directly.
#[cfg(unix)]
pub fn take_pipe_reader() -> Box<dyn Read + Send> {
    use std::os::fd::FromRawFd;
    unsafe {
        let dup = libc::dup(libc::STDIN_FILENO);
        reattach_terminal_to_stdin();
        Box::new(File::from_raw_fd(dup))
    }
}

#[cfg(not(unix))]
pub fn take_pipe_reader() -> Box<dyn Read + Send> {
    Box::new(std::io::stdin())
}

/// Read the pipe in chunks on a background thread, streaming each to the UI loop
/// over a channel. The thread ends (and the channel disconnects, signalling EOF)
/// when the pipe closes or the receiver is dropped.
pub fn spawn_reader(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break; // receiver gone — viewer quit
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    rx
}
