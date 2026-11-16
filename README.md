# cvfs

The cross-platform filesystem backend of the CareerVector workspace (`cvws`). It stores opaque
documents and Yjs op logs under one root and never reads their content.

- Conditional writes: a commit builds on the stored revision (and, for a Yjs document, the stored
  clock); a stale commit is refused with `Error::Conflict` and changes nothing.
- Integrity: every record carries `sha256:` of its bytes; writes and reads verify it.
- Crash safety: a commit is written ahead as an intent and finished on the next access.
- Receipts with a per-workspace sequence number, retained earlier revisions, assets as ordinary files.
- `dump` / `restore` move a whole workspace between backends without loss.
- `std` only; Linux, macOS and Windows.

Layout:

```text
<root>/workspaces/<workspace>/<key>/snapshot.bin     authored bytes
                                   /meta.json        envelope without the bytes
                                   /ops/<clock>.bin  Yjs updates (+ <clock>.json)
<root>/workspaces/<workspace>/_assets/<path>         assets
<root>/.cvfs/                                        locks, intents, receipts, retained snapshots
```

Names are escaped to portable file names (lowercase, digits, `-`, inner `.`; everything else `%XX`).

Licence: FSL-1.1-ALv2.
