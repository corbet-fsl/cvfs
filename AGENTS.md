# Agent instructions

`cvfs` is a small filesystem storage backend. Keep it that way.

- It never interprets document bytes, metadata, TOML, Typst or Yjs. It stores, hashes, orders and
  receipts.
- `std` plus `serde`/`sha2` only. No platform-specific calls; it must behave the same on Linux,
  macOS and Windows.
- Every commit step must be safe to repeat (the intent is replayed after a crash).
- Never depend on `cstore`; ideas only.
- Comments and documentation are English.

Checks (CI runs the same): `cargo fmt --all -- --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`.
