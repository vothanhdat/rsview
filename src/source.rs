//! The byte source the viewer reads from.
//!
//! A file is memory-mapped (zero-copy, the kernel pages bytes in on demand — so
//! a multi-GB file opens in near-constant memory). A pipe has no seekable fd to
//! map, so piped stdin is spilled to a temp file that's mmap'd as it grows (see
//! `StreamStore` in main.rs) — also a `Mapped` here — or, where that isn't
//! available, read into a `Buffered` in RAM. Everything downstream — scanner,
//! flatten, search — works on `&[u8]` and never sees which variant it got.
//! `Source` is `Send + Sync` (both `Mmap` and `Vec<u8>` are), so it can live in
//! the `Arc` the search worker thread shares.

use memmap2::Mmap;
use std::ops::Deref;

pub enum Source {
    /// A memory-mapped file (an opened file, or a stream spilled to a temp
    /// file): bytes page in on demand, near-constant memory.
    Mapped(Mmap),
    /// Fully-buffered bytes in RAM — the streaming fallback when a stream can't
    /// be spilled to a mappable temp file.
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
