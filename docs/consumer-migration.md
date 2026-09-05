# Consumer migration acceptance

The shared Core/Store dependency alone does not complete a consumer migration.
Each shared behavior needs a production caller, compatibility tests, and removal
of the implementation it replaces.

## Scope

- Work on Core, Lite, and the CLI migration branch only. Do not change the full
  CC Switch repository or submit PRs there. Do not push the CLI main branch.
- Core owns reusable transformations and contracts. Products own UI, paths,
  process/network I/O, and policy choices. Preserve existing product behavior.
- Review and verify each small change before proceeding. Do not combine provider,
  MCP, and Skill rewrites into one change set.
- Use isolated test data and remove generated build caches after validation.
- Apply the [full-product readiness plan](full-product-readiness.md) to each Core
  change. CLI/Lite acceptance must not narrow the contracts needed by the full
  desktop product, whose actual migration still requires separate authorization.

## Stages

1. **MCP:** share native entry conversion and document handling. Preserve catalog
   field selection, native extensions, enablement, and each host's synchronization
   rules. Start with OpenCode/Hermes entry conversion, then Codex/Gemini/Claude.
   This stage does not change provider configuration or Skill deployment.
2. **Providers:** move native import and projection behind Core adapters, one
   application at a time. Cover current modes, authentication, unknown fields,
   multi-file failure behavior, and proxy-related host policies. Do not remove
   advanced CLI features to fit a Lite-only interface.
3. **Skills:** share portable observation, reference/configuration, and deployment
   logic where behavior agrees. Retain host storage migration and download policy.
   Test unowned destinations, stale state, and failure recovery before deleting
   old implementations.
4. **Architecture acceptance:** trace real CLI and Lite calls against the registry,
   adapters, capability declarations, shared infrastructure, and per-app conformance
   tests. Audit remaining app-specific matches and model-fetch declarations. Record
   intentional product boundaries separately from unfinished shared behavior.

## Blind review gate

Two fresh reviewers independently inspect the entire change set, including tests.
Give each the goal, expected behavior, acceptance criteria, and scope, but no
implementation explanation or earlier findings. Confirm reported defects before
fixing them, then repeat with fresh reviewers. Reduce to one fresh reviewer only
when fixes have converged to small refinements. Repeated repair cycles require a
design reassessment; report an unresolved design instead of extending scope.

## Current acceptance status

Provider/MCP/Skill catalog storage already uses Store in the CLI. Native provider,
MCP, and Skill behavior is not yet fully migrated. OpenCode/Hermes MCP entry
conversion and Codex MCP entry projection have passed compatibility tests and two
independent reviews per change. Remaining MCP conversion and document handling
are not complete. Gemini entry conversion now has explicit policy boundaries for
the consumers' different timeout precedence and import tolerance.

Provider migration starts with Codex auth observation. The CLI retains opaque
nonempty snapshot payloads, including timestamps; Lite uses credential-aware
checks. Shared observations must support both without changing either policy.
Auth observation has passed compatibility tests and independent double review.
Codex bearer-token routing has also passed compatibility tests and double review.
Shared reads and writes retain each host's accepted TOML table syntax, token
precedence, and error behavior.

CLI Codex two-file auth/config writes now use Core execution. The CLI
keeps path resolution, permission policy, accepted JSON/TOML syntax, and native
file sizes. Core owns conditional writes and guarded recovery; the CLI must not
repeat an unconditional snapshot restore after a Core-managed write failure.
Single-file replacement stays host-owned and does not acquire a read-permission
requirement. Conditional Windows replacements must not delete the old file before
publishing its replacement; Core's existing atomic file writer provides that primitive.
Model-catalog publication and rollback after later MCP/snapshot work remain host
workflows, not part of this slice. The default execution and wire size limits
remain unchanged; trusted local plans may use an explicit host-selected bound.
This connection passed compatibility tests, double-review rounds, and a final
independent review after the fixes converged.

The shared Codex credential codec covers API-key selection,
third-party auth sanitization, token projection/removal, and snapshot backfill.
The CLI retains its TOML grammar checks, localized errors, and decisions about
when to write auth or capture a snapshot. The default Core projection keeps its
existing inline-table support. Model catalogs, session-history rewriting, and
proxy strategy are outside this slice.
This connection passed compatibility tests and two independent blind reviews.

Codex native import now uses the registered adapter. Hosts
choose validation responsibility, document-presence rules, and authentication
classification explicitly. The default import remains strict. A host-validated
snapshot preserves the host's accepted JSON shape and TOML grammar; paths, parser
diagnostics, provider names, model-catalog loading, and session-history policy
remain in that host. Local document inventories can use an explicit content
bound without changing default snapshot or operation-plan limits.
This connection passed compatibility tests and two independent blind reviews.

Codex MCP entry import now shares tolerant transport decoding. Its explicit
field-selection policy does not replace the default structural codec or strict
document import. Hosts retain parser diagnostics, shallow extension selection,
collection tolerance, legacy-section precedence, catalog metadata, and enablement.
The CLI production import uses the shared codec and removes the replaced
transport conversion. Compatibility tests, two independent blind reviews, and
a final independent review after a test-only refinement passed.

Gemini MCP now shares entry import and export in the CLI's real
`read_mcp_servers_map` / `set_mcp_servers_map` calls. The explicit policies retain
unconsumed fields, string-based type inference, and saturating seconds-first
timeouts. Default Core/Lite codecs keep their existing rules. Parsing, document
bounds, non-object entry tolerance, wrapper/metadata selection, catalog enablement,
and file publication remain host-owned. No provider, proxy, Skill, or UI behavior
belongs in this slice.

The intended full-product boundary is the same native entry codec, independent
of simple provider forms. Rich synthetic entry fixtures test unconsumed fields;
they do not establish full-product parity. The full product's field selection
and timeout rules still require baseline verification before adoption. This is
an additive Rust API change, not a wire/schema, dependency, or Rust-version change.
Consumers can keep their previous pins without a data migration. Acceptance
passed baseline-derived CLI comparisons, real caller tests, unchanged default
codec conformance, and two independent blind reviews before publication.

Minimum MCP connection validation is shared. The CLI's production
`mcp::validate_server_spec`, used by the native import paths, delegates transport
and required-field checks to Core and keeps its localized errors. Core's strict
validator reuses those checks but still owns canonical field validation, IDs,
native-alias rejection, limits, and error order. Connection-only validation is
not permission to write a canonical configuration: hosts still select parsing,
catalog enablement, metadata, and projection policy. Legacy entry points with
different validation semantics are not changed.

The intended full-product boundary is the same connection check for rich native
entries, without narrowing them to a simple form. Synthetic extension fixtures
test that boundary, not full-product parity. No full-product caller was inspected
or migrated. This additive Rust API changes no defaults, wire/schema contract,
dependency versions, or MSRV. Acceptance passed baseline-derived CLI result and
localized-error comparisons, real import tests, strict-default conformance, and
two independent blind reviews. Consumer rollback reverts the integration and pin
together; no data migration is needed for this slice.

Codex native MCP document operations are the next slice. The CLI's bulk sync,
single-entry sync, and removal retain their distinct initialization, parsing,
legacy cleanup, logging, and file-publication rules. Core handles native table
replacement, tolerant upsert/removal, and explicitly requested legacy cleanup.
Entry field selection stays unchanged in the CLI; strict Core/Lite projection
defaults are not replaced by the tolerant native document API.

The intended full-product boundary accepts native TOML entries, including rich
unknown fields, without coupling callers to the editor's AST types. Core does
not choose paths, catalog enablement, parser-error presentation, document limits,
or transactions here. Display diagnostics can contain source; Debug is redacted.
Compatibility requires byte-for-byte comparisons against the previous CLI file
operations, including inline tables, malformed collections, skipped entries,
uninitialized apps, and large native files. Synthetic extension fixtures test
this contract, not full-product parity; that product has not been inspected or
integrated. This additive API changes no schema, wire contract, dependencies, or
MSRV. Lite retains its strict projection path. Consumers adopt reviewed pins;
rollback reverts caller changes and pins together without a data migration.

The rest of native provider projection/import and Skill deployment remain pending.
No stage above is marked complete yet.
