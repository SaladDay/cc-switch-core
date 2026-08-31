# CC Switch Core

Small, reusable Rust primitives shared by CC Switch applications.

The `0.1` API contains:

- built-in application identifiers and their existing serialization behavior;
- a built-in application registry with product-neutral metadata and capabilities;
- sealed built-in adapter and live-operation plan contracts;
- source-neutral, lossless provider snapshots;
- redacted provider entries for additive application configs;
- MCP application contracts, validation, import, and loss-aware projection;
- deterministic JSON and atomic file-writing primitives;
- host-neutral compare-and-swap execution with guarded rollback;
- pure live projections and validation for all nine supported applications.

The live-operation layer does not own paths, raw-plan syntax validation,
concrete file I/O, locks, business state, databases, UI, OAuth flows, proxy
behavior, catalog generation, or the plugin system. Hosts supply stable
resource identities, bounded reads, and conditional single-target replacement
relative to their documented synchronization primitive; Core supplies
structural plan validation, compare-and-swap sequencing, and guarded rollback.
Ordinary filesystems cannot exclude non-cooperating writers between comparison
and replacement, so each host must document that platform limit. The crate's
separate `fs` module remains the small shared file-writing primitive layer.

The API may change before `1.0.0`.
