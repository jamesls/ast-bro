# File filtering

`ast-bro` deliberately skips files while walking a directory. This page documents what gets included, what gets skipped, and how to override the filters. The same filters apply to `map`, `digest`, `show`, `implements`, `surface`, `deps`, `reverse-deps`, `cycles`, `graph`, `search`, `find-related`, and `index`.

## The five layers

Each file goes through five filter layers in order. The first to reject wins, and ast-bro never opens the file.

### 1. `.gitignore` (and friends)

ast-bro uses the [`ignore`](https://crates.io/crates/ignore) crate's `WalkBuilder`, which respects the same conventions `git` does:

- `.gitignore` at every level (root + nested directories)
- `.git/info/exclude`
- The user's global gitignore (`core.excludesfile`)
- `.ignore` files (the `ignore` crate's own convention: git itself doesn't read these, but `ripgrep`/`fd`/etc. do)
- The `.git/` directory itself

For most well-maintained repos this catches `node_modules/`, `dist/`, build outputs, etc.

### 2. Hardcoded denylist

Some repos forget to gitignore `node_modules/` (or are monorepos where it slipped through, or are vendoring deps). As a safety net, ast-bro always skips these directory names regardless of `.gitignore`:

```
.git .hg .svn .jj
__pycache__ .venv venv .tox
.mypy_cache .pytest_cache .ruff_cache
node_modules .next .nuxt .turbo .parcel-cache
dist build out .eggs target zig-out .zig-cache zig-cache
.cache .gradle .idea .vscode
.ast-bro .ast-outline
```

The list is in [`src/file_filter.rs`](../src/file_filter.rs): `HARDCODED_IGNORE_DIRS`. New entries should be:

- virtually never containing searchable user code
- huge enough to slow indexing meaningfully
- a stable, conventional name

### 3. `.ast-bro-ignore`

A `.ast-bro-ignore` file at any level is a per-repo escape hatch. `ignore::WalkBuilder::add_custom_ignore_filename(".ast-bro-ignore")` applies its gitignore syntax, including `!` un-ignore patterns and nested files in subdirectories.

Useful for excluding paths from ast-bro that you don't want excluded from git itself. For example:

```gitignore
# .ast-bro-ignore
tests/fixtures/large_corpus/
benches/data/
*.generated.rs
```

`tests/fixtures/large_corpus/` stays git-tracked but doesn't get walked when you map or search the repo.

### 4. Extension allowlist

`search`, `find-related`, `index`, and unified graph-cache fingerprints require an extension recognized by `chunker::is_indexable`. Graph fingerprints reuse `search::cache::compute_delta`, so this gate also decides whether an edit can invalidate dependency and call caches. A recognized file can use syntax-aware or plain-text chunking:

- Anything `ast-grep` can parse (`.rs`, `.py`, `.pyi`, `.ts`/`.tsx`/`.js`/`.jsx`/`.mjs`/`.cjs`, `.java`, `.cs`, `.go`, `.kt`/`.kts`, `.scala`/`.sc`, `.bash`/`.sh`, `.cpp`/`.hpp`/`.c`/`.h`, `.css`, `.dart`, `.ex`/`.exs`, `.hs`, `.hcl`, `.html`, `.json`, `.lua`, `.nix`, `.php`, `.rb`, `.swift`, `.yaml`/`.yml`, `.sol`)
- Native adapter formats: Markdown (`.md`, `.markdown`, `.mdx`, `.mdown`) and Zig (`.zig`). Search chunks Markdown at sections and chunks Zig structurally by declarations, nested members, and oversized callable bodies; both use direct tree-sitter parsers.
- Other plain-text formats with no tree-sitter grammar: `.toml` and PowerShell (`.ps1`/`.psm1`/`.psd1`), chunked at blank-line boundaries (LF or CRLF)

Unknown extensions such as `.txt`, image formats, and font formats are skipped before search opens them. Recognized generated or minified files are not excluded by name; ranking applies a penalty instead.

`map` / `digest` / `show` / `implements` use a narrower set: only the languages with a hand-written adapter at [`src/adapters/`](../src/adapters/) (Rust, Python, TS family, Java, C#, C++, Go, Kotlin, Scala, PHP, Ruby, SQL, Markdown, Zig). The chunker's broader set means search supports more languages than map does. See [architecture.md](architecture.md).

### 5. File-level guards

The search and graph-fingerprint walks skip files larger than `MAX_INDEX_FILE_BYTES`, currently 2,000,000 bytes. Search reads recognized files as UTF-8, so unreadable or non-UTF-8 content produces no chunks.

Generated names such as `*.min.js` and `generated/client.ts` remain indexable under the size cap but receive a strong ranking penalty. JSON lockfiles also remain indexable unless an ignore rule excludes them. These guards do not apply to map-family commands.

## Overlapping roots

The layers above decide *whether* a file is walked. A separate question is how
many times it is reported once it is, because `ignore::WalkBuilder` visits each
root independently and cannot tell that one root contains another. All three
of these name one file by two routes:

```bash
ast-bro map src ./src           # two spellings of one root
ast-bro run -p '...' src src/hot  # a root and a directory inside it
ast-bro show src/a.rs ./src Sym # an explicit file and a directory holding it
```

Comparing paths as walked doesn't catch any of it: results are rooted at the
spelling of the root they came from, so one file arrives as both `src/a.rs` and
`./src/a.rs`. (`PathBuf` equality compares *components*, so it folds an
interior `.` away by itself but keeps a leading `./`, a `..` traversal, and a
symlink.) `walk_paths` and `walk_and_parse` therefore post-filter their
collected results through `file_filter::file_identity`. This helper calls
`canonicalize`, which resolves `.`, `..` and symlinks against the filesystem rather than guessing
lexically. Lexical is the unsafe direction: collapsing `link/../a.rs` by string
surgery can declare two *different* files equal and drop one. When
`canonicalize` fails (a path that went away mid-walk) the spelling is used,
keeping a duplicate rather than losing a file.

It is a post-filter rather than a check inside the walk closure because the
closure runs on every parallel worker: a shared set would need a lock taken
once per file in *every* walk, to save duplicate work only in the rare
overlapping one.

Neither walk root is the authoritative spelling, so the tie-break selects the
shortest path. That picks the bare
form over `./`, the relative over the absolute, and the direct route over a
`..` detour, every time. Equal lengths fall back to sort order, so the result
is still deterministic.

```bash
ast-bro map ./src src      # displays src/a.rs
ast-bro map "$PWD/src" src # displays src/a.rs
```

`show` runs the same dedup across two pipelines. Explicitly named files are
parsed directly (a file you typed is a file you want, ignore rules
notwithstanding), while directories and globs go through the walker. No
single walk can see across them. Naming a file
alongside a directory containing it still cannot let the ignore rules hide it:
the routes are merged rather than chosen between, and only the walk filters.
When both reach the same file they produce the same parse, so shortest-wins
decides the display there too. This also keeps an explicitly typed
`src/a.rs` from being shown as the walk's `./src/a.rs`.

This was a correctness fix, not a cosmetic one: before it, `run --write` read
back its own output on the second visit and applied a re-matchable rewrite
twice, turning `log!("a")` into `log!(tag, tag, "a")`.

## Debugging excluded files

Quickest path:

```bash
# What does ast-bro actually walk?
ast-bro digest path/to/dir | head

# Compare against rg's ignore-respecting walk:
rg --files path/to/dir --no-ignore-vcs   # ignore .gitignore (still skips .git)
rg --files path/to/dir                   # respects .gitignore (closest baseline)
```

If `rg --files` shows a file but `ast-bro digest` doesn't:

1. Check whether the extension is in the allowlist for the command you're running.
2. Check for `.ast-bro-ignore` files at any level.
3. Check the hardcoded denylist (some directory in the path is in `HARDCODED_IGNORE_DIRS`).

If `ast-bro` shows a file but you want it excluded:

1. Add it to the per-repo, version-controlled `.ast-bro-ignore` file.
2. Or add it to `.gitignore` (also excludes from git).

## Trade-offs we've made

- **Hardcoded denylist over pure `.gitignore` reliance**: protects users with permissive `.gitignore` from accidentally indexing 1 GB of `node_modules`. A fresh repo cannot index its own `node_modules` through a directory walk, but an explicit path still works.
- **No CLI flag to disable the denylist in v1**: keeps the surface small. If you genuinely need to walk `node_modules`, point ast-bro at it directly: `ast-bro digest node_modules/some-package` (the denylist is component-based and only triggers when `node_modules` appears as an *intermediate* component).
- **Same filtering for map + search**: both use the shared walker, but a new language still needs shape-command routing and an `is_indexable` search entry.
