//! `cvfs`: the cross-platform filesystem backend of the `CareerVector` workspace.
//!
//! It stores opaque documents and Yjs op logs under one root, never reads their content, and
//! guarantees conditional writes (compare-and-swap on revision and clock), revision and hash
//! integrity, crash-safe commits and receipts. It builds on `std` only (no platform calls), so it
//! behaves the same on Linux, macOS and Windows.
//!
//! ```text
//! <root>/workspaces/<workspace>/<key>/snapshot.bin      the authored bytes
//!                                    /meta.json         the envelope without the bytes
//!                                    /ops/<clock>.bin   one Yjs update, with <clock>.json beside it
//! <root>/workspaces/<workspace>/_assets/<path>          ordinary asset files
//! <root>/.cvfs/                                         the control tree: locks, intents,
//!                                                       receipts, retained snapshots
//! ```
//!
//! Names (workspace ids and keys) map to file names with a reversible escape that is safe on
//! case-insensitive file systems and Windows; the real key is kept in `meta.json`.

mod fs;
mod model;

pub use fs::FsStore;
pub use model::{Commit, Dump, Error, Op, Origin, Receipt, Record, Result, Yjs, content_hash, hex};
