# CC Switch Core

Small, reusable Rust primitives shared by CC Switch applications.

The `0.1` API contains:

- built-in application identifiers and their existing serialization behavior;
- a built-in application registry with product-neutral metadata and capabilities;
- sealed built-in adapter and live-operation plan contracts;
- source-neutral, lossless provider snapshots;
- redacted provider entries for additive application configs;
- deterministic JSON and atomic file-writing primitives;
- pure live projections and validation for all nine supported applications.

The live-projection layer does not own paths, operation execution, file I/O, locks, rollback,
business state, databases, UI, OAuth flows, proxy behavior, catalog generation,
or the plugin system. The crate's separate `fs` module remains the small shared
file-writing primitive layer.

The API may change before `1.0.0`.
