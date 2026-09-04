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

Each built-in application owns one `AppIntegration` declaration in its App
module. The declaration binds its public descriptor, logical targets, simple
provider form, native import, and native projection behavior. The central
catalog contains only ordered references to those declarations. App-specific
parsers and projectors stay in their capability modules; shared entry points
dispatch through the catalog. `AppType` and `LogicalTarget` remain stable wire
contracts, so their exhaustive decoding, ownership, and cross-capability
checks still need deliberate updates for a new App. Do not add a second
App-shaped registry or a parallel behavior dispatcher.

Conformance tests iterate every registered adapter, form, capability, and
logical target. A new built-in App is complete only when it passes the same
suite; products should consume the public registry and adapter APIs instead of
copying native settings templates.

## Dependency direction

Core owns product-neutral contracts and pure transformations. The app registry
declares the common default config root and whether native resources are
relative to it or need host-defined platform handling. Native hosts apply
settings and environment overrides, resolve home and platform directories,
check installations, and perform file I/O. Core may canonicalize and validate
paths supplied to its Skill and filesystem contracts. Shared SQLite schema and migrations should live in a separate
storage crate that depends on Core, so desktop and CLI products can share
persistence without making Core database-specific. Core may expose stable
display-name fallbacks, brand keys, simple forms, presets, and authentication
modes needed by pure projections. Hosts own localized UI copy, components,
assets, styles, process orchestration, OAuth flows and token acquisition,
network model discovery, proxying, and plugin installation.

The workspace also contains `cc-switch-store`. This separate crate owns the
shared SQLite connection and explicit contracts for the canonical `providers`,
`mcp_servers`, MCP native ownership links, and installed `skills` tables. Hosts
can run product migrations before opting into those contracts. The crate does
not own product CRUD policy, host extension tables, product schema versions, or
database path discovery.

Provider rows expose a fingerprint covering canonical and unknown host columns.
The `*_if_unchanged` writes use it for compare-and-swap updates while preserving
host fields. `insert_provider_if_absent` and strict deletes additionally reject
trigger or foreign-key side effects, including same-value row replacement. The
original insert and delete primitives remain available for hosts whose
compatibility contract requires dependent-row cleanup.
`delete_provider_with_host_cleanup_if_unchanged` is the cross-product delete:
it keeps the complete-row compare-and-swap and permits cleanup outside the
provider catalog while preventing any write to another provider row.

The MCP catalog has a narrower opt-in write contract. Hosts may add columns,
indexes, and triggers, but table constraints must retain SQLite's default
`ABORT` conflict handling. Trigger bodies keep their own conflict policies and
may maintain host columns or host-owned tables. They must not replace a target
row or mutate other MCP catalog rows. Shared writes detect and roll back a
suppressed write or a changed public target state; a same-value replacement is
not observable and therefore remains a host contract violation. Callers own an
immediate transaction and any other product-specific MCP state. Read-only
consumers do not need to opt into this write contract.

Per-application MCP selection columns come from the Core registry. New hosts
should use the registry-complete catalog values; fixed-field Store APIs remain
available only for compatibility with existing callers.

MCP native links record which application owns a catalog entry and retain an
optional lossless import snapshot. Core validates the link table and installs a
single cleanup trigger so deleting a catalog row cannot leave a stale snapshot.

Skill catalog writes accept only the catalog part of a Core `SkillSwitchPlan`.
They compare-and-swap one registry-declared selection while leaving product
metadata and unknown columns untouched. A trigger may maintain independent host
data, but must not rewrite catalog rows, their host fields, or tables connected
to the catalog through foreign keys. Suppressed or unexpected writes are rolled
back; independent audit-table writes remain supported. Callers retain the
immediate transaction and live file work.

Hosts with their own live deployment policy can instead use the lower-level
Skill catalog row APIs. They read every registry selection, insert complete
catalog rows, replace selections with a complete-row compare-and-swap, and
delete rows while allowing host cleanup outside the Skill catalog. These APIs
keep raw snapshots opaque and expose typed values only for storage-valid rows,
so malformed legacy data remains removable without leaking host fields through
`Debug`. A newly created catalog contains only shared base and registry columns;
each product owns its metadata schema. The guarded host-field update primitive
accepts those product-owned columns without embedding them in Core and rejects
trigger rewrites outside the requested row and fields. These APIs do not own
paths, copy/symlink policy, or filesystem rollback.

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
