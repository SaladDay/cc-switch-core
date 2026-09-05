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
are not complete: Gemini timeout precedence and import tolerance differ between
consumers and need explicit policy boundaries.

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

The next slice moves Codex native import through the registered adapter. Hosts
choose validation responsibility, document-presence rules, and authentication
classification explicitly. The default import remains strict. A host-validated
snapshot preserves the host's accepted JSON shape and TOML grammar; paths, parser
diagnostics, provider names, model-catalog loading, and session-history policy
remain in that host. Local document inventories can use an explicit content
bound without changing default snapshot or operation-plan limits.
The rest of native provider projection/import and Skill deployment remain pending.
No stage above is marked complete yet.
