# Provider and MCP transaction composition

This is step 2 of the CLI's provider-owned MCP migration. Step 1 added real
provider/Lite acceptance gates on CLI `604313d2`; those gates remain failing
until step 3 adopts this contract. No production CLI or Lite caller is changed
here. Both currently retain their previous Core/Store pins.

## Shared contract

`McpTransactionGuard::from_provider_transaction` takes a caller's transaction
before native publication. It captures the complete provider, MCP catalog and
native-link tables, including unknown columns and App IDs. Provider settings and
selection writes use the existing guarded Store primitives. A suppressed or stale
provider write poisons the composite even if its `NotApplied` result is ignored.
Checks before later writes and at final commit reject cross-table trigger effects.
Standalone `McpTransactionGuard::begin` retains its MCP-only contract and does not
require the provider table.

The guard does not choose provider settings, selection order or native file paths.
Raw provider settings remain opaque; they are not restricted to Lite's form.
Hosts read their extension tables before transferring the transaction. There is
no raw transaction escape or arbitrary SQL callback after transfer. Post-transfer
extension-table writes need a separately justified contract, not an unchecked
escape added for a hypothetical consumer.

Use an immediate transaction for cooperating writers. Construction failure rolls
back the whole transferred transaction. The host holds its native lock and all
Core receipts until its final decision. On a later host failure, restore receipts
in reverse publication order, then roll back the database. On commit failure,
`commit_preserving_on_error` returns the guard so native recovery can happen before
database rollback. If SQLite itself aborted, the host cannot assume the database
lock remains held. Recovery stays conditional and must preserve external edits.
This is not crash-atomic filesystem/SQLite execution.

## Consumer boundaries and acceptance

The real next caller is CLI `ProviderService::switch_gemini_coordinated`, whose
MCP tail currently lacks shared removal snapshots and all-App recovery. It will
transfer its transaction before writing files, retain host auth and Skill-tail
policy, and use one final decision for all owned effects. Step 3 must pass the
three existing real-entry gates and delete the replaced per-entry path.

Lite can use the same composite when a workflow needs both provider and MCP
ownership; its independent MCP commands keep their existing API. A future full
desktop host can retain rich provider values, unknown Apps and database columns.
The synthetic consumer test combines real SQLite, a real shared lock and Core
receipts with in-memory native I/O. It checks the contract, not full-product
native-format parity or real CLI/Lite adoption. No full-product source is accessed.

Acceptance covers successful interleaving and later provider writes, unknown
fields, stale/suppressed writes, trigger side effects in both directions, aborted
transactions, constructor/read failure, deferred commit failure, later host
failure, reverse native recovery and preservation of external changes. Existing
MCP tests must also pass. Each change undergoes independent blind review under
the [consumer review gate](consumer-migration.md#blind-review-gate).

This additive Rust API changes no schema, wire format, dependency or MSRV.
Core/Store remain Rust 1.85.0. Consumers can keep their old pins; adopting or
reverting the API requires changing caller code and pins together, with no data
migration. Proxy workflows, UI, product schema ownership, authentication choices,
new App support and full-product migration remain outside this step.
