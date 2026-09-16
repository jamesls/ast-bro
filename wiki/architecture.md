# Architecture

`ast-bro` is a structurally aware code-navigation toolkit. Eight subsystems share one binary, common filtering primitives, and walk infrastructure:

1. **`src/adapters/` + `src/core.rs`**: language adapters parse files into a shared `Declaration` IR; renderers turn that into `map` / `digest` / `show` / `implements` output.
2. **`src/surface/`**: resolves the *true public API* of a package (`pub use`, `__all__`, TypeScript barrels, Scala `export`) instead of just listing every public item per file.
3. **`src/deps/`**: file-level dependency graph (`deps`, `reverse-deps`, `cycles`, `graph`) for 13 languages. See [deps.md](deps.md).
4. **`src/calls/`**: symbol-level call graph (`callers`, `callees`) for 13 source-code languages, with a three-pass resolver (same-file -> global symbol table -> dep-graph disambiguation). SQL and Markdown are intentional no-ops. See [calls.md](calls.md).
5. **`src/impact.rs`**: cross-file impact analysis (`impact`): callers + callees + file reverse-deps + test detection bundled into one "blast radius" report, with `--mode {deps,dependents,tests,all}` and `--tests` / `--exclude-tests` filters. See [impact.md](impact.md).
6. **`src/context.rs`**: token-budgeted context pack (`context`): greedy knapsack that assembles "everything the agent needs to understand symbol X" into a caller-supplied token budget. Works for both callable and type targets (type targets include implementors, methods, and method dependents). See [context.md](context.md).
7. **`src/search/`**: hybrid BM25 + dense semantic search, plus `find-related`. Cached at `.ast-bro/index/`. See [search.md](search.md).
8. **`src/squeeze/`**: reversible token compression for **logs/text** (`squeeze`). Its multi-stage pipeline shrinks repetitive lines and emits a legend so the output round-trips back to the original. Use `map` / `digest` / `show` to reduce code. See [squeeze.md](squeeze.md).

The dep graph and call graph share one on-disk cache at `.ast-bro/deps/graph.bin` (`UnifiedGraph { deps, calls: Option<CallGraph> }`) and a process-wide registry in `src/graph_cache/`. The registry keeps one entry per canonical repository root, revalidates it on every call, reuses its `Arc<UnifiedGraph>` while the tree is unchanged, and swaps in a patched `Arc` after an edit. `impact` and `context` use the same unified cache.

Most adapters use the [tree-sitter](https://tree-sitter.github.io/tree-sitter/) parsers exposed by [`ast-grep`](https://ast-grep.github.io/). Markdown and Zig instead use a raw `tree_sitter::Parser`, while SQL uses a regex parser. Zig structural search wraps the same bundled grammar in ast-grep's `Language` trait. See [Zig support](zig.md) for coverage and static analysis limits. Directory walks use `rayon` for parallel work.

The walking subsystems share ignore handling and the hardcoded denylist in `src/file_filter.rs`, but their extension gates differ. Shape commands use `can_parse_for_hook`, dependencies use `deps::resolver::Lang::from_path`, and search plus graph fingerprints use `search::chunker::is_indexable`. See [file-filtering.md](file-filtering.md). The `squeeze` command is the exception because it reads one explicit file directly. `file_filter.rs` also defines `is_test_file`, shebang-based `detect_language`, and the canonical-path `file_identity` key used to deduplicate overlapping walk roots.

## Shape command flow

1. **Routing (`src/main_helpers.rs`)**: `parse_file_for_hook` routes SQL, Markdown, and Zig by extension before it asks `SupportLang::from_path`. Every route calls `populate_markers` after parsing.
2. **Parsing (`src/adapters/*`)**: Most adapters traverse `ast_grep_core::Node`s. Markdown and Zig traverse raw `tree_sitter::Node`s, and SQL parses source text with regular expressions.
3. **IR generation (`src/core.rs`)**: Each adapter emits the shared `Declaration` tree, including kinds, names, signatures, docs, visibility, calls, and source ranges.
4. **Rendering (`src/core.rs`)**:
   - `map` iterates the declarations to print a hierarchical file breakdown.
   - `digest` squashes the tree into a concise module-level API map.
   - `show` walks the tree for a specific suffix match and extracts the raw string boundaries. Target resolution and rendering live in [`src/show.rs`](../src/show.rs), shared by the CLI and the MCP tool so the two cannot drift: a target is a file, a directory, or a quoted glob, and `show::partition` decides where the target list ends and the symbol list begins by asking the filesystem rather than by argument position. That is what recovers an unquoted glob: the shell expands `show src/*.cs Widget` before the process starts, and the extra files used to land in the symbol slot, resolving the symbol against the first file alone and silently dropping its other definitions at exit 0. Explicitly named files are parsed directly (a file you typed is a file you want, ignore rules notwithstanding); directories and globs go through `walk_and_parse`, so they honour the same filter pipeline as `map`. Multi-file answers carry a coverage header and a `--limit` display cap; a single explicit file keeps its historical uncapped, headerless output.
   - `implements` performs a generic Breadth-First-Search across the IR trees of the entire repository to find inheritance hierarchies.
   - `--json` is the fifth rendering mode: any of the above commands accepts `--json` to serialise the same `Declaration` IR directly via `serde_json` into a versioned JSON schema, instead of formatting it as text. Add `--compact` for single-line output.

The `surface` and `calls` subsystems reuse the `Declaration` IR. The call graph extends `Declaration` with a `calls: Vec<CallSite>` field and `ParseResult` with `imports: Vec<ImportBinding>` so adapters can populate raw call sites and import bindings during their existing tree walk. Dependencies instead build `RawImport` and `DepGraph` values, while search builds `Chunk` values. These subsystems share file-filtering primitives rather than one IR. The call resolver lives in `src/calls/resolve.rs`; see the dedicated wiki pages for the other internals.

## CLI structure and the error contract

Every operation is an explicit subcommand. Bare `ast-bro` prints help to stdout and exits 0 because no arguments constitute a help request.

Everything else follows one contract (issues #33/#36), implemented in `src/cli_error.rs` and applied by every subcommand:

- **Channel**: stdout carries results only. Every rejection, note, hint, and warning goes to stderr, which keeps `--json` output parseable without preprocessing and makes "stdout is empty" a reliable signal on its own.
- **Exit code**: `0` means the query ran, even if the answer is empty. Qualifications such as unresolved paths, display caps, and depth cutoffs appear as `# note:` messages on stderr. `2` means the query could not run as asked and writes nothing to stdout. `1` means an internal failure such as a parse crash or unreadable cache.
- **Machine-readable form**: with `--json`, a rejection also emits an `ast-bro.error.v1` object on stderr (`{schema, command, kind, detail, hint}`). `kind` is one of `no_input | path_not_found | symbol_not_found | unknown_flag | bad_argument | index_error`, so a consumer needs one check instead of a per-subcommand table.
- **Unknown flags**: exit 2 with clap's error on stderr. If the flag exists on a sibling subcommand, the message names that subcommand (`--glob is a map flag`). Help on stdout is reserved for `--help`.
- **Notes beside a real result**: qualifications of a delivered answer (partial path misses, truncation notes, ambiguity counts) stay exit 0, message on stderr, result on stdout.

The recovery rule an agent needs is one sentence: *stdout empty or exit non-zero -> the call was wrong; read stderr; fix the call.*

The same reasoning applies to *coverage*. An empty answer states how much ground it covers so a caller can distinguish zero matches from an empty scope. `run` reports `no matches for "<pattern>" (N file(s) scanned)` on stderr and includes `files_scanned` in JSON. A multi-file `show` reports `N match(es) ... in M of K file(s) searched` and includes `files_scanned` in the `symbol_not_found` envelope. `implements` distinguishes "no such type" (exit 2) from "type exists, zero implementors" (exit 0).

A rejected path offers a repair only when the filesystem can verify it, such as the same basename elsewhere or a quoted path that the shell split at spaces. [`src/path_repair.rs`](../src/path_repair.rs) implements these hints for `require_paths`, `require_path`, `show`, `find-related`, and the MCP path resolver. Without evidence, the command omits the hint.

Capped output is never silent (issue #32). `callers`, `callees`, `impact`, and `reverse-deps` headers report the true total on stdout when `--limit` trims the display. JSON carries `total` and `truncated`, while `map --max-members` prints a `+N more` line. Every section of a multi-section answer counts toward those values and shares the display budget. `--limit` bounds display rather than work, so reverse walks still traverse the full cone to calculate an exact total.

`--depth` can also make an answer partial. Each depth-bounded command reports this as `frontier_truncated` in JSON, a stderr note in text mode, and a line in the single-channel MCP response. `callers`, `callees`, `deps`, `reverse-deps`, and `trace` use a top-level field; each `impact` report carries its own field. This flag is independent of display truncation. For example, `truncated: false` with `frontier_truncated: true` means the command displayed every result it found within the requested depth.

Only an edge to an unvisited node sets `frontier_truncated`; an edge back to a visited node does not. `callers`, `callees`, and `impact` omit the stderr note at depth 1 but still set the JSON field. If a depth limit stops `trace`, it reports that no path was found within the requested depth instead of claiming the symbols have no static connection.

`map` and `digest` use the same walk and `Declaration` IR (issue #37), with byte-identical `--json` output. `map` exposes detail (`--detail names|signatures|full`), visibility (`--no-private`, `--no-fields`, ...), and scope (`--glob`, `--max-members`) as independent controls. `digest` is an alias for `map --preset digest` (`--detail names --no-private --no-fields --max-members 50`), and explicit flags override the preset. Detail levels below `full` also omit doc comments from JSON.

## MCP server (`src/mcp/`)

`ast-bro mcp` runs the binary as a [Model Context Protocol](https://modelcontextprotocol.io) server so coding agents can invoke the same operations as native tools. The implementation is intentionally tiny:

- **Transport**: line-delimited JSON-RPC 2.0 on stdin/stdout with no tokio or other runtime dependencies. It adds about 600 KB to the binary and does not run during regular CLI commands.
- **`src/mcp/protocol.rs`**: serde types for `Request`/`Response`/`RpcError` and the standard JSON-RPC error codes.
- **`src/mcp/tools.rs`**: declares nineteen tool schemas (`map`, `digest`, `show`, `implements`, `callers`, `callees`, `trace`, `surface`, `impact`, `context`, `squeeze`, `deps`, `reverse_deps`, `cycles`, `graph`, `search`, `find_related`, `index`, `run`) and dispatches `tools/call` into the existing `core::render_*` / `calls::*` / `surface::*` / `impact::*` / `context::*` / `deps::*` / `search::*` functions. Each tool maps 1:1 to a CLI subcommand and reuses its render logic byte-for-byte, so the JSON schemas are shared with the CLI's `--json` output. The defaults are shared the same way, from [`src/defaults.rs`](../src/defaults.rs): clap reads the constant (`default_value_t = defaults::LIMIT`), serde reads a thin function around it, and the schema interpolates it into the property description, so the number an agent is told is the number the tool applies.
- **`src/mcp/mod.rs`**: read loop, method routing (`initialize`, `ping`, `tools/list`, `tools/call`, `resources/list`, `prompts/list`), and panic-safe tool dispatch (panics are surfaced as `-32603 internal error` instead of taking the server down).

Tools return text by default because the agent prompt uses that form. Clients can request structured output with `json: true`.

## Adding a new language

Adding a language requires an adapter plus explicit gates in each subsystem that should process its files.

1. Check whether `ast-grep` exposes the language through `SupportLang`. If it does, implement `LanguageAdapter` over `ast_grep_core::Node`. If it does not, use a free-function adapter. Markdown and Zig each create a raw `tree_sitter::Parser` and walk `tree_sitter::Node`s; SQL parses source text with regular expressions.
2. Add `src/adapters/mylang.rs` and export it from [`src/adapters/mod.rs`](../src/adapters/mod.rs). Convert native declarations into the shared `Declaration` IR.
3. Route the extension in both `can_parse_for_hook` and `parse_file_for_hook` in [`src/main_helpers.rs`](../src/main_helpers.rs). A language outside `SupportLang` needs an early extension branch, and that branch must call `populate_markers` after parsing.
4. If the dependency graph supports the language, add its extension to `deps::resolver::Lang::from_path` and implement extraction and resolution rules. The `Lang` gate controls which files enter the dependency graph.
5. Add the extension to `search::chunker::is_indexable` with an AST, Markdown, or plain-text chunking strategy when search or the dependency graph supports it. This gate serves semantic indexing and unified graph-cache fingerprints because `graph_cache::collect_file_records` reuses `search::cache::compute_delta`. A dependency language must pass this gate even when its first chunker is plain text.
