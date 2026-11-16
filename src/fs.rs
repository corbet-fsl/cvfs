use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::model::{Commit, Dump, Error, Op, Origin, Receipt, Record, Result, Yjs, content_hash};

const DEFAULT_KEEP: usize = 8;
const MAX_NAME: usize = 180;
const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

static TMP: AtomicU64 = AtomicU64::new(0);

/// A workspace store on a directory tree.
#[derive(Clone, Debug)]
pub struct FsStore {
    root: PathBuf,
    keep: usize,
}

/// `meta.json`: a record without its bytes.
#[derive(Serialize, Deserialize)]
struct Meta {
    schema_id: String,
    key: String,
    kind: String,
    revision: u64,
    content_hash: String,
    metadata: String,
    yjs: Option<Yjs>,
    origin: Option<Origin>,
}

impl Meta {
    fn of(r: &Record) -> Self {
        Self {
            schema_id: r.schema_id.clone(),
            key: r.key.clone(),
            kind: r.kind.clone(),
            revision: r.revision,
            content_hash: r.content_hash.clone(),
            metadata: r.metadata.clone(),
            yjs: r.yjs.clone(),
            origin: r.origin.clone(),
        }
    }

    fn with_bytes(self, bytes: Vec<u8>) -> Record {
        Record {
            schema_id: self.schema_id,
            key: self.key,
            kind: self.kind,
            revision: self.revision,
            content_hash: self.content_hash,
            bytes,
            metadata: self.metadata,
            yjs: self.yjs,
            origin: self.origin,
        }
    }
}

/// `ops/<clock>.json`.
#[derive(Serialize, Deserialize)]
struct OpMeta {
    clock: u64,
    actor_class: String,
    actor_id: String,
    at_ms: u64,
    hash: String,
}

/// The write-ahead record of a commit, replayed after a crash.
#[derive(Serialize, Deserialize)]
struct Intent {
    commit: Commit,
    seq: u64,
}

/// A file-system-safe, reversible-by-`meta.json` form of a name. Lowercase letters, digits and
/// `-` stay; a `.` stays when it is not at either end; every other byte becomes `%XX`. Uppercase is
/// escaped so keys differing only in case never share a file on a case-insensitive file system.
fn encode_name(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Error::Invalid("empty name".into()));
    }
    let last = name.len() - 1;
    let mut out = String::with_capacity(name.len());
    for (i, b) in name.bytes().enumerate() {
        let plain = b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || b == b'-'
            || (b == b'.' && i != 0 && i != last);
        if plain {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    let stem = out.split('.').next().unwrap_or("");
    if RESERVED.contains(&stem) {
        let first = out.remove(0);
        out = format!("%{:02X}{out}", first as u8);
    }
    if out.len() > MAX_NAME {
        return Err(Error::Invalid("name too long".into()));
    }
    Ok(out)
}

/// An asset path: `/`-separated, each segment a portable file name.
fn check_asset_path(path: &str) -> Result<Vec<&str>> {
    let segments: Vec<&str> = path.split('/').collect();
    for s in &segments {
        let bad = s.is_empty()
            || *s == "."
            || *s == ".."
            || s.len() > 200
            || s.ends_with('.')
            || s.ends_with(' ')
            || s.chars()
                .any(|c| c.is_control() || "\\:*?\"<>|".contains(c))
            || RESERVED.contains(
                &s.split('.')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str(),
            );
        if bad {
            return Err(Error::Invalid(format!("bad asset path segment {s:?}")));
        }
    }
    Ok(segments)
}

fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Write `bytes` to `path` so that readers see the old file or the new one, never a part.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Invalid("no parent".into()))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = File::create(&tmp)?;
    IoWrite::write_all(&mut f, bytes)?;
    f.sync_all()?;
    drop(f);
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    sync_dir(dir);
    Ok(())
}

fn read_opt(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Names of the entries of a directory, sorted; empty when it does not exist.
fn list(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    match fs::read_dir(dir) {
        Ok(rd) => {
            for e in rd {
                let name = e?.file_name().to_string_lossy().into_owned();
                if !name.starts_with(".tmp-") {
                    names.push(name);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    names.sort();
    Ok(names)
}

fn read_record(bin: &Path, meta: &Path) -> Result<Option<Record>> {
    let Some(m) = read_opt(meta)? else {
        return Ok(None);
    };
    let m: Meta = serde_json::from_slice(&m)?;
    let bytes =
        read_opt(bin)?.ok_or_else(|| Error::Integrity(format!("{}: snapshot missing", m.key)))?;
    if content_hash(&bytes) != m.content_hash {
        return Err(Error::Integrity(format!(
            "{}: bytes do not match the recorded hash",
            m.key
        )));
    }
    Ok(Some(m.with_bytes(bytes)))
}

fn check_record(r: &Record) -> Result<()> {
    if r.revision == 0 {
        return Err(Error::Invalid("revision 0 is not a stored revision".into()));
    }
    if content_hash(&r.bytes) != r.content_hash {
        return Err(Error::Integrity(format!(
            "{}: hash does not match the bytes",
            r.key
        )));
    }
    if r.yjs.as_ref().is_some_and(|y| y.clock == 0) {
        return Err(Error::Invalid("clock 0 is not a stored clock".into()));
    }
    Ok(())
}

impl FsStore {
    /// Open (and create) a store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("workspaces"))?;
        fs::create_dir_all(root.join(".cvfs"))?;
        Ok(Self {
            root,
            keep: DEFAULT_KEEP,
        })
    }

    /// Keep this many earlier revisions of every document (default 8).
    #[must_use]
    pub fn with_retention(mut self, keep: usize) -> Self {
        self.keep = keep;
        self
    }

    fn ctl(&self) -> PathBuf {
        self.root.join(".cvfs")
    }

    fn ws_dir(&self, ws: &str) -> Result<PathBuf> {
        Ok(self.root.join("workspaces").join(encode_name(ws)?))
    }

    fn doc_dir(&self, ws: &str, key: &str) -> Result<PathBuf> {
        Ok(self.ws_dir(ws)?.join(encode_name(key)?))
    }

    fn receipts_dir(&self, ws: &str) -> Result<PathBuf> {
        Ok(self.ctl().join("receipts").join(encode_name(ws)?))
    }

    fn retained_dir(&self, ws: &str, key: &str) -> Result<PathBuf> {
        Ok(self
            .ctl()
            .join("snapshots")
            .join(encode_name(ws)?)
            .join(encode_name(key)?))
    }

    fn intent_path(&self, ws: &str) -> Result<PathBuf> {
        Ok(self
            .ctl()
            .join("intent")
            .join(format!("{}.json", encode_name(ws)?)))
    }

    /// Run `f` holding the workspace's exclusive lock, after finishing any interrupted commit.
    fn locked<T>(&self, ws: &str, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        let dir = self.ctl().join("locks");
        fs::create_dir_all(&dir)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{}.lock", encode_name(ws)?)))?;
        file.lock()?;
        let out = self.recover(ws).and_then(|()| f(self));
        let _ = file.unlock();
        out
    }

    fn recover(&self, ws: &str) -> Result<()> {
        let path = self.intent_path(ws)?;
        let Some(raw) = read_opt(&path)? else {
            return Ok(());
        };
        let Ok(intent) = serde_json::from_slice::<Intent>(&raw) else {
            // A torn intent was never applied: it is written before anything else.
            fs::remove_file(&path)?;
            return Ok(());
        };
        self.apply(ws, &intent)?;
        fs::remove_file(&path)?;
        Ok(())
    }

    fn current(&self, ws: &str, key: &str) -> Result<Option<Record>> {
        let d = self.doc_dir(ws, key)?;
        read_record(&d.join("snapshot.bin"), &d.join("meta.json"))
    }

    /// The steps of a commit; every one is safe to repeat.
    fn apply(&self, ws: &str, intent: &Intent) -> Result<()> {
        let rec = &intent.commit.record;
        let doc = self.doc_dir(ws, &rec.key)?;
        if let Some(old) = self.current(ws, &rec.key)?
            && old.revision + 1 == rec.revision
        {
            self.retain(ws, &old)?;
        }
        if let Some(op) = &intent.commit.op {
            Self::write_op(&doc, op)?;
        }
        write_atomic(&doc.join("snapshot.bin"), &rec.bytes)?;
        write_atomic(
            &doc.join("meta.json"),
            &serde_json::to_vec_pretty(&Meta::of(rec))?,
        )?;
        let receipt = Receipt {
            seq: intent.seq,
            key: rec.key.clone(),
            revision: rec.revision,
            clock: rec.yjs.as_ref().map(|y| y.clock),
            content_hash: rec.content_hash.clone(),
            at_ms: intent.commit.at_ms,
        };
        self.write_receipt(ws, &receipt)
    }

    fn write_op(doc: &Path, op: &Op) -> Result<()> {
        let name = format!("{:020}", op.clock);
        let meta = OpMeta {
            clock: op.clock,
            actor_class: op.actor_class.clone(),
            actor_id: op.actor_id.clone(),
            at_ms: op.at_ms,
            hash: content_hash(&op.bytes),
        };
        write_atomic(&doc.join("ops").join(format!("{name}.bin")), &op.bytes)?;
        write_atomic(
            &doc.join("ops").join(format!("{name}.json")),
            &serde_json::to_vec_pretty(&meta)?,
        )
    }

    fn write_receipt(&self, ws: &str, r: &Receipt) -> Result<()> {
        let path = self.receipts_dir(ws)?.join(format!("{:020}.json", r.seq));
        write_atomic(&path, &serde_json::to_vec_pretty(r)?)
    }

    fn write_doc(&self, ws: &str, r: &Record) -> Result<()> {
        let doc = self.doc_dir(ws, &r.key)?;
        write_atomic(&doc.join("snapshot.bin"), &r.bytes)?;
        write_atomic(
            &doc.join("meta.json"),
            &serde_json::to_vec_pretty(&Meta::of(r))?,
        )
    }

    fn write_retained(&self, ws: &str, r: &Record) -> Result<()> {
        let dir = self.retained_dir(ws, &r.key)?;
        let name = format!("{:020}", r.revision);
        write_atomic(&dir.join(format!("{name}.bin")), &r.bytes)?;
        write_atomic(
            &dir.join(format!("{name}.json")),
            &serde_json::to_vec_pretty(&Meta::of(r))?,
        )
    }

    fn retain(&self, ws: &str, old: &Record) -> Result<()> {
        self.write_retained(ws, old)?;
        let dir = self.retained_dir(ws, &old.key)?;
        let mut revs: Vec<String> = list(&dir)?
            .into_iter()
            .filter_map(|n| n.strip_suffix(".json").map(str::to_owned))
            .collect();
        revs.sort();
        let drop = revs.len().saturating_sub(self.keep);
        for rev in &revs[..drop] {
            for ext in ["json", "bin"] {
                let _ = fs::remove_file(dir.join(format!("{rev}.{ext}")));
            }
        }
        Ok(())
    }

    fn next_seq(&self, ws: &str) -> Result<u64> {
        let last = list(&self.receipts_dir(ws)?)?
            .iter()
            .filter_map(|n| n.strip_suffix(".json")?.parse::<u64>().ok())
            .max();
        Ok(last.map_or(1, |s| s + 1))
    }

    /// Apply a conditional write. Returns its receipt, or `Error::Conflict` when the stored
    /// revision (or clock) is not the one the commit builds on; nothing is written then.
    pub fn commit(&self, ws: &str, c: &Commit) -> Result<Receipt> {
        let rec = &c.record;
        check_record(rec)?;
        match (&rec.yjs, &c.op) {
            (Some(y), Some(op)) if op.clock == y.clock => {}
            (None, None) => {}
            _ => {
                return Err(Error::Invalid(
                    "a Yjs record needs an op of its clock, and only it".into(),
                ));
            }
        }
        if let Some(op) = &c.op
            && op.bytes.is_empty()
        {
            return Err(Error::Invalid("empty op".into()));
        }
        self.locked(ws, |s| {
            let cur = s.current(ws, &rec.key)?;
            let rev = cur.as_ref().map_or(0, |r| r.revision);
            if rec.revision != rev + 1 {
                return Err(Error::Conflict {
                    key: rec.key.clone(),
                    expected: rec.revision - 1,
                    actual: rev,
                });
            }
            let clock = cur.as_ref().and_then(|r| r.yjs.as_ref()).map(|y| y.clock);
            match (&rec.yjs, clock) {
                (Some(y), cur) => {
                    let cur = cur.unwrap_or(0);
                    if y.clock != cur + 1 {
                        return Err(Error::Conflict {
                            key: rec.key.clone(),
                            expected: y.clock - 1,
                            actual: cur,
                        });
                    }
                }
                (None, Some(_)) => {
                    return Err(Error::Invalid(
                        "a Yjs document cannot lose its clock".into(),
                    ));
                }
                (None, None) => {}
            }
            let intent = Intent {
                commit: c.clone(),
                seq: s.next_seq(ws)?,
            };
            let path = s.intent_path(ws)?;
            write_atomic(&path, &serde_json::to_vec(&intent)?)?;
            s.apply(ws, &intent)?;
            fs::remove_file(&path)?;
            Ok(Receipt {
                seq: intent.seq,
                key: rec.key.clone(),
                revision: rec.revision,
                clock: rec.yjs.as_ref().map(|y| y.clock),
                content_hash: rec.content_hash.clone(),
                at_ms: c.at_ms,
            })
        })
    }

    /// The current record of a document.
    pub fn get(&self, ws: &str, key: &str) -> Result<Option<Record>> {
        self.locked(ws, |s| s.current(ws, key))
    }

    /// Every document key of a workspace, sorted.
    pub fn keys(&self, ws: &str) -> Result<Vec<String>> {
        self.locked(ws, Self::all_keys(ws))
    }

    fn all_keys(ws: &str) -> impl FnOnce(&Self) -> Result<Vec<String>> + '_ {
        move |s| {
            let mut keys = Vec::new();
            for name in list(&s.ws_dir(ws)?)? {
                if name == "_assets" {
                    continue;
                }
                if let Some(m) = read_opt(&s.ws_dir(ws)?.join(&name).join("meta.json"))? {
                    keys.push(serde_json::from_slice::<Meta>(&m)?.key);
                }
            }
            keys.sort();
            Ok(keys)
        }
    }

    /// The ops of a Yjs document with a clock above `after`, in clock order.
    pub fn ops_since(&self, ws: &str, key: &str, after: u64) -> Result<Vec<Op>> {
        self.locked(ws, |s| s.read_ops(ws, key, after))
    }

    fn read_ops(&self, ws: &str, key: &str, after: u64) -> Result<Vec<Op>> {
        let dir = self.doc_dir(ws, key)?.join("ops");
        let mut ops = Vec::new();
        for name in list(&dir)? {
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(clock) = stem.parse::<u64>() else {
                continue;
            };
            if clock <= after {
                continue;
            }
            let meta: OpMeta = serde_json::from_slice(
                &read_opt(&dir.join(&name))?
                    .ok_or_else(|| Error::Integrity("op metadata vanished".into()))?,
            )?;
            let bytes = read_opt(&dir.join(format!("{stem}.bin")))?
                .ok_or_else(|| Error::Integrity(format!("{key}: op {clock} bytes missing")))?;
            if content_hash(&bytes) != meta.hash || meta.clock != clock {
                return Err(Error::Integrity(format!(
                    "{key}: op {clock} does not match its metadata"
                )));
            }
            ops.push(Op {
                clock,
                actor_class: meta.actor_class,
                actor_id: meta.actor_id,
                at_ms: meta.at_ms,
                bytes,
            });
        }
        Ok(ops)
    }

    /// Receipts with a sequence number above `after`, in order.
    pub fn receipts(&self, ws: &str, after: u64) -> Result<Vec<Receipt>> {
        self.locked(ws, |s| s.read_receipts(ws, after))
    }

    fn read_receipts(&self, ws: &str, after: u64) -> Result<Vec<Receipt>> {
        let dir = self.receipts_dir(ws)?;
        let mut out = Vec::new();
        for name in list(&dir)? {
            let Some(seq) = name
                .strip_suffix(".json")
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            if seq > after {
                let raw = read_opt(&dir.join(&name))?
                    .ok_or_else(|| Error::Integrity("receipt vanished".into()))?;
                out.push(serde_json::from_slice(&raw)?);
            }
        }
        Ok(out)
    }

    /// Earlier revisions kept of a document, oldest first.
    pub fn retained(&self, ws: &str, key: &str) -> Result<Vec<Record>> {
        self.locked(ws, |s| s.read_retained(ws, key))
    }

    fn read_retained(&self, ws: &str, key: &str) -> Result<Vec<Record>> {
        let dir = self.retained_dir(ws, key)?;
        let mut out = Vec::new();
        for name in list(&dir)? {
            if let Some(stem) = name.strip_suffix(".json")
                && let Some(r) = read_record(&dir.join(format!("{stem}.bin")), &dir.join(&name))?
            {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Store an asset (an ordinary file) under a `/`-separated relative path.
    pub fn put_asset(&self, ws: &str, path: &str, bytes: &[u8]) -> Result<()> {
        let segments = check_asset_path(path)?;
        let mut file = self.ws_dir(ws)?.join("_assets");
        file.extend(segments);
        self.locked(ws, |_| write_atomic(&file, bytes))
    }

    /// The bytes of an asset.
    pub fn get_asset(&self, ws: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let segments = check_asset_path(path)?;
        let mut file = self.ws_dir(ws)?.join("_assets");
        file.extend(segments);
        self.locked(ws, |_| read_opt(&file))
    }

    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        for name in list(dir)? {
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let p = dir.join(&name);
            if p.is_dir() {
                Self::walk(&p, &rel, out)?;
            } else {
                out.push((rel, fs::read(&p)?));
            }
        }
        Ok(())
    }

    /// Everything of a workspace in a deterministic order.
    pub fn dump(&self, ws: &str) -> Result<Dump> {
        self.locked(ws, |s| {
            let mut d = Dump::default();
            for key in Self::all_keys(ws)(s)? {
                d.records.extend(s.current(ws, &key)?);
                d.ops.extend(
                    s.read_ops(ws, &key, 0)?
                        .into_iter()
                        .map(|o| (key.clone(), o)),
                );
                d.retained.extend(s.read_retained(ws, &key)?);
            }
            d.records.sort_by(|a, b| a.key.cmp(&b.key));
            d.ops
                .sort_by(|a, b| (&a.0, a.1.clock).cmp(&(&b.0, b.1.clock)));
            d.retained
                .sort_by(|a, b| (&a.key, a.revision).cmp(&(&b.key, b.revision)));
            d.receipts = s.read_receipts(ws, 0)?;
            Self::walk(&s.ws_dir(ws)?.join("_assets"), "", &mut d.assets)?;
            Ok(d)
        })
    }

    /// Load a dump into an empty workspace, keeping receipts, clocks and retained revisions exactly.
    pub fn restore(&self, ws: &str, d: &Dump) -> Result<()> {
        for r in d.records.iter().chain(&d.retained) {
            check_record(r)?;
        }
        for (p, _) in &d.assets {
            check_asset_path(p)?;
        }
        self.locked(ws, |s| {
            let empty = list(&s.ws_dir(ws)?)?.is_empty() && list(&s.receipts_dir(ws)?)?.is_empty();
            if !empty {
                return Err(Error::Invalid(format!("workspace {ws} is not empty")));
            }
            for r in &d.records {
                s.write_doc(ws, r)?;
            }
            for r in &d.retained {
                s.write_retained(ws, r)?;
            }
            for (key, op) in &d.ops {
                Self::write_op(&s.doc_dir(ws, key)?, op)?;
            }
            for r in &d.receipts {
                s.write_receipt(ws, r)?;
            }
            for (p, bytes) in &d.assets {
                let mut file = s.ws_dir(ws)?.join("_assets");
                file.extend(check_asset_path(p)?);
                write_atomic(&file, bytes)?;
            }
            Ok(())
        })
    }
}
