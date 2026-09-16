# Call graph

`callers`, `callees`, and `trace` share a persistent per-repository call graph with the dependency graph. This page documents the internal architecture. See the README for the user-facing commands, [deps.md](deps.md) for the file-level import graph used during disambiguation, and [file-filtering.md](file-filtering.md) for file selection.

## What it answers

`callers` and `callees` are kind-aware. The same target string returns different results depending on whether the resolved symbol is a callable or a type:

| Target kind | `callers X` | `callees X` |
|---|---|---|
| function / method / constructor | call sites where `X` is invoked (in-edges) | call sites inside `X`'s body (out-edges) |
| class / struct / trait / interface / enum / record | downstream uses: implementors and constructions, including unit-struct receiver patterns (`Foo()`, `Foo::new()`, `Foo {}`, `new Foo()`) | upstream dependencies: ancestor types and the methods they declare, walked transitively via `--depth N` |

Both directions are inverses on their respective graphs. Diamond inheritance is handled. Ambiguous callers and unresolved or external callees are shown by default; `--hide-ambiguous` for callers and `--hide-external` for callees drop them. `callers` also scans for bare edges that name the target but were never attributed to a node (`traverse::unattributed_callers`). Without this scan, a chain such as `connection.getCtx().getPrefs().forDate()` with an unknown receiver type would make `callers forDate` report `0` with no lead to inspect (issue #31).

Three checks in `traverse.rs` and `render.rs` bound the unattributed results:

- A receiver that names a project type other than the target's enclosing type drops the edge. Receivers that name locals, parameters, or external types remain because rejecting an unknown name would drop real `connection.close()` sites with the noise.
- `name_declarers` counts project symbols that declare the target's terminal name. Above `render::MAX_DECLARERS_TO_LIST` (3), the renderer replaces rows with a count and reason because each site is evidence about every declarer. For example, `callers CliError.new` in this repository would otherwise sample 1,020 `Vec::new()` and `String::new()` sites. JSON includes `unattributed_declarers` and `unattributed_suppressed` so consumers cannot mistake this total for a caller count.
- The section takes `min(--limit, 25)` rows, independent of the resolved-hit budget. It orders rows by ascending ambiguity breadth and shows the receiver as written (`recv=os`), which supports triage without type inference.

Resolving the remaining sites requires adapters to record declared local and parameter types such as `ResultSet rs = ...` and `void f(PgConnection c)`. This information is explicit syntax, not inferred type flow.

Symbol forms accepted by both subcommands:

```
ast-bro callers TakeDamage
ast-bro callers Player.TakeDamage
ast-bro callers src/Player.cs:TakeDamage
ast-bro callers --file src/Player.cs --symbol TakeDamage
```

The first three are positional; the flag form exists for clients that prefer to avoid string-splitting on `:` / `.`.

## Pipeline

```
build_call_graph(root, deps):
  par_iter(files):                          # rayon
    pass = extract_file(file, lang)         # adapter walks tree-sitter tree:
                                            #   Declaration::calls   (raw CallSites)
                                            #   ParseResult::imports (ImportBindings)
    aggregate(pass) -> Vec<FilePass>

  symbol_table = build_symbol_table(passes) # name -> Vec<Qn> (terminal segment)

  for each raw edge:
    pass A: same-file                       # bare name -> qn via local
                                            #   defined_names + ImportBindings,
                                            #   resolved through suffix index
    pass B: global symbol table             # single-match promotion;
                                            #   receiver-bearing calls deferred
                                            #   to pass C (avoids
                                            #   `builder.hidden()` false hits
                                            #   on global homonyms)
    pass C: dep-graph disambiguation        # filter ambiguous candidates
                                            #   by the caller's transitive
                                            #   forward-dep closure

  -> CallGraph {
       forward, callable_meta, types,
       symbol_table, type_by_name,
       implementors, stats,
     }

callers <Sym>:                              # kind-aware:
  if callable: reverse traversal of `forward`
               + unattributed bare edges naming <Sym>
  if type:     implementors plus constructions

callees <Sym>:                              # kind-aware:
  if callable: forward traversal of `forward`
  if type:     ancestor walk on `types[*].bases`, depth-limited
```

## Tracing call paths (`trace`)

`trace <FROM> <TO>` answers "how does `<from>` reach `<to>`?" It returns the chain of
calls between two symbols, with each hop's source body inlined so a flow
question (`request -> handler`, `update -> render`) is answered in one call instead
of the agent manually chaining `callees`.

- **Search**: a multi-source / multi-target BFS over `forward` (the callees
  direction), following only `Resolved` edges, from any qn matching `<from>`
  to any qn matching `<to>`. Returns the shortest path; `--depth N` caps the
  hop count (default 12). Targets are suffix-matched exactly like `callers`
  (`run`, `Type.method`, `src/f.rs:name`).
- **Bodies inlined**: each node on the path is rendered with its source,
  extracted via the same `core::find_symbols` path `show` uses (parsed files
  are cached, so a path through one file parses it once). Output is
  size-capped by `MAX_BODY_CHARS` per symbol and `MAX_TOTAL_CHARS` total. After
  either limit, remaining hops are listed header-only.
- **Graceful failure**: a missing static path can indicate a dynamic-dispatch
  or framework boundary, such as a callback, trait object, or route handler for
  which the resolver will not invent an edge. The response
  still inlines both endpoints plus the target file's sibling callables, so
  the agent has somewhere to look. A found path and a resolved-but-no-path are
  both exit 0 (the output is the answer); only an unresolved `<from>` / `<to>`
  is exit 2.
- **JSON** (`--json`): `ast-bro.trace.v1` returns `{from, to, found, frontier_truncated,
  hop_count, hops: [{qn, file, line, kind, via, via_line, confidence, body}]}`, or
  `{found: false, frontier_truncated, endpoints, siblings}` on the no-path branch.
- **`found: false` is two findings, not one.** The BFS stops either because it
  exhausted the reachable graph or because it hit `--depth` (default 12) with
  callees still to follow, and only the first justifies blaming dynamic
  dispatch. `frontier_truncated` separates them: the text output says "no
  static call path found within `--depth N`" and names the flag to raise, and
  the JSON field is present on both branches so a consumer reads one thing
  either way. Only an edge into a node the search never visited counts;
  unresolved and external callees are where the static graph ends, so raising
  `--depth` would not walk through them (issue #32).

The implementation lives in `src/calls/trace.rs` as a thin layer over the same `CallGraph.forward`
map `callees` walks; it needs no new IR or cache state.

## Module layout

```
src/calls/
|-- mod.rs          orchestrator: build_call_graph(root, &DepGraph) -> CallGraph
|-- pass.rs         shared phase-1 IR: FilePass, RawEdge, qn_from, raw_to_edge,
|                   file_rel  (lifted out of build.rs to break the
|                   build/resolve cycle; `ast-bro cycles src/calls/`
|                   was flagging it)
|-- build.rs        per-file extraction + FilePass aggregation
|-- resolve.rs      three-pass resolver:
|                     run = build_symbol_table -> run_with_table
|                   (the split lets the incremental updater resolve a
|                   partial pass set against a precomputed global table)
|-- graph.rs        Qn, CallEdge, CallTarget, Confidence, CallableMeta,
|                   TypeMeta, CallGraph, GraphStats
|-- traverse.rs     forward / reverse BFS
|-- trace.rs        shortest-path BFS between two symbols, bodies inlined
|-- render.rs       text + JSON renderers (palette matches core/surface)
|-- cli.rs          run_callers / run_callees / run_trace + type-aware paths
|-- cli_helpers.rs  kind-aware target resolution (Callable vs Type)
`-- mcp.rs          MCP server wrappers
```

## IR additions

Three new types in `src/core.rs` plus two new fields on the existing `Declaration` and `ParseResult`:

```rust
pub struct Declaration {
    // ... 18 existing fields ...
    pub calls: Vec<CallSite>,           // direct body only; nested decls own theirs
}

pub struct ParseResult {
    // ... 6 existing fields ...
    pub imports: Vec<ImportBinding>,    // local-name -> module spec
}

pub struct CallSite {
    pub name: String,                   // bare name as written: `foo`, `bar`, `baz`
    pub receiver: Option<String>,       // `obj` for `obj.bar()`, `Foo` for `Foo::baz()`
    pub line: u32,
    pub kind: CallKind,
}

pub enum CallKind { Call, Construct, Macro, Super }

pub struct ImportBinding {
    pub local: String,
    pub module: String,
    pub line: u32,
}
```

`Declaration::calls` is attached to the enclosing declaration so the source/caller relationship is implicit (the caller is the declaration that owns the list). This gives correct nesting semantics for free in languages where one function is defined inside another (Python closures, JS arrows, Rust nested fns).

## Three-pass resolver

Homonyms such as `helper`, `init`, `parse`, and `validate` create most false matches. The resolver applies three passes in increasing order of cost.

### Pass A resolution

For each `RawEdge` whose target is a bare name:

1. Try Zig import paths. For `helper.work()`, pass A resolves `helper` as an import binding and selects a real file-scope `work` declaration. It also resolves inline calls such as `@import("helper.zig").work()`, declaration scopes such as `module.Type.init()`, and renamed facade chains such as `api.Controller.init()`. An alias can refer to a module, namespace, type, or callable, including a bare call such as `const run = module.run; run()`. The adapter rewrites function-local import calls to their inline `@import` form within the binding's lexical block, so a local name cannot leak into another function. This check runs before generic receiver handling because Zig permits imports named `self`, `this`, `crate`, and the other spellings that are keywords or conventions in other languages.
2. Try same-file definitions. A receiverless or self-like call can bind locally. Self-like binding prefers the sibling under the caller's scope, so `self.shared()` inside `Greeter::caller` selects `Greeter::shared` instead of another class's `shared`. Zig treats only `self` and `Self` as self-like; `this`, `cls`, `crate`, and `super` remain ordinary explicit receivers. An explicit object receiver such as `connection.getCtx()` blocks bare same-file binding because the target belongs to another object (issue #31).
   - A type-qualified call such as `Foo::bar()` or `Foo.bar()` can bind `Exact` only when a local qn has the complete `::Foo::bar` scope under the caller. The Zig adapter records simple parameter types and local struct-initializer types, so `widget.ping()` can become the exact `Widget.ping`. A Zig receiver without a concrete import or type remains unresolved instead of binding through a file-level dependency guess.
   - Rust prefixes use anchored scopes. `self::P` starts at the caller's enclosing scope, each `super::` skips one additional level, and `crate::P` anchors at a crate root such as `lib.rs`, `main.rs`, or a `src/bin` target. A miss continues to passes B and C without terminal-name fallback.
   - PHP scoped keywords do not arrive as receivers. The adapter normalizes case-insensitive `self::`, `static::`, and `parent::` forms to `None`. The resolver does not classify bare `static` or `parent` variables as self-like.
3. Try direct imports. A receiverless or non-shifting self-like call whose callee matches an import binding resolves through the same dependency resolver.

Within the resolved target file, import paths require an exact qualified name. `helper.work()` selects `helper.zig::work`, while `helper.Type.work()` selects only `helper.zig::Type::work`. Neither form selects a sibling or nested homonym, and the resolver never invents a missing Zig qn. A recognized Zig namespace path that fails partway stays unresolved. Legacy direct-import resolution for other languages may still fall back to a nested declaration or a synthesized file-scope qn when an adapter did not emit a matching declaration.

### Pass B symbol table

`symbol_table: HashMap<String, Vec<Qn>>` indexes every project declaration's terminal name. For each remaining bare edge:

- 0 candidates: leave `Bare`.
- 1 candidate: promote to `Resolved` with `Exact`.
- N candidates: defer to pass C.

Receiver-bearing calls such as `obj.bar()` are not promoted by pass B. A single global match could otherwise let `obj.hidden()` claim an unrelated `hidden` definition. These edges go through pass C, which can confirm a dependency relationship. `receiver_is_self_like` in `src/calls/resolve.rs` lists the self and scope forms: `self`, `Self`, `cls`, `crate`, `super`, `this`, and `$this`.

### Pass C dependency disambiguation

For each ambiguous edge with N candidates, load the dep half of the unified graph, compute the caller file's transitive forward-dep closure via `src/deps/traverse.rs::forward`, and filter the candidates to those whose file is in that closure.

- Exactly 1 survives: promote to `Resolved` with `Inferred`.
- More than 1 survives: keep all in `CallEdge::candidates` and tag the edge `Ambiguous`. The renderer shows the count and one canonical choice. Ambiguous edges are shown by default; `--hide-ambiguous` drops them.

This mirrors `code-review-graph`'s `resolve_bare_call_targets` but uses the richer ast-bro dep graph instead of just IMPORTS_FROM edges.

### Confidence

Every `CallEdge` carries one of:

| Tag | Meaning |
|---|---|
| `Exact` | Pass A or single-candidate pass B promotion. |
| `Inferred` | Pass C narrowed multiple candidates to one via dep closure. |
| `Ambiguous` | Pass C left more than one candidate. |

Renderers colour the tag (green / yellow / red) and downstream tooling can filter at the precision level it needs.

## Per-language extraction

Every source-code language adapter emits `Declaration::calls`. The SQL and Markdown adapters intentionally emit none. JavaScript uses the TypeScript adapter. Each participating adapter calls an `_extract_call_sites` or `_walk_calls_in_body` helper from its function, method, or constructor walker. The helper stops at nested type and callable declarations so each declaration owns only the calls in its body.

| Language | AST node kinds | `Construct` source | Notes |
|---|---|---|---|
| Rust       | `call_expression`, `macro_invocation`, `struct_expression` | struct literal | `super::` becomes `CallKind::Super` |
| Python     | `call` | class call (`Foo()`) | receiver from attribute access |
| TypeScript | `call_expression`, `new_expression` | `new T()` | also serves JavaScript |
| Java       | `method_invocation`, `object_creation_expression` | `new T()` | construct type stripped of generics + dotted prefix |
| C#         | `invocation_expression`, `object_creation_expression`, `implicit_object_creation_expression` | `new T()` | callee splitter handles `identifier`, `generic_name`, `member_access_expression`, `qualified_name`, `alias_qualified_name` |
| Kotlin     | `call_expression` | none (no `new`) | navigation_expression receiver via navigation_suffix |
| Scala      | `call_expression`, `instance_expression`, `generic_function` | `new T(...)` | receivers from `field_expression` |
| C++        | `call_expression`, `new_expression` | `new T()` | `qualified_identifier` / `scoped_identifier` split on `::`; `template_function` recurses into `name`; destructor names handled |
| Go         | `call_expression` | none (`new(T)` is just a regular call) | `selector_expression` for receivers |
| PHP        | `function_call_expression`, `member_call_expression`, `nullsafe_member_call_expression`, `scoped_call_expression`, `object_creation_expression` | `new T()` (last `\` segment of qualified type) | `\Foo\bar()` namespace-prefixed free function drops the namespace and emits the bare name with `receiver: None` so pass B promotes it; `self::` / `static::` / `parent::` keywords drop receiver (case-folded by tree-sitter-php's `keyword()` helper); dynamic `$func()` and `new $cls()` return `None` |
| Ruby       | `call` (with `method` / `receiver` fields) | `Foo.new` (constant receiver) | tree-sitter-ruby 0.23.1 represents `obj.method()`, `obj.method "x"`, `puts "hello"`, and `Greeter.shout` as `call`. The walker enters `block` and `do_block` because these closures use the enclosing method's scope. |
| Zig        | `call_expression`, `builtin_function`, `struct_initializer` | typed struct literal | preserves raw `field_expression` receiver text; records compiler builtins except dependency-only `@import`; functions, tests, `comptime` blocks, and local container methods own their direct calls |
| SQL        | n/a | n/a | intentionally emits no calls |
| Markdown   | n/a | n/a | intentionally emits no calls |

### Known per-language limitations

- **Ruby**: bare calls without parentheses or arguments, such as `helper`, parse as `identifier` rather than `call`. The grammar cannot distinguish them from local variable references.
- **Python**: the resolver has no Jedi-style receiver type inference. A call such as `obj.method()` whose receiver type depends on runtime flow falls through to passes B and C. Adding Jedi would require a Python runtime dependency.
- **Zig**: generated modules, C preprocessing, arbitrary comptime evaluation, and runtime-selected callbacks can remain unresolved. Literal `@call` and `@field` targets, lexical aliases, expected declaration-literal types, and simple generic factories are supported. See [Zig support](zig.md).
- **External-base ancestor walk**: `callees` on a type stops at depth 1 when a base type does not resolve to a project file. The graph cannot traverse source it cannot see.

## Unified graph cache

The call graph shares `.ast-bro/deps/graph.bin` with the dependency graph as `UnifiedGraph { deps, calls: Option<CallGraph> }`. The schema constant is `JSON_SCHEMA_GRAPH_INDEX = "ast-bro.graph-index.v4"`. The directory keeps its `deps/` name because changing the path would force a separate rebuild.

### Disk layout

```text
.ast-bro/
|-- .gitignore             # auto-written: "*"
|-- deps/
|   |-- graph.bin          # bincode CacheFile { schema, graph, files }
|   `-- lock               # fs2 advisory exclusive lock during writes
`-- index/                 # see search.md
    `-- ...
```

### Lazy promotion

`deps`, `reverse-deps`, `cycles`, and `graph` populate only the `deps` half, leaving `calls` as `None`. The first `callers` or `callees` invocation triggers `promote_calls`, which builds the call graph from the existing dependency graph without walking the project again. It then persists the upgraded `CacheFile`. Users who never query calls do not pay the call-graph build cost.

### Process-wide sharing

`src/graph_cache/shared.rs` holds a process-wide
`OnceLock<RwLock<HashMap<root, Entry>>>`, where each `Entry` pairs the parsed
`Arc<UnifiedGraph>` with the `FileRecord` fingerprints it was built from.
Within one process, every `tools/call` goes through `get_or_init`. This path is most useful to the long-running `ast-bro mcp` server. It performs three operations:

- It validates the cached graph against the working tree on every call. In the steady state, a stat-only `compute_delta` compares in-memory fingerprints without reading `graph.bin`. An unchanged tree returns the memoised `Arc` without parsing files again.
- When it detects an edit, it patches the in-memory graph through the same `apply_delta_*` functions used after a cold load and swaps the `Arc`. A long-lived session therefore reflects edits without `--rebuild`.
- It serialises the slower load, patch, and rebuild path behind a process-wide lock with a double-check. Concurrent callers cannot load the same stale graph and race on the disk write.

`Arc` swap on promotion or refresh means existing readers keep their prior
view safely. For one-shot CLI invocations the registry initialises, work
happens, the process exits.

### Schema migration

The legacy `deps-index.v1` cache was retired in v2.1.0, while the path remained `.ast-bro/deps/graph.bin`. The current schema is `ast-bro.graph-index.v4`. `cache::load_with_delta` compares the stored string with this value. Older schema values return `LoadOutcome::Missing`, and `load_or_build` rebuilds the cache in place.

Version 2 also fixes a bincode round-trip bug. `#[serde(skip_serializing_if)]` on `DepEdge::local_name`, `DepEdge::raw_path`, `CallEdge::receiver`, and `CallEdge::candidates` omitted positional fields and shifted the bytes that followed. Removing those annotations and rejecting v1 ensures the next graph query writes a complete cache.

Version 4 invalidates graphs produced before the Zig 0.16 grammar, lexical scope, and inline test fixes. Test callables retain `kind = test`; nested helpers inherit test status through their qualified ancestors.

### Per-file invalidation

`load_with_delta` returns a three-armed `LoadOutcome`:

- `Fresh(graph)`: cache with no file changes.
- `Stale { graph, delta, prev_records }`: cache with a per-file delta to apply.
- `Missing`: schema mismatch, I/O error, or decode error. The caller rebuilds.

`load_or_build` drives the patch flow: on `Stale`, hand the delta to two sibling patchers in `src/graph_cache/delta.rs`. On patch failure, fall back to full `build_and_save` so a query never sees a half-applied state.

**`apply_delta_to_deps`:**

1. Drop entries for removed + modified files.
2. Re-extract + re-resolve only added + modified files (parallel via rayon, same loop the full build uses).
3. Rebuild the suffix index once because file membership changed.
4. Re-aggregate stats.

**`apply_delta_to_calls`** (more careful since the call graph has cross-file edges):

1. Drop forward entries originating in changed files.
2. Drop changed-file qns from `callable_meta` / `types` / `symbol_table` / `type_by_name` / `implementors`. Prune empty buckets so callers don't see ghost keys.
3. Re-extract changed files via `pass::extract_file`.
4. Splice new qns into the live indices before resolving. Passes A and B must see qns added by the same delta.
5. Resolve only the new passes via `resolve::run_with_table` (the split-out resolver entrypoint that takes a prebuilt symbol table instead of constructing one).
6. Validate every `Resolved` edge against the updated qn set. If a target qn was deleted or renamed, demote its edge to `Bare` while preserving the callee name. An unchanged target keeps its `Exact` confidence, even when another declaration in the file changed.
7. After any source file changes, re-extract every unchanged Zig caller file and run the complete resolver on it. The persisted `CallGraph` does not contain import bindings, and a non-Zig edit can add a homonymous callable. The resolver also reparses unchanged intermediate Zig facades for their top-level aliases. This refresh preserves cold-build behavior for receiver namespaces, bare callable aliases, and rejected homonyms.
8. Re-resolve every remaining `Bare` edge against the updated symbol table. A receiverless single match promotes to `Resolved/Exact`, matching pass B. Other edges use the same `resolve::disambiguate` function as pass C, with a memoised dependency closure per file. This step finds targets moved to another file and targets added for bare edges in unchanged files.

   A partial update must produce the same graph as a cold build of the same content. The previous updater assigned each edge the unfiltered symbol-table candidates. On ast-bro, appending a comment to `src/adapters/sql.rs` increased `callers CliError.new` from 1,023 unresolved sites to 1,073 because every `Vec::new()` gained a candidate that the dependency filter had rejected. Reusing pass C prevents that divergence. `tests/calls_e2e.rs::incremental_update_matches_a_cold_build_of_the_same_content` checks this invariant.
9. Rebuild reverse adjacency + recompute stats. Both are derived; rebuilding fresh is cheaper than incremental maintenance.

### Cache timings

| operation                       | before    | after  |
|---------------------------------|-----------|--------|
| deps, cold                      | 2.85 s    | 2.85 s |
| deps, warm (no edits)           | 2.85 s    | 8 ms   |
| deps, warm + 1 file modified    | 2.85 s    | 22 ms  |
| callers, cold                   | 125 ms    | 125 ms |
| callers, warm (no edits)        | 125 ms    | 11 ms  |
| callers, warm + 1 file modified | 125 ms    | ~45 ms |

The before column's warm operations fell back to cold rebuilds because the v1 cache could not decode. In v2, the no-edit rows load from cache and the modified rows apply a per-file patch.

In `ast-bro mcp`, the in-process `Arc<UnifiedGraph>` shares parsed state across `tools/call` requests. Version 2 lets the first call load the persisted cache and lets later calls reflect file edits without `--rebuild`.

### Concurrency

Same pattern as the search index: `fs2` advisory exclusive lock at `.ast-bro/deps/lock` during writes; atomic `.tmp` + rename so a SIGKILL mid-write leaves the previous cache intact. Reads use the in-memory `Arc` and don't touch the lock.

## Known gaps

- The suffix index gets a fresh full walk on every delta. The walk is the cheap part of a cold build (hundreds of milliseconds even on large repositories). Changed files are the only files re-extracted by default. When the call graph exists, the patcher also re-extracts unchanged Zig callable files so import and callable aliases match a cold build. A surgical suffix-index update would add more complexity than it saves.

## Adding a new language

If you've already added a `Declaration`-emitting adapter (see [architecture.md](architecture.md)), call-site extraction is one helper and one wiring step:

1. Add an `_extract_call_sites` (or `_walk_calls_in_body`) function in your `src/adapters/<lang>.rs` that walks the function body, bails on nested type/callable declarations, and emits one `CallSite` per recognised AST node kind.
2. Call it from inside each `_function_to_decl` / `_method_to_decl` / `_class_to_decl` builder so the populated `calls` ride along on the returned `Declaration`.
3. If the language has its own import syntax not already covered by `src/deps/extract.rs` and `src/surface/imports.rs`, populate `ParseResult::imports` so pass A can resolve same-file `use` / `import` / `using` bindings.
4. Add an end-to-end test in `tests/calls_e2e.rs` that mirrors the existing per-language pair: `<lang>_callers_finds_intra_file_caller` and `<lang>_callees_lists_construct_and_invocation`. This pair exercises same-file pass A behavior without depending on import resolution.
5. If the language supports namespace import bindings, add a cross-file `helper.work()` test with same-name decoys. Assert that pass A resolves an existing direct file-scope callable with `Exact` confidence. Also test that a nested-only homonym or missing callable remains unresolved.

For languages where AST kind names are case-folded by tree-sitter (PHP's late-binding keywords, Ruby's command unification), pin the assumption with a regression test so future grammar drift surfaces as a test failure instead of silently dropping edges.
