use cvfs::{Commit, Error, FsStore, Op, Origin, Record, Yjs, content_hash};

fn rec(key: &str, revision: u64, bytes: &[u8]) -> Record {
    Record {
        schema_id: "careervector.node-tree/v1".into(),
        key: key.into(),
        kind: "projected".into(),
        revision,
        content_hash: content_hash(bytes),
        bytes: bytes.to_vec(),
        metadata: r#"{"a":1,"unknown":[true]}"#.into(),
        yjs: None,
        origin: Some(Origin {
            actor: "human".into(),
            device: "d1".into(),
            parent_revision: revision - 1,
        }),
    }
}

fn yrec(key: &str, revision: u64, clock: u64, bytes: &[u8]) -> Record {
    let mut r = rec(key, revision, bytes);
    r.kind = "crdt-snapshot".into();
    r.yjs = Some(Yjs {
        state_vector: vec![0, 1, 2, u8::try_from(clock).unwrap()],
        clock,
    });
    r
}

fn op(clock: u64, bytes: &[u8]) -> Op {
    Op {
        clock,
        actor_class: "human".into(),
        actor_id: "u1".into(),
        at_ms: 1_000 + clock,
        bytes: bytes.to_vec(),
    }
}

fn commit(record: Record, op: Option<Op>) -> Commit {
    Commit {
        record,
        op,
        at_ms: 5_000,
    }
}

fn store() -> (tempfile::TempDir, FsStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = FsStore::open(dir.path()).unwrap();
    (dir, s)
}

#[test]
fn roundtrip_preserves_bytes_revisions_and_receipts() {
    let (d, s) = store();
    // Authored bytes that no text handling would keep: CRLF, NUL, invalid UTF-8, no final newline.
    let raw = b"line\r\nnul\0\xff\xfe end";
    let r1 = s
        .commit("ws", &commit(rec("cv.toml", 1, raw), None))
        .unwrap();
    let r2 = s
        .commit("ws", &commit(rec("cv.toml", 2, b"second"), None))
        .unwrap();
    assert_eq!((r1.seq, r1.revision, r2.seq, r2.revision), (1, 1, 2, 2));
    assert_eq!(r1.content_hash, content_hash(raw));
    let got = s.get("ws", "cv.toml").unwrap().unwrap();
    assert_eq!(got, rec("cv.toml", 2, b"second"));
    assert_eq!(s.receipts("ws", 0).unwrap(), vec![r1.clone(), r2.clone()]);
    assert_eq!(s.receipts("ws", 1).unwrap(), vec![r2]);
    // The first revision is retained byte for byte.
    assert_eq!(
        s.retained("ws", "cv.toml").unwrap(),
        vec![rec("cv.toml", 1, raw)]
    );
    // The authored bytes sit in an ordinary file.
    let ondisk = std::fs::read(d.path().join("workspaces/ws/cv.toml/snapshot.bin")).unwrap();
    assert_eq!(ondisk, b"second");
    // A new handle on the same root sees the same state.
    let again = FsStore::open(d.path()).unwrap();
    assert_eq!(
        again.get("ws", "cv.toml").unwrap(),
        s.get("ws", "cv.toml").unwrap()
    );
    assert_eq!(again.keys("ws").unwrap(), vec!["cv.toml".to_string()]);
    assert_eq!(s.get("ws", "missing").unwrap(), None);
}

#[test]
fn cas_conflicts_are_refused_and_change_nothing() {
    let (_d, s) = store();
    s.commit("ws", &commit(rec("k", 1, b"one"), None)).unwrap();
    // Builds on a revision that is not current.
    let stale = s.commit("ws", &commit(rec("k", 1, b"other"), None));
    assert!(matches!(
        stale,
        Err(Error::Conflict {
            expected: 0,
            actual: 1,
            ..
        })
    ));
    let skip = s.commit("ws", &commit(rec("k", 3, b"skip"), None));
    assert!(matches!(
        skip,
        Err(Error::Conflict {
            expected: 2,
            actual: 1,
            ..
        })
    ));
    assert_eq!(s.get("ws", "k").unwrap().unwrap().bytes, b"one");
    assert_eq!(s.receipts("ws", 0).unwrap().len(), 1);
    // A wrong hash is refused.
    let mut bad = rec("k", 2, b"two");
    bad.content_hash = content_hash(b"not two");
    assert!(matches!(
        s.commit("ws", &commit(bad, None)),
        Err(Error::Integrity(_))
    ));
}

#[test]
fn yjs_clock_cas_and_op_log() {
    let (_d, s) = store();
    let r = s
        .commit(
            "ws",
            &commit(yrec("jobs", 1, 1, b"snap1"), Some(op(1, b"u1"))),
        )
        .unwrap();
    assert_eq!(r.clock, Some(1));
    s.commit(
        "ws",
        &commit(yrec("jobs", 2, 2, b"snap2"), Some(op(2, b"u2"))),
    )
    .unwrap();
    // Right revision, stale clock.
    let stale = s.commit(
        "ws",
        &commit(yrec("jobs", 3, 2, b"snap3"), Some(op(2, b"x"))),
    );
    assert!(matches!(
        stale,
        Err(Error::Conflict {
            expected: 1,
            actual: 2,
            ..
        })
    ));
    // An op that is not of the record's clock, or missing, is malformed.
    assert!(matches!(
        s.commit("ws", &commit(yrec("jobs", 3, 3, b"s"), Some(op(9, b"x")))),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        s.commit("ws", &commit(yrec("jobs", 3, 3, b"s"), None)),
        Err(Error::Invalid(_))
    ));
    // A Yjs document does not lose its clock.
    assert!(matches!(
        s.commit("ws", &commit(rec("jobs", 3, b"s"), None)),
        Err(Error::Invalid(_))
    ));
    let ops = s.ops_since("ws", "jobs", 0).unwrap();
    assert_eq!(ops, vec![op(1, b"u1"), op(2, b"u2")]);
    assert_eq!(s.ops_since("ws", "jobs", 1).unwrap(), vec![op(2, b"u2")]);
    assert_eq!(s.get("ws", "jobs").unwrap().unwrap().yjs.unwrap().clock, 2);
}

#[test]
fn retention_keeps_the_last_revisions() {
    let (_d, s) = store();
    let s = s.with_retention(2);
    for rev in 1..=5u64 {
        s.commit(
            "ws",
            &commit(rec("k", rev, format!("v{rev}").as_bytes()), None),
        )
        .unwrap();
    }
    let kept: Vec<u64> = s
        .retained("ws", "k")
        .unwrap()
        .iter()
        .map(|r| r.revision)
        .collect();
    assert_eq!(kept, vec![3, 4]);
}

#[test]
fn corruption_is_detected() {
    let (d, s) = store();
    s.commit("ws", &commit(rec("k", 1, b"good"), None)).unwrap();
    std::fs::write(d.path().join("workspaces/ws/k/snapshot.bin"), b"evil").unwrap();
    assert!(matches!(s.get("ws", "k"), Err(Error::Integrity(_))));
    s.commit("ws", &commit(yrec("y", 1, 1, b"s"), Some(op(1, b"u"))))
        .unwrap();
    std::fs::write(
        d.path()
            .join("workspaces/ws/y/ops/00000000000000000001.bin"),
        b"tampered",
    )
    .unwrap();
    assert!(matches!(
        s.ops_since("ws", "y", 0),
        Err(Error::Integrity(_))
    ));
}

#[test]
fn an_interrupted_commit_is_finished_not_lost() {
    let (d, s) = store();
    s.commit("ws", &commit(rec("k", 1, b"one"), None)).unwrap();
    // The crash left a complete intent and nothing else of the second commit.
    let pending = commit(rec("k", 2, b"two"), None);
    let intent = format!(
        r#"{{"commit":{},"seq":2}}"#,
        serde_json::to_string(&pending).unwrap()
    );
    std::fs::create_dir_all(d.path().join(".cvfs/intent")).unwrap();
    std::fs::write(d.path().join(".cvfs/intent/ws.json"), intent).unwrap();
    assert_eq!(s.get("ws", "k").unwrap().unwrap().bytes, b"two");
    assert_eq!(s.receipts("ws", 0).unwrap().len(), 2);
    assert_eq!(s.retained("ws", "k").unwrap(), vec![rec("k", 1, b"one")]);
    // A torn intent (cut mid-write) was never applied and is dropped.
    std::fs::write(d.path().join(".cvfs/intent/ws.json"), b"{\"commit\":{\"rec").unwrap();
    assert_eq!(s.get("ws", "k").unwrap().unwrap().revision, 2);
    assert!(!d.path().join(".cvfs/intent/ws.json").exists());
}

#[test]
fn names_are_safe_and_distinct() {
    let (d, s) = store();
    for key in ["Cv", "cv", "con", "a/b", "..", "x:y", "tailing.", "ünï"] {
        s.commit("ws", &commit(rec(key, 1, key.as_bytes()), None))
            .unwrap();
    }
    let keys = s.keys("ws").unwrap();
    assert_eq!(keys.len(), 8);
    assert_eq!(s.get("ws", "Cv").unwrap().unwrap().bytes, b"Cv");
    assert_eq!(s.get("ws", "cv").unwrap().unwrap().bytes, b"cv");
    // Nothing escaped the workspace directory, and no name is a Windows device or a path.
    for e in std::fs::read_dir(d.path().join("workspaces/ws")).unwrap() {
        let name = e.unwrap().file_name().into_string().unwrap();
        assert!(!name.contains('/') && name != ".." && name != "con" && name != "Cv");
    }
    assert!(matches!(
        s.commit("", &commit(rec("k", 1, b"x"), None)),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        s.commit("ws", &commit(rec("", 1, b"x"), None)),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn assets_are_ordinary_files_with_safe_paths() {
    let (d, s) = store();
    s.put_asset("ws", "img/photo.png", b"\x89PNG").unwrap();
    assert_eq!(
        s.get_asset("ws", "img/photo.png").unwrap().unwrap(),
        b"\x89PNG"
    );
    assert_eq!(
        std::fs::read(d.path().join("workspaces/ws/_assets/img/photo.png")).unwrap(),
        b"\x89PNG"
    );
    assert_eq!(s.get_asset("ws", "img/none.png").unwrap(), None);
    for bad in ["../x", "/abs", "a//b", "a\\b", "nul.txt", "x/", "c:evil"] {
        assert!(
            matches!(s.put_asset("ws", bad, b"x"), Err(Error::Invalid(_))),
            "{bad}"
        );
    }
}

#[test]
fn dump_restore_is_lossless() {
    let (_d, s) = store();
    s.commit("ws", &commit(rec("a", 1, b"a1"), None)).unwrap();
    s.commit("ws", &commit(rec("a", 2, b"a2"), None)).unwrap();
    s.commit("ws", &commit(yrec("jobs", 1, 1, b"s1"), Some(op(1, b"u1"))))
        .unwrap();
    s.put_asset("ws", "f.bin", b"\x00\x01").unwrap();
    let dump = s.dump("ws").unwrap();
    assert_eq!(dump.records.len(), 2);
    assert_eq!(dump.receipts.len(), 3);
    let (_d2, t) = store();
    t.restore("ws", &dump).unwrap();
    assert_eq!(t.dump("ws").unwrap(), dump);
    // A workspace that holds anything refuses a restore.
    assert!(matches!(t.restore("ws", &dump), Err(Error::Invalid(_))));
    // Commits continue from the restored state.
    let next = t.commit("ws", &commit(rec("a", 3, b"a3"), None)).unwrap();
    assert_eq!(next.seq, 4);
}

#[test]
fn concurrent_writers_see_exactly_one_winner_per_revision() {
    let (d, _s) = store();
    let root = d.path().to_path_buf();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let root = root.clone();
            std::thread::spawn(move || {
                let s = FsStore::open(root).unwrap();
                s.commit("ws", &commit(rec("k", 1, format!("w{i}").as_bytes()), None))
                    .is_ok()
            })
        })
        .collect();
    let wins = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|w| *w)
        .count();
    assert_eq!(wins, 1);
}
