//! Plain data the backend stores. The backend never interprets `bytes` or `metadata`.

use serde::{Deserialize, Serialize};

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Bytes of a lowercase or uppercase hex string.
pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `sha256:<hex>` of `bytes`, the form `Record::content_hash` takes.
#[must_use]
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

pub(crate) mod hexbytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        super::unhex(&text).ok_or_else(|| serde::de::Error::custom("bad hex"))
    }
}

/// State vector and snapshot clock of a Yjs sub-document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Yjs {
    #[serde(with = "hexbytes")]
    pub state_vector: Vec<u8>,
    pub clock: u64,
}

/// Who wrote a revision and on top of which one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub actor: String,
    pub device: String,
    pub parent_revision: u64,
}

/// One document: the envelope of the charter, with the kind as an opaque string and the metadata as
/// opaque JSON text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema_id: String,
    pub key: String,
    pub kind: String,
    pub revision: u64,
    pub content_hash: String,
    #[serde(with = "hexbytes")]
    pub bytes: Vec<u8>,
    pub metadata: String,
    pub yjs: Option<Yjs>,
    pub origin: Option<Origin>,
}

/// One appended update of a Yjs sub-document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub clock: u64,
    pub actor_class: String,
    pub actor_id: String,
    pub at_ms: u64,
    #[serde(with = "hexbytes")]
    pub bytes: Vec<u8>,
}

/// Proof that a commit was applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub seq: u64,
    pub key: String,
    pub revision: u64,
    pub clock: Option<u64>,
    pub content_hash: String,
    pub at_ms: u64,
}

/// A conditional write. `record.revision` is the NEW revision: the commit applies only if the stored
/// revision is one less (0 when the document does not exist). For a Yjs document `record.yjs.clock`
/// is the new clock, applies only on top of the previous clock, and `op` carries the update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub record: Record,
    pub op: Option<Op>,
    pub at_ms: u64,
}

/// Everything of one workspace, in a deterministic order, for lossless conversion between backends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dump {
    /// By key.
    pub records: Vec<Record>,
    /// By (key, clock).
    pub ops: Vec<(String, Op)>,
    /// By seq.
    pub receipts: Vec<Receipt>,
    /// Retained earlier revisions, by (key, revision).
    pub retained: Vec<Record>,
    /// By path.
    pub assets: Vec<(String, Vec<u8>)>,
}

/// Why an operation failed.
#[derive(Debug)]
pub enum Error {
    /// The stored revision or clock is not the one the commit builds on.
    Conflict {
        key: String,
        expected: u64,
        actual: u64,
    },
    /// The record's hash does not match its bytes, or a stored file does not match its metadata.
    Integrity(String),
    /// The request is malformed (bad name, impossible combination).
    Invalid(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict {
                key,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "conflict on {key}: commit builds on {expected}, store is at {actual}"
                )
            }
            Self::Integrity(m) => write!(f, "integrity: {m}"),
            Self::Invalid(m) => write!(f, "invalid: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Integrity(format!("unreadable metadata: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
