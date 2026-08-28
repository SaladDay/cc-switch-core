# CC Switch Core

Small, reusable Rust primitives shared by CC Switch applications.

The `0.1` API contains:

- built-in application identifiers and their existing serialization behavior;
- source-neutral, lossless provider snapshots;
- deterministic JSON and atomic file-writing primitives;
- a Claude live-settings projection;
- preparation of the values consumed by the Codex live-write pipeline.

Business state, databases, UI, provider-specific TOML migrations, OAuth flows,
catalog generation, and the plugin system remain outside this crate.

The API may change before `1.0.0`.
