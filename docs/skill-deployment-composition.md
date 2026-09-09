# Skill deployment composition

Status: design and acceptance requirements, not an implemented API. This refines
the [third-consumer plan](full-product-readiness.md) for the next Skill slice.
Only Core, Store and the CLI migration branch/Lite are in scope. The full desktop
repository must not be accessed or changed without separate authorization.

## Observed compatibility boundary

The baseline is Core/Store `f5b6b4cd`, CLI `478fb2eb` and Lite `a69bfb2c`.
Lite pins Core/Store `7b7cfae5`; Skill code is identical at those Core revisions.
These are source observations unless identified as tests in the
[consumer acceptance record](https://github.com/SaladDay/cc-switch-cli/blob/refactor/core-migration-validation/docs/shared-consumer-acceptance.md#skill-shared-consumer-adoption).

| Concern | Current Core/Lite | Current CLI |
| --- | --- | --- |
| Deployment | Protected public reference through a private persistent anchor | Direct symlink, directory copy, or Auto selection/fallback |
| Disable | Native controls where declared; reference may remain dangling | Removes the native entry; does not project per-Skill native controls |
| Existing entries | Rejects entries without valid Core ownership | Pi checks source equivalence; other Apps permit replacement |
| State | Separates catalog selection, discovery and native controls | Toggle changes the catalog flag and native directory |

Relevant code: Core `src/skill/{read,reference,write}.rs`, Store
`crates/cc-switch-store/src/skill.rs`, CLI
`src-tauri/src/services/skill.rs` (`sync_to_app_dir`, `remove_from_app`,
`toggle_app`) and Lite `src-tauri/src/{skill,skill_live}.rs`.
The three red CLI acceptance gates cover stale selection loss, writing through
Lite's native lock, and missing native recovery after catalog rejection. They
are not evidence for copy/link interoperability. Lite's recovery gate now passes.

## Intended separation

Core should expose common deployment observation, planning and guarded execution
below its complete Skill switch workflow. A directory deployment must not require
a fabricated Gemini/Hermes document or silently edit it. Conversely, selecting a
directory-only operation must not claim that the App's effective Skill state has
changed: unified discovery and native disable/required-Skill rules still apply.
The existing complete planner keeps these rules and its current defaults.

The host resolves paths and chooses a supported deployment mode. App discovery
and native control rules stay in the registered Core contract. Core owns the
copy/link mechanics, observation checks, ownership records and recovery; Store
owns guarded catalog writes. The host owns transaction order, acquisition of
Skill source files, settings and product events. There must be no `cli_mode`,
`is_lite`, or callback that merely runs the old product deployment implementation.

Ownership and deployment mode are separate decisions. A matching directory name,
catalog row, or content hash alone must not grant Core persistent ownership.
Legacy replacement authority must be explicit, scoped to a host-approved observed
entry, and rechecked before mutation. Preserve Pi's existing conflict policy and
Core-owned entries regardless of the caller's ordinary replacement policy.
Copies must remain copies, including Auto's existing-copy behavior; enabling
through Lite must not silently convert a recognized managed copy into a link.
Do not rename or delete a current protected public reference to simulate a
different representation. Any transition needs its own ownership/recovery design.

## Four implementation gates

1. **Capture deployment compatibility.** Add bounded tests around the existing
   public CLI toggle and real Lite service. Cover Auto/Copy/Symlink, repeated
   enable/disable, existing copies and links, missing sources, native controls,
   Pi conflicts and both consumer orders. Record link targets and object type as
   well as content/catalog results. Separate baseline behavior from desired fixes;
   keep the three known red requirements. This gate changes tests only. It does
   not add another mock deployment engine or treat a passing control as migration.
2. **Implement one shared deployment path with real callers.** Begin with Claude
   toggle, all three CLI modes, and Lite operations on the same installed Skill.
   Compose deployment with Store's scoped selection write, without involving
   native-control documents. Reuse the complete planner's common pieces where
   appropriate; do not construct a fake complete switch plan for a catalog write.
   Replace the migrated production path in CLI and make Lite understand its
   managed representations in the same reviewed slice. Keep Core's old API/default
   behavior and protected-reference state readable. If safe interoperability
   requires a new ownership format, resolve the compatibility gate below first.
   No other App, install, import, set-apps or storage migration changes here.
3. **Extend the composed workflow.** Migrate the remaining Skill Apps one bounded
   change at a time. Verify directory-only CLI behavior separately from Lite's
   complete native-control workflow, including Gemini/Hermes and unified discovery.
   Reuse registered rules, not another App switch table. Include explicit path
   overrides and native-state disagreement; do not reinterpret directory presence
   as effective enablement. The all-App claim requires every supported mode/App.
4. **Remove remaining duplicate writers.** Inventory set-apps, install/reuse,
   import, uninstall, sync and source/storage migration. Move common deployment
   and guarded catalog work into the same shared APIs in separate small changes.
   Preserve acquisition, presentation and scheduling in the host. Record remaining
   local writers and remove only replaced code; a shared dependency pin is not
   completion. Provider/MCP/proxy/UI changes are outside this Skill plan.

## Acceptance before publishing a shared deployment change

- **Representation compatibility:** old/new readers and writers, current version-6
  ownership records, disabled dangling references, direct links, managed copies,
  interrupted transitions and rollback to the previous consumer versions. Unknown
  ownership formats must be preserved, not treated as disposable/unowned paths.
  Define persistence and adoption before extending the format. If the old writer
  can destroy the new representation, require an explicit rollout/migration plan;
  a dependency pin or advisory lock alone does not resolve that incompatibility.
- **Filesystem behavior:** binary assets, nested directories, permissions, non-UTF-8
  names, relative/dangling/nested links, source changes and destination replacement.
  Capture current link-following behavior before imposing new copy limits. Any
  necessary behavior restriction needs an explicit compatibility decision, not a
  silent fallback. Exercise Windows, macOS and Linux; do not label a synthetic
  third host as a full-product parity test.
- **Failure boundary:** database write transaction, shared file lock, fresh native
  observation, deployment, scoped catalog write, commit or guarded recovery. Keep
  protections through recovery while SQLite still owns its transaction. Handle
  SQLite-ended/uncertain commits separately; preserve intervening native edits,
  unrelated App flags, unknown fields and original errors. Check publication,
  compensation, lock release and successful retry with real peer processes.
- **Recovery ownership:** do not delete the last restorable directory before the
  host's catalog decision. Distinguish pre-commit failure from post-commit receipt
  finalization failure; the latter must not replay an obsolete catalog snapshot.
  Specify restart recovery for durable pending state and bound owned temporary
  storage. Do not claim SQLite/filesystem atomicity or protection from all external
  writers. Cleanup may remove only verified owned objects.

Every gate, including this design, needs two independent blind reviews of its
entire diff. Give reviewers the requirements, acceptance criteria and boundary,
not implementation explanations or prior findings. Fix confirmed issues and
repeat; use one fresh reviewer only for converged local refinements. If the
copy/link and ownership designs cannot meet these requirements without repeated
special cases, reconsider the design before adding more patches. Core/Store
completion and full-product adoption remain separate, unfulfilled claims.
