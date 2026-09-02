# CC Switch Core

Small, reusable Rust primitives shared by CC Switch applications.

The `cc-switch-core` 0.1 API contains:

- built-in application identifiers and their existing serialization behavior;
- a built-in application registry with product-neutral metadata and capabilities;
- sealed built-in adapter and live-operation plan contracts;
- source-neutral, lossless provider snapshots;
- redacted provider entries for additive application configs;
- MCP application contracts, validation, import, and loss-aware projection;
- catalog-authoritative installed Skill snapshots and owner-checked switching;
- deterministic JSON and atomic file-writing primitives;
- host-neutral compare-and-swap execution with guarded rollback;
- pure live projections and validation for all nine supported applications.

## Integration model

Built-in applications have one composition point: the internal
`AppIntegration` catalog. Each row binds the public descriptor, logical
targets, simple provider form, native import, and native projection behavior.
App-specific parsers and projectors stay in their capability modules; shared
entry points dispatch through that catalog. `AppType` and `LogicalTarget`
remain stable wire contracts, so their exhaustive decoding, ownership, and
cross-capability checks still need deliberate updates for a new App. Do not
add a second App-shaped registry or a parallel behavior dispatcher.

Conformance tests iterate every registered adapter, form, capability, and
logical target. A new built-in App is complete only when it passes the same
suite; products should consume the public registry and adapter APIs instead of
copying native settings templates.

## Dependency direction

Core owns product-neutral contracts and pure transformations. Native hosts
discover application config paths and apply user settings, environment
variables, platform directories, and installation checks. Core may
canonicalize and validate paths supplied to its Skill and filesystem
contracts. Shared SQLite schema and migrations should live in a separate
storage crate that depends on Core, so desktop and CLI products can share
persistence without making Core database-specific. Core may expose stable
display-name fallbacks, brand keys, simple forms, presets, and authentication
modes needed by pure projections. Hosts own localized UI copy, components,
assets, styles, process orchestration, OAuth flows and token acquisition,
network model discovery, proxying, and plugin installation.

The workspace also contains `cc-switch-store`. This separate crate owns the
shared SQLite connection and an explicit initializer for the canonical
`providers` table contract. Hosts can run product migrations before opting into
that contract. The crate does not own product CRUD policy, host extension
tables, product schema versions, or database path discovery. The shared
contract validates its columns and binary composite identity; additional host
constraints, indexes, triggers, and conflict policies remain host-owned.

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
