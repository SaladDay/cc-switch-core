# Planning for the full CC Switch consumer

Core is intended for the full desktop product as well as CLI and Lite. Passing
CLI/Lite tests is evidence for those consumers, not proof that the full product
can migrate unchanged. This plan adds a gate to every shared-code change; it does
not authorize changes, branches, or PRs in the full product repository.

## Ownership that must survive a third consumer

| Layer | Owns | Must not assume |
| --- | --- | --- |
| Core | App contracts, native codecs, provider snapshots, configuration plans, shared validation and guarded execution | A simple form represents every provider setting; a product's enabled features define an App's capabilities |
| Store | Shared catalog contracts and guarded row operations | Ownership of product schema versions, extension tables, or the entire switching transaction |
| Product host | Paths, settings/environment overrides, product feature availability, storage migrations, process/network I/O, workflow and UI | Updating the Core dependency alone replaces local shared logic or proves compatibility |

The simple-provider API is an optional convenience, not the only path into Core.
The full product must be able to retain native settings, advanced options,
authentication state, and unrelated fields without reducing them to Lite's form.
Unknown fields outside an operation's declared ownership must be preserved, or
the operation must fail before writing; each codec documents its selection rules.

Proxy activation, failover decisions, OAuth sessions, tray/UI events, downloads,
and scheduling remain host workflows. Reusable configuration transformations used
by those workflows may belong in Core even if Lite does not expose the feature.
Do not duplicate a native codec merely because one caller is a proxy workflow.
Future model-fetch protocol declarations, request planning and response decoding
belong in Core when shared; credential acquisition, HTTP execution and retries
remain with the host. These model-fetch contracts are not implemented yet.

Behavior differences require named, typed choices such as validation responsibility,
field selection, or native ownership. Do not introduce `is_lite`, `cli_mode`, or a
collection of product-specific flags. Keep current defaults unchanged. Add an
option only for a demonstrated behavior difference and test it independently;
reassess the abstraction if options merely preserve duplicate implementations.

## Four steps and their boundaries

1. **Record compatibility requirements before extending an API.** For each slice,
   identify its production CLI/Lite caller, intended full-product use, owned
   fields, errors, write order, and unresolved assumptions. Use existing approved
   evidence and synthetic fixtures now. After permission to inspect/migrate the
   full product, capture sanitized baseline fixtures there. Do not label synthetic
   examples as full-product parity tests or infer behavior that was not checked.
2. **Implement the smallest shared contract.** Keep App behavior behind its
   registered integration; retain stable IDs and serialized shapes. Separate native
   snapshots/plans from simple forms and product feature selection. Replace one
   real caller at a time and remove its replaced production implementation. Do not
   change UI, proxy behavior, schema policy, or unsupported Apps as a shortcut.
3. **Validate a third-consumer contract before claiming readiness.** Exercise the
   same API with a host that is not constrained by Lite's feature set. Cover rich
   native settings, unknown fields, shared database extensions, explicit policies,
   conflicts, and failure recovery. Expected results must come from stated
   requirements or approved baseline fixtures, not from Core's own output. A
   synthetic host validates the contract only; full-product parity stays unverified.
4. **Migrate the full product only with separate authorization.** First compare
   read/import results, then migrate one App/capability on an isolated branch with
   baseline tests. Run only one production writer for a resource; do not dual-write
   old and new paths. Preserve host workflows and remove the replaced implementation
   after parity is established. Opening a PR requires the user's explicit approval.

Each implemented slice, including a planning-only slice, uses the
[blind review gate](consumer-migration.md#blind-review-gate). Reviewers receive the
requirements and boundaries, not implementation explanations or earlier findings.

## Required acceptance cases

- **Native data:** optional/malformed documents, host-accepted syntax and sizes,
  opaque authentication, unknown fields, and multiple native targets. A field-only
  codec must not silently become a whole-document validation or import policy.
- **Execution:** stable resource identities, shared locking boundaries, stale reads,
  partial writes, guarded rollback and recovery after later host work fails. The
  host must define database/file commit and compensation order; Core does not make
  SQLite and filesystem changes one atomic transaction. Publish product events only
  after the corresponding host operation succeeds.
- **Storage coexistence:** old/new product versions using the same database,
  unknown columns and Apps, triggers, foreign keys, malformed legacy rows, and
  concurrent updates. Each shared-schema change needs an old-reader/writer and
  rollback assessment. Do not downgrade a schema or discard another product's data
  to fit a consumer; reject unsupported contracts before mutation.
- **Capabilities:** Core declares App support; products select what they expose.
  A disabled Lite feature must not remove the underlying App contract. Extending
  an App requires registry-wide conformance tests, not another product-shaped registry.
- **Release compatibility:** assess the Core/Store API, serialized operation contract,
  Rust requirement, dependencies and Windows/macOS/Linux behavior. Core currently
  declares Rust 1.85.0; the full product's compatibility has not been verified.

## Per-change record and completion language

Record these items in each shared-code PR or direct-main commit's review record:

- The common behavior and actual consumer call sites being replaced.
- The intended full-product call boundary, host-owned decisions, and assumptions
  still awaiting validation. A potential third consumer is not enough reason to add
  an unused abstraction.
- API/default/wire/schema impacts, compatibility fixtures, both blind reviews, and
  any unresolved baseline issue left outside the change.
- Exact Core/Store revisions tested by consumers and a compatible rollback path.

One implementation does not imply automatic updates: pinned consumers adopt a
reviewed revision and run their own tests. Breaking changes need a stated migration
path; retaining a library pin alone is not a database rollback strategy.

Track three separate states: **implemented in Core**, **verified by a real consumer**,
and **verified in the full product**. Current CLI native provider/MCP/Skill migration
is still incomplete; no full-product migration has been validated by this plan.
