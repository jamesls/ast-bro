# Dependency graph

The dependency subsystem resolves file imports for 13 user-facing languages: Rust, Python, TypeScript, JavaScript, Scala, Java, Kotlin, C#, Go, C++, PHP, Ruby, and Zig. TypeScript and JavaScript count separately; TSX uses the TypeScript path internally.

Four subcommands expose the file graph: `deps`, `reverse-deps`, `cycles`, and `graph`. The same graph supports `callers`, `callees`, `trace`, `impact`, `context`, and dependency-aware `find-related` ranking. See [file-filtering.md](file-filtering.md) for walk rules, [calls.md](calls.md) for the symbol graph, and [search.md](search.md) for semantic search.

## Pipeline

```text
graph_cache::shared::get_or_init(root):
  check the per-process registry
  compute_delta(root, recorded_files)
  reuse, patch, load, or build the unified graph

build_graph(root):
  detect_aliases(root)              # tsconfig, Cargo, and Composer metadata
  build_suffix_index(root)          # one filtered project walk
  par_iter(files):                  # per-file extraction and resolution
    raw = extract(file, lang)
    for import in raw:
      resolve(import.spec, ctx, idx)
        -> match -> DepEdge
        -> no match -> external bucket
  dedup_edges(graph)
  -> DepGraph { forward, external, stats }

deps <file>:                        forward BFS
reverse-deps <file>:                reverse adjacency plus BFS
cycles:                             iterative Tarjan SCC
graph:                              full graph renderer
```

`build_graph` stores only forward edges. `reverse_adjacency` computes the reverse map when a caller needs it. This keeps one edge source of truth and avoids maintaining two maps during incremental updates.

## Module layout

```text
src/deps/
|-- mod.rs                 graph construction
|-- options.rs             DepOptions and DepError
|-- graph.rs               DepGraph, DepEdge, ImportKind, dedup_edges
|-- extract.rs             per-language import extraction
|-- resolver/
|   |-- build.rs           Lang, SuffixIndex, build_suffix_index
|   |-- resolve.rs         shared resolver
|   `-- mod.rs             re-exports
|-- manifest.rs            tsconfig, Cargo, Composer, and go.mod parsing
|-- scc.rs                 iterative Tarjan SCC
|-- traverse.rs            forward, reverse, and neighbourhood walks
|-- dsm.rs                 dependency structure matrix
|-- render.rs              text and JSON renderers
`-- cli.rs                 command dispatch

src/graph_cache/
|-- mod.rs                 UnifiedGraph and call-graph promotion
|-- cache.rs               CacheFile persistence
|-- delta.rs               per-file dependency and call graph patches
`-- shared.rs              per-process registry keyed by repository root
```

Cache code lives in `src/graph_cache`. The MCP handlers live in `src/mcp/tools.rs`.

## Depth cutoff

`--depth` bounds the forward and reverse walks. A traversal that stops at the cutoff can otherwise look like one that reached the graph's end. `forward_info` and `reverse_info` return `DepTraversal { hits, frontier_truncated }` so renderers can distinguish those outcomes.

`frontier_truncated` becomes true when a file at the cutoff has an edge to an unseen file. An edge back to a visited file does not count. A predicate-rejected edge does count because filters such as `--exclude-tests` control reporting, while traversal can still continue through the rejected file.

`frontier_truncated` is separate from `truncated`. The first describes the `--depth` boundary. The second reports a `--limit` display cap. Text commands print `render::frontier_note` on stderr. MCP prepends the same note to its single response channel.

## Suffix index

`build_suffix_index` walks every supported source file and maps each extensionless path suffix to an absolute path:

```text
src/deps/cli.rs -> "src/deps/cli", "deps/cli", "cli"
```

The index uses `HashMap<String, Vec<PathBuf>>` because one repository can contain several files with the same suffix. `pick_closest` chooses the candidate that shares the most leading path components with the importer, then breaks ties lexicographically.

The walk adds language metadata beyond ordinary path suffixes:

| Language | Extra metadata or index entries |
|---|---|
| Python | `__init__.py` also indexes its package directory and final package name. |
| Java, Kotlin, Scala, C# | Each top-level type adds a `<package>/<TypeName>` entry. |
| Go | Every `go.mod` contributes a module path and its repository-relative directory to `SuffixIndex::go_modules`. |

A cold graph build runs the filtered suffix-index walk once. A dependency delta patch rebuilds the index when added or modified files need extraction. Per-file extraction and resolution then run in parallel with rayon.

## Import extraction

`src/deps/extract.rs` dispatches on `Lang` and returns `Vec<RawImport>`. Each record carries a normalized `spec`, `ImportKind`, source line, original path, and optional local binding.

| Language | Extracted forms |
|---|---|
| Rust | `use` trees and external `mod` declarations, including `#[path]`. |
| Python | `import`, `from ... import`, aliases, globs, and `__all__`. |
| TypeScript | Imports, re-exports, top-level `require()` calls, named bindings, and namespace bindings. TSX uses this extractor. |
| JavaScript | Imports, re-exports, top-level `require()` calls, named bindings, and namespace bindings. |
| Scala | Import declarations, selector lists, aliases, and globs. |
| Java | Regular and static imports, globs, and nested-type paths. |
| Kotlin | Import directives, `as` aliases, and globs. |
| C# | `using` directives, aliases, static imports, and namespace bodies. |
| Go | Single and grouped import specifications. |
| C++ | Quoted and angle-bracket `#include` directives, including includes inside common preprocessor wrappers. |
| PHP | Namespace `use` forms, grouped imports, aliases, and literal `require` or `include` variants. |
| Ruby | Literal `require`, `require_relative`, `load`, and `autoload` calls. |
| Zig | Literal `@import` AST nodes, including multiline calls and decoded strings. |

Zig preserves import bindings for both graph layers. For `const helper = @import("util/helper.zig");`, dependency extraction records `local_name = "helper"` and normalizes the file spec to `./util/helper.zig`. The Zig adapter also emits `ImportBinding { local: "helper", module: "./util/helper.zig" }`. Call resolution maps `helper.work()` to `helper.zig::work` only when the imported file declares `work` at file scope. `@embedFile` is an asset reference and is not extracted.

## Resolution rules

`resolve(spec, ctx, idx)` handles all 13 languages. Specs beginning with `./` or `../` first use path arithmetic relative to the importing file.

- Rust strips `crate::` and uses suffix lookup with trailing-segment fallback. `self::` and repeated `super::` prefixes resolve from the importer's directory.
- Python relative imports probe `.py`, `.pyi`, and `__init__.py`. If the final imported name is not a file, the resolver drops that segment and retries the containing module.
- TypeScript relative imports probe `.ts`, `.tsx`, `.mts`, `.cts`, `.d.ts`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.json`, and matching `index` files. Bare imports can use root `tsconfig.json` `compilerOptions.paths` aliases.
- JavaScript uses the same file probing and `tsconfig.json` alias rules as TypeScript.
- Java, Kotlin, Scala, and C# convert dotted names to slash paths and query the type-augmented suffix index. A miss drops the final segment once to handle nested types.
- Go strips a matching `go.mod` module prefix and locates a source file in the target package directory. The resolver prefers the deepest module directory that contains the importer, then the longest module path, then the closest copy. Imports outside known module prefixes remain external.
- C++ resolves quoted includes relative to the importer. Angle-bracket system headers remain external.
- PHP resolves literal relative `require` and `include` paths directly. Namespace imports try Composer PSR-4 prefixes, direct suffix lookup, and a final class-name fallback.
- Ruby resolves only `require_relative`. Extraction adds `.rb` when the source omits it. Bare `require`, `load`, and `autoload` remain external because they depend on `$LOAD_PATH` or installed gems.
- Zig resolves relative `.zig` and `.zon` imports and unambiguous literal module wiring in `build.zig`. A spec such as `@import("helper.zig")` becomes `./helper.zig` and must name an existing file. See [Zig support](zig.md) for the supported build forms and remaining limits.

Unresolved imports stay in `DepGraph::external` with their source spelling. They do not become edges to a same-named local file by guesswork.

## Cycle detection

`src/deps/scc.rs` runs Tarjan's strongly connected components algorithm with an explicit work stack.

- Components with more than one member are cycles.
- A one-member component is a cycle only when the file has a self-edge.
- Other one-member components are discarded.

Cycles sort by member count in descending order. Members within each cycle sort lexicographically, which keeps text and JSON output stable.

## Unified cache

`src/graph_cache/cache.rs` stores a bincode-encoded `CacheFile` at `.ast-bro/deps/graph.bin`:

```text
CacheFile {
  schema: String,
  graph: UnifiedGraph {
    deps: DepGraph,
    calls: Option<CallGraph>,
  },
  files: Vec<FileRecord>,
}
```

The dependency graph is always present. The call graph starts as `None` and `promote_calls` builds and persists it when a symbol query first needs it.

The cache wrapper schema is `ast-bro.graph-index.v4`. The loader treats older schemas as a mismatch and performs a cold rebuild. Version 4 invalidates graphs created before Zig 0.16 grammar, lexical scope, and inline test fixes. The separate `DepGraph.schema` field still identifies the dependency payload; it does not replace the wrapper version check.

`search::cache::compute_delta` compares the recorded files with the working tree. It uses path membership plus mtime and size checks, and hashes a file when metadata changes. A stale cache goes through `apply_delta_to_deps`. That function removes entries for changed files, rebuilds the suffix index, re-extracts added or modified files, and recomputes dependency statistics. If `UnifiedGraph.calls` is present, `apply_delta_to_calls` patches the symbol graph against the updated dependency graph. A dependency patch failure falls back to a cold build.

`graph_cache::shared` keeps an `Arc<UnifiedGraph>` and its `FileRecord` list in a process-wide registry keyed by canonical repository root. Every `get_or_init` call runs `compute_delta` before it reuses an entry. Changed files produce a patched graph and a new `Arc`, so a long-lived MCP session does not freeze the first graph it loaded.

Writes hold an `fs2` advisory lock at `.ast-bro/deps/lock` and replace `graph.bin` through a temporary file. The graph cache also creates `.ast-bro/.gitignore` with `*` when needed. `--rebuild` bypasses the saved graph and builds a new dependency half.

## find-related boost

`Index::find_related_opts` uses dependency distance to rerank semantic neighbors when `dep_boost` is enabled.

1. Resolve the source chunk and restrict candidates to the same language and optional query scope.
2. Pull up to `top_k * 5` candidates by cosine similarity.
3. On the first boosted query for an `Index`, `dep_graph_cached` calls `graph_cache::shared::get_or_init`. That call can reuse a fresh graph, patch a stale graph, load `graph.bin`, or build a missing graph.
4. Cache the resulting `DepGraph` clone for the lifetime of the `Index`. If graph loading or construction fails, continue without the dependency boost.
5. Run `neighbourhood_depths` over forward and reverse adjacency.
6. Multiply depth-1 scores by `1.40` and depth-2 scores by `1.20`. Other distances keep their original scores.
7. Sort again and truncate to `top_k`.

Use `--no-dep-boost` to skip graph access. `--dep-depth N` changes the neighborhood depth.

## On-disk files

```text
.ast-bro/
|-- .gitignore             "*"
|-- deps/
|   |-- graph.bin          CacheFile { schema, graph, files }
|   `-- lock               fs2 advisory lock
`-- index/                 semantic search index
```

The graph file contains both graph layers. The directory keeps its historical `deps` name so existing repositories do not move the cache to a second location.

## Adding a language

1. Add a `Lang` variant and extension cases in `src/deps/resolver/build.rs::Lang::from_path`.
2. Add an extraction arm in `src/deps/extract.rs::extract`. Preserve the source path and local binding when the language exposes one.
3. Add resolver logic in `src/deps/resolver/resolve.rs`. Extend `src/deps/manifest.rs` and `SuffixIndex` when resolution depends on project metadata.
4. Add the extension to `search::chunker::is_indexable`. Graph fingerprints come from `search::cache::compute_delta`, which only tracks files that `is_indexable` recognizes. Missing this step makes edits invisible to cache invalidation.
5. If calls should cross imports, make the language adapter emit matching `ImportBinding` records. See [calls.md](calls.md) for call-site extraction and resolution.
6. Add fixtures under `tests/fixtures/deps/` and integration coverage in `tests/deps_e2e.rs`. Include forward, reverse, external, and cycle cases that fit the language.
7. Add an incremental-cache regression that edits a file and reruns a graph query without `--rebuild`.

Java, Kotlin, Scala, and C# share the fully qualified type index. A new language with the same import model can reuse `extract_package_and_types` and the matching resolver branch.
