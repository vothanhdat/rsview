//! The byte source the viewer reads from.
//!
//! A file is memory-mapped (zero-copy, the kernel pages bytes in on demand — so
//! a multi-GB file opens in near-constant memory). A pipe has no seekable fd to
//! map, so piped stdin is read fully into a buffer instead. Everything
//! downstream — scanner, flatten, search — works on `&[u8]` and never sees
//! which variant it got. `Source` is `Send + Sync` (both `Mmap` and `Vec<u8>`
//! are), so it can live in the `Arc` the search worker thread shares.

use memmap2::Mmap;
use std::ops::Deref;

pub enum Source {
    /// A memory-mapped file: bytes page in on demand, near-constant memory.
    Mapped(Mmap),
    /// Fully-buffered bytes (piped stdin — a stream can't be mmap'd).
    Buffered(Vec<u8>),
}

impl Deref for Source {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Source::Mapped(m) => m,
            Source::Buffered(v) => v,
        }
    }
}
