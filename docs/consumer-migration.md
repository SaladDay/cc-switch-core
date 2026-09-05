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
This change is under validation. Native provider projection/import and Skill
deployment remain pending. No stage above is marked complete yet.
