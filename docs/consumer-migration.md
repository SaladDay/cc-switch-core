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

Codex native MCP document operations are shared. The CLI's bulk sync,
single-entry sync, and removal retain their distinct initialization, parsing,
legacy cleanup, logging, and file-publication rules. Core handles native table
replacement, tolerant upsert/removal, and explicitly requested legacy cleanup.
Entry field selection stays unchanged in the CLI; strict Core/Lite projection
defaults are not replaced by the tolerant native document API.

The intended full-product boundary accepts native TOML entries and incremental
native values, including rich unknown fields, without coupling callers to the
editor's AST types. Prepared entries keep their in-memory field order until
publication. Hosts retain their own accepted grammar and diagnostics. Core does
not choose paths, catalog enablement, parser-error presentation, document limits,
or transactions here. Display diagnostics can contain source; Debug is redacted.
Compatibility requires byte-for-byte comparisons against the previous CLI file
operations, including inline tables, malformed collections, skipped entries,
uninitialized apps, and large native files. Synthetic extension fixtures test
this contract, not full-product parity; that product has not been inspected or
integrated. This additive API changes no schema, wire contract, dependencies, or
MSRV. Lite retains its strict projection path. Consumers adopt reviewed pins;
rollback reverts caller changes and pins together without a data migration.

This connection passed byte-for-byte CLI baseline comparisons, Core/Lite
regressions, two fresh independent reviews after the final correction, and Core
multi-platform and Lite CI. Core revision `0ed213d`, CLI local commit `0593724d`,
and Lite PR #39 record the accepted versions. The CLI branch was not pushed.

Claude model-key migration and live metadata cleanup are shared.
Core owns these in-memory transformations on native JSON. The CLI retains
the existing import, backfill, effective-settings, temporary-launch and live-write
call sites, including when each transformation runs. Model-key migration is
explicit: Core/Lite's default import and projection do not start applying it.
Existing role values, malformed shapes, string whitespace, unknown fields, output
key order and the changed result must match the previous CLI implementation.

The intended full-product boundary is native settings, not a simple form. Rich
synthetic fixtures cover permissions, hooks and opaque authentication alongside
these transformations; full-product call order and compatibility remain unverified.
Paths, parsing, common-config selection, proxy/auth workflows, persistence and
transactions stay with the host. No new registry or product-mode flags are needed
for these Claude-specific operations. Acceptance requires baseline comparisons,
real provider/temporary-launch tests, unchanged default codecs, and double review.
The additive Rust API changes no defaults, wire/schema, dependencies or MSRV.
Rollback reverts CLI delegates and pins together; no data migration is introduced.

This connection passed baseline comparisons, real caller tests, two independent
blind reviews, and Core/Lite CI. Accepted versions are Core `23f2ed5`, CLI local
commit `739ac4a3` (not pushed), and Lite PR #40.

## Model-fetch slices

1. Share declarative endpoint candidates, key-header templates and response
   alternatives through `ModelFetchSpec`. Replace the real CLI/TUI HTTP caller's
   transformations, retaining the host's HTTP execution and errors. No new UI,
   network retry, OAuth session or pagination behavior belongs in this slice.
2. Connect application defaults to the existing registry and retain explicit
   provider-protocol overrides. An App's default must not restrict its supported
   provider protocols or reflect Lite's feature selection. Do not introduce a
   second App registry or infer support for unverified native providers.
3. Trace actual consumers and run registry/protocol conformance and compatibility
   tests. Separate remaining host policy from unfinished shared behavior; remove
   replaced production code only after real callers use Core.

Each slice uses the independent double-blind gate. The first passed baseline
comparisons, real CLI/TUI tests, both independent reviews and Core/Lite CI: Core
`a09204b`, CLI local `e95f36de` (not pushed), and Lite PR #41. The second passed
compatibility tests, two independent reviews and Core/Lite CI: Core `4c982ee`,
CLI local `875854da` (not pushed), and Lite PR #42. The third is in progress;
overall model-fetch acceptance is not complete.

The real caller is `cli::tui::fetch_provider_models_for_tui`, also used by CLI
`provider_inspect::fetch_models_from_source`. The older
`ProviderService::fetch_provider_models` has no in-repository caller, but remains
a public API. Its request behavior is not the TUI migration baseline. Tests must
compare candidate order, full-URL derivation,
malformed inputs, response precedence, untrimmed/empty IDs, stable deduplication,
key-header order, custom-header interaction and host errors against that baseline.
Local HTTP fixtures must exercise the production path, not just Core helpers.

The full-product boundary is a static protocol specification and native JSON
response, not a simple provider form. A decoder only selects IDs; the host keeps
the original response for rich metadata and pagination. Custom declaration tests
verify this contract, not full-product parity. Credentials, provider selection,
custom headers, URL/header validation, request limits, timeouts, retries and
network publication stay host-owned. Candidate URLs are not authorization to send
credentials, and template expansion does not validate header contents.

The [Claude model-list reference](https://platform.claude.com/docs/en/api/models/list)
and [Gemini model-list reference](https://ai.google.dev/api/models) describe their
native response fields and pagination. Existing compatibility-root fallbacks,
dual Anthropic authentication headers and cross-format response fallback come
from the CLI baseline, not canonical API requirements. Preserve these behaviors
explicitly rather than applying new protocol defaults during migration.

The first slice adds Rust APIs without changing existing defaults, wire/schema,
dependency versions or MSRV. Lite adopts the reviewed pin without exposing a new
feature. Rollback restores CLI transformations and pins together, or Lite's pins;
no data migration is introduced. HTTP and full-product compatibility outside the
baseline fixture scope remain unverified.

The second slice declares optional model-fetch defaults in the existing App
descriptors. The seven CLI Apps retain their established defaults. Claude Desktop
and GrokBuild have no verified common default yet; absence does not mean the App
cannot fetch models. No capability flag, serialized descriptor field, second
registry or product-mode flag is added.

The CLI one-off and saved-provider paths consume those defaults. TUI requests
carry the form's App identity independently of shared field names, retaining
explicit protocol overrides. Core specs reach the
HTTP executor directly, without conversion back into a consumer protocol enum.
Claude bearer-only providers, Gemini access tokens, Pi custom authentication,
OAuth dispatch and CLI flag parsing stay host-owned and retain their behavior.
The CLI's generic fallback for a missing default remains a host choice.

Acceptance covers every registered default and unchanged descriptor payloads,
CLI/TUI default and override matrices, form-to-request App identity, existing
provider-auth tests, independent baseline HTTP comparisons, and a custom
declaration through the real HTTP executor. This is a product-neutral contract
for the future full consumer, not
evidence of that consumer's defaults or compatibility. The API is additive;
wire/schema, dependency versions and MSRV stay unchanged. Lite only adopts the
pin. Rollback restores CLI callers and pins together; no data migration occurs.

The third slice checks the actual worker request/HTTP/result channels, including
App identity independent of destination fields, protocol overrides, response
correlation and host errors. Synthetic requests check this channel contract;
they do not establish full-product parity or test OAuth credential acquisition.

Response decoding is also available without a request specification. The old
public `ProviderService::fetch_provider_models` uses that decoder, removing its
duplicate response traversal. Its signature, URL candidates, optional/empty-key
handling, two authentication headers and localized errors remain host-owned and
unchanged. Adopting a built-in request spec there would change existing behavior.
The main `ModelFetchSpec` method delegates to the same decoder, so the new entry
point does not create another implementation. Full-product consumers may likewise
reuse response rules while retaining their own requests and rich response data.

Acceptance requires real worker-channel tests, the old public API's loopback
response/header/error fixtures, existing baseline HTTP comparisons, Core/Lite
regressions and double review. The additive decoder API changes no defaults,
wire/schema, dependencies or MSRV. Lite only adopts the reviewed pin; rollback
reverts the CLI delegate and consumer pins without a data migration.

The model-fetch acceptance slice passed two independent blind reviews. Core
`4dafd0af` passed CI `34088602804`; Lite PR #43 adopted that revision and passed
exact-head CI `34088627022`. CLI acceptance is local commit `83b4d114` on the
migration branch, not a remote release. Whole-CLI strict Clippy still has existing
out-of-scope findings; no clean whole-CLI lint claim is made.

## Gemini native provider migration

1. **Read/import:** share assignment parsing and env-backed native snapshot import
   through the registered Gemini adapter. Replace CLI's live reader and initial
   provider import assembly. Preserve strict Core defaults, tolerant CLI grammar,
   localized errors, env-before-settings I/O, accepted JSON shapes and sizes,
   unknown settings, catalog classification and persistence order. Test against
   the previous reader, including both actual CLI entry points. Lite retains its
   strict default and only adopts the reviewed pin.
2. **Write planning:** compare the CLI's existing native write behavior with Core
   using isolated fixtures before replacing anything. Define exact field ownership,
   authentication selection, common-config preprocessing and malformed-input rules.
   Share reusable transformation only; keep product choices in the host. Do not
   narrow native snapshots to Lite's form or discard future/advanced settings.
3. **Execution acceptance:** establish file order, stale-read conflicts and recovery
   for the actual host write path, then replace it and remove the old implementation.
   Verify database/file compensation separately; shared execution is not an atomic
   SQLite/filesystem transaction. Each step requires fresh double blind review.

The read/import slice passed two independent blind reviews. Core `1153d83f`
passed CI `34091675162`; Lite PR #44 passed exact-head CI `34091762164` and merged.
CLI acceptance is local commit `f92fed11`, not a remote release. All 184
Gemini-related CLI tests pass with a canonical temporary directory. Two transcript
index tests fail under macOS's symlinked default temp path on both the unchanged
CLI baseline and the candidate, and pass with a canonical path; no transcript
code was changed.

That slice does not change file writes, auth workflows,
common configuration, proxy behavior, MCP/Skill operations, UI or shared schema.
The assignment codec exposes grammar, not product flags; callers choose whether
to reject invalid lines. Snapshot import has no credential classification and
does not imply write validity. The API is additive; dependency versions, wire
formats and MSRV are unchanged. Rollback restores CLI readers and consumer pins
together, with no data migration. Synthetic rich-settings fixtures check a future
consumer contract, not full-product compatibility. No full-product repository is
inspected or changed by this slice.

Step 2 shares string env-field selection and a typed top-level settings overlay.
The registered Core default planner and CLI preparation use the same primitives.
An overlay may select authentication before applying its owned fields; the strict
Core path continues to select authentication after applying the provider fields.
These are explicit operation stages, not product flags. Selecting auth in an
overlay owns its entire top-level `security` field; unrelated top-level fields
remain untouched. The CLI's existing preprocessing may supply retained native
security fields before that operation and is tested as part of the real path.

CLI's effective-snapshot builder and write preparation now share the overlay;
the env conversion wrapper shares field selection. Credential requirements,
sync gates, common-config policy, missing/non-object file handling and localized
diagnostics remain in the host. Native I/O order and execution are unchanged.
Independent copies of the prior preparation and effective-snapshot transformations
check values, errors and key order, alongside real force-write and sync tests.
Core's strict default is compared with its prior implementation across malformed
and rich native inputs. These fixtures do not establish full-product parity.
This additive API changes no dependency versions, MSRV, schema or wire contract.
Rollback restores the CLI delegates and pins together, without a data migration.
Execution acceptance remains step 3, not a claim made by this change.

The preparation slice passed two independent blind reviews. Core `9f7be30e`
passed three-platform CI `34095956872`; Lite PR #45 passed exact-head CI
`34096025123`, including its production build, and merged. CLI acceptance is
local commit `dc0b6ced`, not a remote release. Parent validation passed all 189
Gemini-related library tests and 71 provider-service tests. Existing whole-CLI
strict Clippy findings remain outside the changed code. Build targets were cleaned.

### Execution compatibility gate

Step 3 starts with CLI `dc0b6ced` execution baselines, not an executor replacement.
The real force-write and provider-switch entry points have different contracts:

| Boundary | Current CLI behavior | Consequence for shared execution |
| --- | --- | --- |
| Force write | Reads settings during preparation, but does not read the old env file | Requiring an env observation would reject previously accepted unreadable or non-UTF-8 files |
| Provider switch | Captures parsed env/settings before preparation and database publication | Retain exact observations at the existing read boundary; a parsed backup is not a byte-level precondition |
| Native publication | Writes env, then settings, then the host auth flag; no stale-content check | Core's conditional executor cannot be substituted without an explicit conflict-policy decision |
| Native or host-flag failure | Direct application leaves earlier writes; the switching transaction restores its database/configuration snapshot and native backup | One layer must own compensation; do not follow Core recovery with an unconditional native restore |
| Later MCP failure | Switch restores native values and provider selection, but retains the updated host auth flag | A native receipt does not cover every later host side effect |
| Native restore | Re-serializes parsed values and can overwrite subsequent edits | Exact-byte guarded rollback is a different contract, not an already verified behavior |

The baseline suite exercises these production paths without changing them.
It also checks replacement of leaf symlinks without writing their referents,
env/directory permissions, partial writes and edits between preparation/application
or application/restoration. These tests record existing limitations, not desirable
Core guarantees. They must change deliberately when an accepted execution design
changes that policy; do not weaken Core to preserve unsafe rollback.

Continue step 3 in this order:

1. Capture and verify the baseline above. Keep this change test/documentation-only;
   do not add a new planner policy, wire variant, writer or shared lock yet.
2. Define the observed-write boundary and receipt lifetime for the real switching
   transaction. Reuse Core's executor and retain host-owned path/security/error
   binding. Keep force-write I/O distinct until its read requirements are resolved.
   Cover native failure and later host failure before removing the old recovery.
3. Integrate cross-process coordination across the affected transaction, not just
   one file writer. Lite uses the shared live-config lock; the current CLI Codex
   executor uses a process-local mutex. Reconcile database/file lock order and
   resource identity with cross-consumer contention tests before claiming safety.

Each implemented change still requires local validation and two fresh blind
reviews. No production contract, dependency pin, schema or MSRV changes in the
baseline-only gate. Lite needs no dependency update for documentation alone.
The full product's own write and recovery behavior remains unverified; these CLI
baselines are requirements to compare, not defaults to impose on another consumer.

The rest of native provider projection/import and Skill deployment remain pending.
No overall migration stage above is marked complete yet.
