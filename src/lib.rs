//! rsview is a terminal JSON viewer with mmap-backed lazy parsing.
//!
//! **This crate ships only a binary — there is no public Rust API.**
//! Install with `cargo install rsview` (or `cargo binstall rsview`) and run
//! the `rsview` command. See <https://github.com/vothanhdat/rsview> for
//! usage, key bindings, and how the lazy byte-range tree keeps memory
//! near-constant on multi-GB files.
//!
//! This empty library target exists only so docs.rs (which always invokes
//! `cargo rustdoc --lib`) can complete its build — without it the docs
//! badge would stay red across every release for a crate that has nothing
//! to document anyway.
