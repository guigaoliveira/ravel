# Changelog

All notable changes to Ravel are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed — a diagnostic that was wrong more often than right
- **`validate` no longer reports `cross_package`.** It compared the first path
  component of each side, so a monorepo laid out as `apps/<service>` and
  `libs/<shared>` had every descent into a shared library reported as a boundary
  violation — while coupling between peer services was invisible, because `apps`
  equals `apps`. On a 19,489-file workspace: 122,324 findings, 48,220 of them legal
  app-to-library descents, against 25,258 app-to-app imports it could not see.
  Aggregated to directory pairs the entire report was 15 distinct rows.
  **If you gate merges on this, read on:** `policy_report.by_code` loses the key,
  `total` drops, and `ravel ci --strict` now passes on repos that previously failed.
  Plain `ravel ci` is unaffected — it has always gated on cycles alone. Imports that
  point nowhere (`dangling_edge`) and relative imports that did not resolve
  (`unresolved_import`) are unchanged, and a declared `ravel.boundaries.toml`
  still reports `layer_bypass` / `cross_package_deny` at package granularity.
  Known limitation, measured rather than assumed: an acyclic `libs/* -> apps/*`
  inversion outside an existing import cycle is now reported by nothing.

### Fixed — a policy file that could not be read meant "no violations"
- **An unparsable `ravel.boundaries.toml` was swallowed.** `validate` returned
  `{"total":0}` and exit 0, indistinguishable from a clean repository, so a broken
  policy protected nothing while looking like it protected everything. Both
  `validate` and `ci` now fail with the file named. Consumers piping `ravel validate`
  into `jq` should expect exit 1 and no stdout in that case.
- **`boundaries` blamed git for a TOML syntax error.** The same loader failure was
  reported as `worktree identity: …`, sending the reader to an unrelated file. Both
  commands now share one error that names the policy file exactly once.

### Fixed — a full index could delete the pack it was writing
- **`index` could fail with `No such file or directory` on its own staged pack.**
  Generation GC removes every unreachable name containing `.tmp-`, which is exactly
  what an in-flight pack write is called; staging held no GC barrier, so a collection
  scheduled by the preceding sync could delete the file mid-write. Staging now holds
  the shared generation guard for the lifetime of the temp file, and GC defers as it
  already does for readers. Reproduced under parallel load at ~5% and now covered by
  a deterministic test.

## [1.14.0] - 2026-08-01

Answers stop being confident about what the index could not see. Every item below was
found by a blind or adversarial review and reproduced before being changed; two
reported problems turned out not to exist and were dropped rather than "fixed".

### Fixed — a zero that meant "I could not look"
- **A workspace of `.vue`, `.svelte` or `.astro` components certified false zeros.**
  Those formats contain TypeScript and import TS symbols, and the known-source list
  covered only languages that never do — so a function called from every component
  answered `total: 0` with `authoritative_zero: true`, `n_affected_exact: true`, and
  `orphans` naming it dead code. Nothing in any response contradicted "safe to
  delete". They are counted now, and `status` names them.
- `authoritative_zero` and `n_affected_exact` consult what the index could not read.
  They used to report only that a name resolved — that the question was *asked*, not
  that it could be answered. Both are false when a component format is unparsed, a
  file failed to read, or the resolver config is broken, and `explore`,
  `callers_of` and `calls_from` all carry `degraded_by` naming the cause. Gated on
  the component formats only: a Python build script does not reference TS symbols and
  must not make every later answer untrustworthy.
- A reverted edit is no longer answered from content that is no longer on disk. Auto-sync
  asks git what changed, which is a valid staleness oracle only while the index matches
  HEAD; once a sync publishes a generation built from uncommitted content, a clean tree
  stops implying "index equals tree". Edit, query, `git checkout --` used to leave the
  phantom edit in the index permanently with every health field green. Paths absorbed
  while dirty are recorded, checked against the index's own hashes, and pruned as soon
  as they match.
- A file that fails to read stays in the index with the reason, and `status` shows it.
  The scanner already recorded `read_failed` and `file_too_large`; the reason was
  collapsed into an unlabelled `parse_errors` integer that nothing inspected, so the
  unresolved-name hint accused the caller of a typo for a symbol defined in a file the
  indexer could not open. Adds `status.diagnostics`, read from the artifact index
  rather than by hydrating the snapshot.
- An incremental sync treats an unusable file exactly as a full index does — including a
  newly added one, which used to look "unchanged" because it had no bytes to hash. The
  stats a sync returns now match the generation it published; they disagreed whenever a
  path was rewritten outside the changed set.

### Fixed — diagnosis that blamed the wrong thing
- `validate` no longer reports an unresolved alias as an architecture violation. An
  unresolved import keeps its raw specifier, so `@utils/money` versus `src` looked like
  a boundary crossing. Relative imports that should resolve and do not are reported as
  `unresolved_import`; bare package names, Node builtins and asset imports are left
  alone, and `cross_package` only judges edges that actually resolved.
- A `tsconfig.json` that parses but loses its base is reported. Aliases usually live in
  the base config of a monorepo, so a missing base — sparse checkout, uninitialised
  submodule, renamed file — took every alias with it while the top file parsed fine.
  A `tsconfig.json` that is not a regular file is reported too; an empty one is treated
  as `{}`, which is what `tsc` does.
- A schema mismatch outranks a config problem in the `status` hint: when the on-disk
  index is newer, "fix it and run `ravel index`" is the one action that makes it worse.

### Fixed — an index its own reader refuses
- Every delta component is checked against the ceiling the reader enforces, not just the
  graph record, and the same check covers overlay compaction — composition is a union, so
  that is where a chain actually reaches the limit. Two overlays that each fit could merge
  into one that did not, producing the exact "exceeding read limit" failure the guard was
  written for.
- `sync` fails on a path that exists neither on disk nor in the index. It cannot be a
  deletion, and returning whole-index stats for it read as "synced, you are up to date".
- The coverage walk reports `walk_truncated`, and the "mostly another language" warning
  compares counts gathered in the same bounded pass. It used to compare a capped number
  against the uncapped index total, so on a large repo it could never fire.

### Not changed, and why
- `parse_errors` was reported as decreasing when a file failed to scan. Not reproducible:
  the count held at 0 on both sides of the failure.
- A hash-sidecar read failure was reported as silently disabling auto-sync. Unreachable:
  `publish_packed_snapshot` — the path `ravel index` takes — records `file_hashes: None`,
  so the sidecar is never written. The status hint and the MCP tool descriptions no longer
  advertise it.

## [1.13.0] - 2026-08-01

Two parameters that exist to cut what a caller has to read, plus a platform bug the
release pipeline could not see.

### Fixed
- **The gitignore filter was inert on macOS and Windows.** `IgnoreChain` canonicalized
  the workspace root but compared incoming paths against it byte for byte. macOS hands
  out `/var/folders/...` for `/private/var/folders/...` and Windows canonicalizes to
  `\\?\C:\...`, so every check answered "outside the workspace" — meaning gitignored
  trees were indexed on both platforms while Linux stayed green. Both spellings of the
  root are now accepted, and the path is re-spelled onto the canonical root before
  matching, because `ignore` *panics* when handed a path outside the matcher root.
  A third spelling — macOS aliases a tempdir's own `/var/folders/...` to the canonical
  `/private/var/folders/...` — falls back to canonicalizing the parent directory, which
  still exists when the file itself was just deleted and the watcher must process the
  removal. Reproduced on Linux with two symlinks to one directory. This is why CI's
  `Runtime` jobs were red on four platforms since 1.12.0 while `validate` — ubuntu
  only — stayed green; all four are green again.

### Added
- `scope` on `callers_of` / `calls_from` (`--scope` on the CLI): a path fragment that
  picks one definition when a bare name matches several, instead of copying a
  hundred-character candidate id out of the previous response. Measured on a 21k-file
  monorepo: 801 tokens across two calls becomes 348 in one.
  - Applied even when a single definition matched, so it can never be silently ignored,
    and echoed back as `scope` plus `scope_applied` — a mistyped fragment used to be
    byte-identical to a working one.
  - Matched against every definition, never the display preview. Filtering the preview
    would return one of an arbitrary ten shaped exactly like a successful answer.
  - Falls through to the short-name tier, so a scope naming where a *method* lives
    resolves it rather than falsely reporting `not_in_scope`.
  - Narrowing to none reports `not_in_scope` with the candidates showing where the name
    does live; narrowing to several stays ambiguous over just those.
- `rollup: "dir"` / `--rollup dir` (or `dir:N` for N levels): counts per directory
  prefix **instead of** the site list, top 12 plus a named `(other)` bucket. Answers
  "where is this concentrated" without paging every site: on a symbol with 2128
  references, 104,732 tokens across 22 pages becomes 202 in one call, and the size does
  not grow with the symbol. Each bucket carries `n` (edges) and `files` (distinct
  files) — one import plus one extends from the same file is one place to change, not
  two. Outgoing rollups group by the *referenced* file, since the referencing path is
  the queried symbol's own and identical on every row; `grouped_by` says which.
- `definitions_total` and `showing` on unresolved relation answers. A capped list of
  candidates with no count reads as "there are ten of these".

### Changed
- `--limit` is a visible alias for `--page-size` on `callers-of` / `calls-from`, so the
  CLI and the MCP tool stop disagreeing about the name of the page size.
- An unknown `rollup` value, or a depth outside 1–10, is refused rather than ignored:
  returning a normal page would read as a grouping that came out flat. A `cursor` passed
  with a rollup is reported back as `cursor_ignored` rather than silently dropped.

## [1.12.2] - 2026-07-31

### Fixed
- `shared daemon could not be started` now names its cause and the remedy. That
  message was the one dead end left in the surface: no reason, no next action. Its
  most common cause is an upgrade — a long-lived MCP server keeps running from a
  deleted inode after npm replaces the package, the daemon endpoint is
  version-scoped so it cannot borrow the new build's daemon, and spawning its own
  fails with a bare `ENOENT`. The client is now told which version it is running,
  that its binary no longer exists on disk, and to restart or reconnect. Two
  layers were hiding it: `daemon_client` collapsed the error into `None`, and the
  caller replaced it with a fixed string; the cause is propagated instead.

## [1.12.1] - 2026-07-31

### Fixed
- The file watcher no longer feeds itself. Indexing a path reads it, Linux reports
  that read as `Access(Open(Any))`, and the watcher treated every event kind except
  `Other` as a change — so one edit scheduled a sync, the sync's own reads raised
  more events, and those scheduled more syncs. Measured on a two-file repository:
  **53 publications for a single edit**, all returning the same snapshot, at ~6/s
  until the process was killed. Now 1. Reads are dropped in the producer, before
  they can occupy the bounded queue and raise the overflow signal that escalates to
  a full reconcile.
- `Access(Close(Write))` is deliberately kept: some backends report a completed
  write only that way, and dropping it would lose real edits. A test covers both
  directions, using the event modes Linux actually emits — the first version of
  this filter matched `Open(Read)`, which the kernel never sends, so it passed its
  own test while the loop continued.

## [1.12.0] - 2026-07-31

**If a workspace grew a gitignored copy of itself, reindex it once:** `ravel index`.
On a 21k-file monorepo that took 9s and shrank `.ravel` from 12GB to 2.2GB.

Most of this release comes from three blind adversarial reviews of the previous
one. Several fixes correct earlier fixes that were incomplete.

### Fixed — answers that could not be distinguished from real ones
- `callers_of` and `calls_from` no longer report `total: 0` for a name that
  resolved to nothing, or to several definitions. The resolver fell back to the
  raw string, the graph returned an empty page for a non-node, and the response
  was byte-identical to a symbol that genuinely has no references — the answer a
  caller acts on when deciding a change is safe. Both now return
  `resolved: false` with a reason and the candidate ids, and no count at all.
- `impact` refuses the same two cases instead of reporting
  `total_affected: 0, exact: true`. It is the blast-radius tool; that was the
  most dangerous version of this bug. The silent-fallback resolver is deleted, so
  no third caller can reintroduce it.
- `explore` names an invented identifier before reporting ambiguity. The check
  sat in an `else if` after the ambiguity branch, so `handleRequestzzz` came back
  as `ambiguous: true` with two `file:line` locations for `handle` — asserting
  the opposite of the truth. The lexical test also accepted a query that merely
  *contained* a real name; only the other direction (a real name containing the
  query, as in a prefix search) is a match now.
- `status` reports `indexed: false` when the on-disk schema is not the one this
  binary speaks. It read the manifest without the schema check, so it answered
  "Index ready" with file counts while every query failed — the exact state a
  running server hits after npm replaces the binary under it. Adds
  `binary_version` and `schema: {on_disk, expected}`, and a hint that says which
  direction the mismatch points and therefore whether to upgrade or reindex.
- `n_affected_exact` is `false` when no impact analysis ran. It was
  `unwrap_or(true)`: a positive claim of precision next to an unasked zero.
- `sync` fails when every path it was given was skipped, rather than returning
  whole-index stats that read as confirmation. Callers pass paths they just
  edited; a mistyped relative path used to yield certainty of a stale answer.
  Workspace metadata (`package.json`, `tsconfig.json`) is exempt — it carries no
  symbols but legitimately drives a re-resolution.
- `ravel ci` propagates validation errors instead of `unwrap_or_default()`.
  Swallowing the error emptied the findings, which *satisfies* the strict gate,
  so a policy-violating merge went green — while `ravel validate` exited non-zero
  for the same repo. A gate that fails open is worse than no gate.

### Fixed — the index no longer disagrees with itself about what belongs in it
- The file watcher applies gitignore, at every directory level. Its filter was
  "not inside `.ravel` and not a noise directory name", while a full index walks
  with gitignore applied. Found on a 21k-file workspace holding ten agent
  worktrees under a gitignored `.claude/worktrees/`: 279k TypeScript files, a
  second copy of every source. The builtin noise list missed them because a
  nested worktree's `.git` is a gitlink *file*, so no path component inside it is
  named `.git`. In order: every symbol gained a twin and stopped resolving, the
  index went 21228 -> 23969 files and 4.4GB -> 8.6GB, and the structural
  overlay's graph record reached 426MB against the 256MB component read limit,
  which makes `callers_of` fail outright.
- Nested `.gitignore` files count. A first version of the fix consulted only the
  root file, which is inert in the layout that matters: `apps/web/.gitignore`
  holding `dist/` is what excludes `apps/web/dist`. The matcher now walks the
  chain from the file outward and stops at the first decisive rule, so a negation
  in a deeper file re-includes as git does. Tests assert it agrees with the walk
  on both.
- `sync` with explicit paths is bounded by the same rules. It accepted any source
  path, so syncing a gitignored file indexed it and left index membership
  dependent on which command ran last.
- The predicate lives in one place and all four watchers use it — the shared
  daemon's, the MCP server's, both post-batch filters, and `ravel watch`. It had
  been inlined three times; fixing one copy is what left the others wrong.
- Gitignore rules apply only inside a repository, matching the index walk.
  Applying them unconditionally inverts the bug: a workspace with no git but a
  stray `.gitignore` would drop files the index collects.
- Paths are resolved against a canonicalized root. The CLI passes absolute paths
  while `--root .` is relative, and with a relative base nothing stripped — so
  every ignore check tested an absolute path against relative rules and answered
  "not ignored", silently passing everything.
- A newer index is never rebuilt downward. Two long-lived servers of different
  versions can share one workspace, and the incremental paths decline on any
  schema mismatch and fall back to a full rebuild — so an older binary rewrote a
  newer index in its own format, the newer one rebuilt it back, and each flip
  cost a full reindex. A full publish now refuses, naming the version that wrote
  the index and offering the way out.
- `coverage.authoritative_zero` reports whether an empty relation set was
  actually asked. It was hard-coded `false`.

### Known
- A structural overlay component can be written larger than the 256MB the reader
  accepts (`MAX_DELTA_COMPONENT_BYTES`), leaving the index unreadable until a full
  reindex. Excluding ignored trees removes the case that triggered it here, but a
  large enough legitimate workspace can still reach it: the writers enforce no
  matching ceiling.
- A malformed `tsconfig.json` is indistinguishable from an absent one, so every
  path alias silently disappears from the index and `callers_of` understates
  permanently, with no diagnostic.
- A file that fails to scan (permissions, fd exhaustion) is dropped from the index
  with no diagnostic, and the `parse_errors` count *decreases*.
- An over-size file is kept with a `file_too_large` diagnostic by a full index and
  removed by an incremental sync, so its coverage depends on which ran last.
- A hash-sidecar read failure is treated as an absent sidecar, so auto-sync
  silently does nothing and answers from the previous generation.

## [1.11.0] - 2026-07-31

Closes both items 1.10.1 recorded as Known.

### Fixed
- `explore` candidate scores no longer contradict the order the candidates are
  listed in. Exact id/qualified matches are deliberately listed ahead of scored
  search hits, but they carried a hard-coded 1250000 while an `exact-case` hit
  scores 1300000 — so reading the list top down found a higher score further
  along, and a caller could not tell which signal to believe. The sentinel is now
  `SCORE_EXACT_IDENTITY`, declared next to the scoring tiers it has to top, with a
  test that fails if any tier ever passes it. Ranking is unchanged; only the
  reported number was wrong.

### Added
- `callers_of` and `calls_from` report a failed background update as
  `sync_warning`, the field `explore` already used. These sites are the whole
  answer to "what breaks if I change this", and a dead watch update left them
  quietly describing an older tree; `status` knew, but a caller that only asked
  for relations had no way to find out. Present only when something actually
  failed, so a healthy response does not grow a permanently null field.

## [1.10.1] - 2026-07-31

### Fixed
- `explore` no longer answers a name nobody defined with a real symbol's
  identity. An identifier-shaped query is tokenized, so
  `TotallyNotARealSymbolXyz` matched an unrelated constant on the shared
  `Symbol` token and that constant became `primary` — carrying its definition,
  its source excerpt and its callers, with an empty `warnings` array. The old
  guard only fired when *nothing* matched, so any partial token hit silenced it.
  A query that looks like an identifier, matches nothing by identity, and
  resolves to a primary sharing no substring with what was asked now says so.
  Multi-word queries are untouched: term coverage is the point there and the
  caller never spelled an identifier. Found while using the MCP surface against
  a 25k-file workspace, then reproduced in the fixture, where the same query
  resolved to `getPendingLegalPersonOnboarding` and reported a caller for it.
- The `source` block carries its own `path`. `detail` already had it, but a
  source excerpt that does not name its file forces a second lookup to place it.

### Known
- `explore` candidates are not ordered by score: for a name like `create` in a
  large workspace, five `exact-qualified` hits scoring 1250000 precede
  `exact-case` hits scoring 1300000. The higher-scoring definitions are present,
  just below the lower-scoring ones.
- `callers_of` and `calls_from` expose no health channel. `explore` echoes a
  failed background update as `sync_warning` and `status` reports
  `last_update_error`; the relation tools return only their sites, so a client
  that never calls `status` cannot learn the index stopped updating.

## [1.10.0] - 2026-07-31

**The index is rebuilt once on first use after upgrading.** Pack records changed
layout, so `SCHEMA_VERSION` and the pack header version both move and auto-sync
rebuilds on the first command. Nothing to do by hand.

### Changed
- The pack is 67% smaller and a full index is slightly faster. Records whose
  reader deserializes them anyway — every bincode payload — are stored
  zstd-compressed; they are long runs of repeated path and symbol strings and
  compress 8-14x on real payloads. On a 20.4k-file / 744k-edge workspace:
  1731MB -> 562MB, and `index.total` 8849ms -> 8535ms over 3 alternating rounds,
  because writing 1.17GB less pays for the compression. Records borrowed
  zero-copy out of the mmap stay raw, and the zero-copy borrow refuses a
  compressed record rather than hand back bytes the consumer would misread.
- Syncing an edit is ~4x faster. `read_component_ref` opened a fresh pack reader
  per call, and opening one decodes the pack's entire directory — 39305 entries
  on that workspace — so resolving the 129 files affected by a single edit
  decoded it 129 times. Readers are cached per pack name, which is sound because
  a published pack is immutable: `delta.build_subset` 1059ms -> 22ms, and the
  sync containing it 1512ms -> 418ms.
- Cold graph load skips two full copies. It used to validate the ~90MB record,
  deserialize it into an owned flat form, expand that into one `Vec` per node,
  then move the result into the index — only the last shape is used. Building
  straight from the archive: `graph.open` 85ms -> 41-61ms.
- Indexing allocates in tight loops, which is where a general-purpose allocator
  does worst. mimalloc is now the global allocator: full index -23%, peak RSS
  3161MB -> 2738MB, cold CLI queries -27%. Not on musl, whose release builds
  could not be verified for a C toolchain here — those keep the system
  allocator, so no gain and no regression.
- `status` no longer re-walks the workspace on every call. The coverage probe
  added in 1.6.0 cost 77.9ms *per call*, making it the most expensive primary
  tool; it is now cached behind a TTL, because coverage describes which files
  exist on disk and has nothing to do with which index generation is published.
  Warm `status` 77.9ms -> 0.6ms.
- Auto-sync discovers dirty paths before loading the hash sidecar. The sidecar
  unzips the artifact index into tens of thousands of owned strings, and on a
  clean tree there is nothing to compare it against.
- Applying an incremental overlay makes one pass per node instead of scanning the
  adjacency once per changed neighbour — O(changes x degree) -> O(changes +
  degree), which matters exactly when an edit reaches a hub.

### Fixed
- `scale_exact_prefix_sublinear_wall` asserted absolute milliseconds calibrated
  for a release build while `cargo test` runs a debug build, so it failed on a
  loaded machine while the property it exists to protect was intact. It now
  asserts growth ratios. Verified by running it under 16 competing CPU hogs: the
  test took 3.6x longer and still passed.

### Notes
Six tests were added, one per invariant introduced here: compressed round-trip
with the bound applied to the expanded size, incompressible payloads stored raw,
the zero-copy borrow refusing a compressed record, the archived graph load
matching the path it replaced, overlay adds and removes in one batch, and
auto-sync still catching an edit after the reorder. Two of them failed on first
run — one caught a mistake in the test, one caught a payload that was not
actually incompressible — which is the point of writing them.

## [1.9.0] - 2026-07-30

Disk footprint, after a 13GB `.ravel` turned up on a workspace whose index is
1.7GB.

### Added
- `status` reports `disk`: bytes on disk, generations retained, and the
  configured `retention`. Retention keeps whole generations so a reader's mmap is
  never pulled out from under it, which makes the footprint a multiple of one
  index — 4.86GB across 3 generations on the workspace above. That is working as
  configured, and it was invisible until a disk filled up.
- `ravel gc` runs generation collection now instead of waiting for the deferred
  background pass, and reports bytes reclaimed. `--aggressive` collects down to
  the live generation for that run only, leaving the configured retention alone:
  2.48GB -> 2.08GB on a 20.4k-file corpus, after which explore, callers_of and
  validate return identical results.

### Notes
The 13GB observation is not reproduced and not explained. Retention is respected
in every path measured — content-only deltas share one base pack across
generations (1.7GB total for 4 generations), and structural edits grow it slowly
(1.8GB -> 2.4GB over six syncs), both capped at three generations. The directory
had self-collected to 4.86GB by the time it was investigated. Recorded here
rather than fixed by guesswork; `status` now makes the footprint visible and `gc`
makes it reclaimable, which is what the symptom actually called for.

## [1.8.0] - 2026-07-30

### Changed
- `explore` relations use the same site shape `callers_of` returns. 1.7.0 trimmed
  one and left the other emitting the old form for the same data: `id` beside both
  of its own parts, `provenance`, a `resolved` confidence on every entry, and
  `path` repeated inside a nested `site` object. Relations were 6203 of the 12828
  bytes of a concise explore — 48% of the response, half of it repetition.

  Both now return `path`, `line`, `symbol`, `kind`, plus `type_only` and
  `confidence` only when they say something. One shape to learn, and a site read
  the same way whichever tool produced it.

  Explore on a symbol with 20 reference sites: 12828 -> 8690 bytes (-32%), and
  19789 -> 8690 (-56%) counting the concise default from 1.6.0.

### Migration
Items in `relations.incoming` / `relations.outgoing` are flat: `site.path` and
`site.line` moved to `path` and `line`, `name` became `symbol`, and `id` /
`provenance` are gone. `symbol` is accepted as input by `callers_of` and
`calls_from`, so it replaces `id` for chaining.

## [1.7.0] - 2026-07-30

1.6.0 made the graph tool reachable and cheap. It did not make its answer
*sufficient*: `callers_of` returned the files that reach a symbol, so acting on
the answer still meant opening each one — the grep-then-read loop it was supposed
to replace. This release fixes that.

### Changed
- `callers_of` / `calls_from` (and `ravel callers-of` / `calls-from`) return the
  reference **sites**: file, line, the referring symbol's qualified name, the edge
  kind, and `type_only` when the reference cannot break at runtime — plus an exact
  `total` and a complete `by_kind`. Each site can be judged without opening the
  file, which is the whole point.

  They also switched from a transitive walk to the direct edges, matching what the
  names claim. "How far does this spread" is a different question, answered by
  `impact` or the new `reachable`.

  Measured on a 20.4k-file corpus, for a symbol with 20 real reference sites:
  4992 bytes for the complete, exact answer. `grep -rn` on the same name returns
  **820** textual hits — imports, comments, and same-named fields on unrelated
  classes — so the comparable grep sweep is ~115KB and still needs every hit
  triaged by hand. The 2801-byte grep quoted in 1.6.0 was 20 of those 820, which
  made it look cheaper than a complete answer by comparing it to none.
- Sites omit what does not change a decision: no `id` (it is `symbol://` + path +
  kind + qualified name, so emitting it beside `path` and `symbol` tripled the
  cost of every site, and `symbol` is already accepted as input), and
  `confidence` only when it is not `resolved`.
- `visited_nodes` / `visited_edges` are gone from these responses. They were
  traversal counters — cost on every call, no bearing on any decision.

### Added
- `reachable` (extended surface) keeps the transitive walk that `callers_of` used
  to perform, with a `reverse` flag, so the capability is not lost.

### Migration
`callers_of` / `calls_from` responses changed shape: `items` (file paths from a
transitive walk) became `sites` (direct references with lines), alongside `total`,
`by_kind` and `next_cursor`. For the previous transitive behaviour use
`reachable`, or `ravel query --reverse` on the CLI.

## [1.6.0] - 2026-07-30

This release is about whether an agent *reaches for* the graph, not how fast the
graph is. The trigger was measuring a session where an agent had Ravel connected
and used `grep` throughout: the cheap precise tool was not exposed, and the tool
that was exposed cost 7× a grep for the same question.

### Added
- `callers_of` and `calls_from` are primary MCP tools, and `ravel callers-of` /
  `ravel calls-from` are named CLI commands. "Who depends on this" was previously
  reachable only as `query --reverse` behind `RAVEL_MCP_TOOLS=all`, so the common
  question required knowing an uncommon spelling. Measured on a 20.4k-file corpus:
  answering it costs 2006 bytes this way, against 19769 for `explore` and 2801 for
  a bare `grep` that still leaves every hit to verify by reading.
- `status` reports coverage: `indexed_files`, `unsupported_source_files`, and the
  count per extension, from a bounded walk. In Ravel's own repository the previous
  answer was `"indexed": true, "hint": "Index ready"` while 3 files of 40 were
  indexed and 37 Rust files were invisible — a graph that answers nothing looks
  identical to a symbol that has no callers. It now says which is which.
- `explore` takes `detail` (CLI: `--detail`) for the full payload.

### Changed
- `explore` is concise by default: similar spellings are capped at 10 with
  `matches_total` alongside, and the blast-radius sample is omitted while
  `n_affected` keeps the count a decision turns on. Response size on that corpus:
  19789 -> 12327, 9827 -> 4118, 6111 -> 4507 bytes (-26% to -58%). Anything
  dropped is still available via `detail=true`, `callers_of`, or `impact`.
- MCP server instructions and the `AGENTS.md` / `CLAUDE.md` block now state which
  question each tool answers, including the one it should *not* be used for:
  literal text search belongs to grep, where a resolved graph has no advantage.
  They also state that answers include uncommitted edits — the standard objection
  to any code index is staleness, and auto-sync against the hash sidecar already
  removes it.

### Migration
`explore` responses no longer carry the full `matches` list or a populated
`impact` array unless `detail` is set. Consumers that read either should pass
`detail: true` or move to `callers_of` / `impact`.

## [1.5.0] - 2026-07-30

**The index is rebuilt once on first use after upgrading.** Symbol metadata
changed on-disk layout, so `SCHEMA_VERSION` moves 15 -> 16 and auto-sync
rebuilds the index the first time a command runs. Nothing to do by hand.
Running an older binary against a 1.5.0 index also rebuilds, in its own
format — see the note under Changed for why that is deliberate.

### Changed
- Symbol lookups no longer decode a shard to reach one entry. Metadata shards
  are rkyv archives read in place, binary-searched in the mmap, and only the
  matching entry is owned. Validation is proportional to a record's size, so it
  runs once per shard per process; packs are immutable once published, and a new
  generation builds a new runtime. Isolated A/B over MCP on a 20.4k-file /
  744k-edge corpus: `context.candidates` 6.87ms -> 1.93ms (-72%), warm
  `explore` 17.25ms -> 7.97ms (-54%). Explore output is identical for 9 queries
  with each binary reading the format it writes.

  Two honest consequences. rkyv's relative pointers and alignment padding make
  these records slightly *larger*, not smaller — 182.3MB -> 188.2MB, +0.3% of
  the pack; the change earns its place on the read path, not on size. And the
  schema bump is not cosmetic: a binary predating this layout would otherwise
  reject only the shard index and then answer from an empty symbol-meta
  backend, which is indistinguishable from a genuine "nothing found". With the
  bump both directions report an unsupported schema and rebuild, so a downgrade
  is loud instead of silently empty.
- Full index is a further 13% faster on that corpus (9124ms in 1.4.1 ->
  7949-8071ms), snapshot id unchanged:
  - Resolution's two remaining sequential steps now fan out — building the
    resolution universe and ordering the resulting edges.
    `resolve.sort_edges` 281.4ms -> 54.7ms, `index.resolve_edges` 1240.0ms ->
    910.8ms.
  - Structural contributions are built per file in parallel: 475.8ms ->
    411.4ms. The small share confirms that stage is dominated by its
    sequential merge.
- `RAVEL_TIMING=1` reports resolution's phases: universe build,
  imports/exports, symbol refs, merge/dedup, and the edge sort.

### Notes for the next round
Two measured findings are recorded in the code rather than acted on, so they
are not rediscovered from scratch:
- `snapshot/edges` (293MB, 16% of the pack) is derivable from the graph's file
  section and is read only when hydrating a full snapshot, never by a query.
- `graph_from_edges` (1050ms) is dominated by ordered-map traversals over
  string keys, not by allocation — two attempts to cut its clones measured
  22% and 7% *slower*. Interning node ids is the structural fix, and it would
  also cut ~500MB from the pack.

## [1.4.1] - 2026-07-29

Released as 1.4.1: the 1.4.0 tag failed its own validation job (`cargo fmt
--check`) before publishing anything, and a tag that exists is not moved.
No 1.4.0 artifact was ever published, so this is the first release of the
work below.

### Added
- `this.<field>.<member>` calls resolve through the field's declared type.
  NestJS controllers and use-case classes are built almost entirely from
  constructor-injected services, so their call graph was largely invisible:
  the member lookup stopped at the owning class. Covers constructor
  injection, aliased imports, and interface-typed fields.
- Cursor pagination on the graph walk (`QueryLimits.cursor`, `ravel query
  --page-size N --cursor N`). Every page previously restarted at offset 0,
  which made the returned `next_cursor` unusable and hubs impossible to
  enumerate completely.
- Complete per-kind edge counts on explore (`incoming_by_kind` /
  `outgoing_by_kind`). Bounded by the number of edge kinds, so a hub reports
  its true shape even when the relation page truncates.
- File-level cycles (`ravel cycles --files`). Package-level SCCs collapse a
  monorepo whose top-level buckets all reach each other into one giant
  component, hiding the actionable cycles between files.
- `related-tests` also probes `src/` -> `test/` mirrors (NestJS/Jest layout).

### Changed
- `ImpactReport.exact` marks a budget-saturated `total_affected` as a lower
  bound instead of presenting it as the real total.
- Truncation warnings only promise a re-query limit the explore page can
  actually serve, and point at the paginated walk beyond that.
- `validate` returns a bounded findings page plus complete per-code counts
  (`--limit`); the raw list reaches megabytes on large monorepos.
- `context` builds the index when no snapshot exists instead of failing and
  telling the agent to run `ravel index` by hand.
- Warm query latency is 5x lower. Measured over MCP against a live server on
  a 20.4k-file / 744k-edge corpus, 3 alternating rounds against the previous
  binary: mean warm `explore` 70.2ms -> 13.7ms. Four causes, all in the read
  path:
  - Term search evaluated the union of every query token's postings. Query
    "PixPaymentService" tokenizes to pix/payment/service, covering 190671
    postings over 156022 distinct definitions — all scored and ranked to
    return 160. The scorer's ceiling per coverage level is an analytic
    constant, so once the k-th best definition carrying the rarest token
    scores strictly above the ceiling for one-fewer-token coverage, no
    skipped definition could have entered the result. Term search now walks
    the lists as a merge join driven by the rarest one and falls back to the
    full union when that test fails. Candidates 156022 -> 29831;
    `search_terms` 20-70ms -> 1.3ms.
  - Packed archives were re-validated on every query. `rkyv::access` walks
    the whole archive, and the term record is ~117MB on that corpus.
    Validation now happens once per runtime; generation packs are immutable
    once published, and a new generation drops the runtime.
  - Candidates were owned before ranking: three heap allocations per
    candidate, of which all but `limit` were discarded. They are now ranked
    while still borrowing from the index.
  - Auto-sync rebuilt the hash sidecar (20k+ owned paths) on every call to
    check index freshness: 14-21ms per call, now cached per generation.

  Explore and term-search output are byte-identical before/after for 35
  queries, including bare "service" / "pix" / "payment", a multi-token
  phrase, and a token absent from the index.
- Syncing a batch of edits is ~15x faster. Loading previous artifacts,
  re-parsing new bytes, and reading/hashing the batch are per-file work that
  ran one file at a time, and the artifact index was decoded once per path
  probed. 30-file sync on the corpus above: `sync.prepare_paths` 481ms ->
  17ms, `sync.structural_delta` 1587ms -> 103ms, total 2066ms -> 141ms.
- Full index is 23% faster: 11838ms -> 9124ms, 3 alternating rounds vs 1.3.0
  (tmpfs, to isolate CPU from disk). `stage_snapshot` 4649ms -> 2173ms.
  Term index construction, symbol-meta staging, artifact encoding and edge
  digesting now fan out across cores; snapshot id is unchanged throughout, so
  the built index is identical.
- Reference resolution uses per-artifact lookups instead of scanning the
  artifact's symbols once per reference: O(refs x symbols) -> O(1) per
  reference. Performance-neutral on the corpus above (its files are small);
  it bounds the cost for generated files with thousands of symbols. The
  lookups only needed for `this.<field>.<member>` are built on first use,
  since most files never contain such a reference.
- `RAVEL_TIMING=1` breaks down what was previously opaque: `stage_snapshot`
  and `stage_graph` (together ~40% of a full index), the context workers,
  the auto-sync steps, dirty discovery, both halves of `prepare_paths`,
  artifact-delta publication and compaction, and what the candidate loop
  actually does. `context.prefix` in particular used to measure a whole
  parallel scope rather than search.

### Fixed
- `diff-impact` mapped nothing: git returns absolute paths while graph nodes
  are workspace-relative, so every changed file was skipped.
- An upgraded binary could route queries to a daemon from an older install
  (same wire protocol, older semantics). The runtime endpoint and singleton
  lock now embed the binary version.
- A daemon that is shutting down answers politely instead of dropping the
  connection; that reply surfaced "daemon is shutting down" to the agent
  rather than respawning.

## [1.3.0] - 2026-07-17

### Changed
- Search backend moved off `tantivy` to a custom `rkyv` + `memmap2` sectioned
  reverse index (definition-level inverted term index plus spelling dictionary),
  read zero-copy through the generation-pack mmap reader. On a 20.6k-file corpus:
  cold one-shot search 3–6× faster (exact/prefix 60→10ms, fuzzy 60→20ms, terms
  80→30ms), `.ravel` −10% (1.9→1.7GB), peak RSS −6%, release binary −18%
  (18.0→14.8MB), and the `tantivy` dependency tree removed (−601 Cargo.lock
  lines). Result value-sets are identical before/after for every search kind.
- Trade-off: full workspace index is ~21% slower (~10.0→12.1s); it is a rare,
  one-time operation off the agent hot path.

### Fixed
- Term-search paths bound-check `document_index` against the document count
  (`.get()` + skip) rather than indexing directly, so a corrupt or
  version-skewed on-disk pack degrades gracefully instead of panicking — rkyv's
  structural validation does not enforce that semantic invariant.

## [1.2.0] - 2026-07-17

### Changed
- Structural sync cold path rebuilt around a sectioned graph base pack:
  independent `graph/file`, `graph/edge`, `graph/adj` key-spaces so a single-file
  sync decodes only the section it touches; edge refcounts keyed by blake3-128 of
  the edge bytes. Cold one-shot structural sync 12.5s → ~0.76s, graph section
  read 785MB → ~94MB, peak sync RSS 3.9GB → ~0.69GB.
- Warm daemon structural sync 11.5–19s → ~0.26s via partial delta state,
  resident readers carried across generations, adaptive delta overlays
  (63MB → 3.9MB per sync), and parallel shard decode.
- Watcher reconcile runs incremental sync instead of a full reindex on backend
  rescan; full reindex only on non-git roots.
- Full index parallelised (parallel per-artifact resolution, parallel publish).

## [1.1.0] - 2026-07-12

### Added
- Shared per-workspace daemon with transient MCP leases, persistent CLI control,
  per-root caching, bounded connections, and watcher leadership failover.
- Incremental generation packs, artifact deltas, structural reverse indexes,
  atomic failpoint coverage, and bounded generation garbage collection.
- Configurable bounds for watcher storms, sync tickets, daemon connections,
  retained generations, and artifact-store amplification.

### Changed
- Explicit sync batching is contention-driven; a single writer no longer waits
  for a fixed coalescing window.
- Changed-path no-op checks use point lookups instead of materializing all file
  hashes. Worktree identity and large sidecars are cached per engine.
- Structural acceleration is resident-only on sync, avoiding a cold path that
  used more time and memory than the exact fallback.
- Common CLI/MCP reads use compact sidecars and bounded/LRU caches; packages,
  fuzzy search, graph limits, impact counts, and byte limits were tightened.
- Installers and release artifacts verify SHA-256 checksums; release workflows
  pin actions and validate packages before publication.

### Fixed
- Cross-process writer races, stale-generation cache reads, watcher event storms,
  daemon lease exhaustion, blocked daemon startup, and GC/reader deadlocks.
- Add/delete/rename equivalence, idempotent A-to-B-to-A publication, stale hubs,
  Git path handling, co-change history, boundary matching, and fuzzy ranking.
- Concurrent agent configuration writes and archive extraction validation.

## [1.0.0] - 2026-07-12

Initial public release.

### Added
- Local code graph for TypeScript / JavaScript codebases — index once, query cheaply.
- Sub-200ms agent hot path over `.ravel/` sidecars (cold CLI, existing index).
- CLI commands: `index`, `sync`, `watch`, `status`, `context`,
  `search`, `query`, `impact`, `hubs`, `cheatsheet`, `doctor`, `install`, `uninstall`.
- MCP server (`ravel mcp`) with 3 primary tools by default; `RAVEL_MCP_TOOLS=all` for the full set.
- One-shot agent wiring (`ravel install`) for Claude Code, Cursor, Codex, OpenCode,
  Gemini, Windsurf, VS Code, Grok, and more.
- Compact JSON output by default (`--pretty` for humans).
- Zero-config defaults with optional `.ravel.toml` overrides (extensions, ignore, sync).
- Git-optional operation: auto-sync when `.git` is present, `watch`/`sync <paths>` otherwise.
- Automatic entry-point detection for application entry files/controllers and `main.ts` / `bootstrap`.
- Install scripts (curl / PowerShell), npm distribution, and `cargo install` from source.

[Unreleased]: https://github.com/guigaoliveira/ravel/compare/v1.10.0...HEAD
[1.10.0]: https://github.com/guigaoliveira/ravel/compare/v1.9.0...v1.10.0
[1.9.0]: https://github.com/guigaoliveira/ravel/compare/v1.8.0...v1.9.0
[1.8.0]: https://github.com/guigaoliveira/ravel/compare/v1.7.0...v1.8.0
[1.7.0]: https://github.com/guigaoliveira/ravel/compare/v1.6.0...v1.7.0
[1.6.0]: https://github.com/guigaoliveira/ravel/compare/v1.5.0...v1.6.0
[1.5.0]: https://github.com/guigaoliveira/ravel/compare/v1.4.1...v1.5.0
[1.4.1]: https://github.com/guigaoliveira/ravel/compare/v1.3.0...v1.4.1
[1.3.0]: https://github.com/guigaoliveira/ravel/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/guigaoliveira/ravel/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/guigaoliveira/ravel/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/guigaoliveira/ravel/releases/tag/v1.0.0
