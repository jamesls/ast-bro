# ast-bro

[ast-bro](https://github.com/aeroxy/ast-bro) is an AST-based **code-navigation toolkit**. It maps file structure, resolves public APIs, builds dependency and call graphs, searches by symbol or behaviour, estimates change impact, packs context to a token budget, and compresses repetitive logs. Nineteen analysis subcommands serve coding agents and humans from one binary.

[ast-bro](https://github.com/aeroxy/ast-bro) is written in Rust and uses [tree-sitter](https://github.com/tree-sitter/tree-sitter), usually through [ast-grep](https://github.com/ast-grep/ast-grep)'s bindings. [rayon](https://github.com/rayon-rs/rayon) parses workspace files concurrently. Large monorepos can add the abstraction layer provided by [repolayer](https://github.com/zhousiyao03-cyber/repolayer).

[![crates.io](https://img.shields.io/crates/v/ast-bro.svg)](https://crates.io/crates/ast-bro)
[![npm](https://img.shields.io/npm/v/@ast-bro/cli)](https://www.npmjs.com/package/@ast-bro/cli)
[![PyPI](https://img.shields.io/pypi/v/ast-bro)](https://pypi.org/project/ast-bro/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/aeroxy/ast-bro)

> **Renamed from `ast-outline` (v2.1.x and earlier).** The project now includes dep graphs, call graphs, hybrid semantic search, public API resolution, and structural search and rewrite. The old name also collided with an unrelated [VS Code extension](https://marketplace.visualstudio.com/items?itemName=cancerberosgx.vscode-typescript-ast-outline) and an [npm package](https://www.npmjs.com/package/ast-outline).
>
> **Upgrading from `ast-outline`?** Run any `ast-bro` command once and it will auto-migrate `.ast-outline/` -> `.ast-bro/` (cache), `.ast-outline-ignore` -> `.ast-bro-ignore` (per-repo filter), `~/.cache/ast-outline/` -> `~/.cache/ast-bro/` (model cache), and any `ast-outline` entries in your MCP config -> `ast-bro`. The legacy `ast-outline` binary is still installed as a thin proxy that execs into `ast-bro`, and a shorter `sb` alias ships alongside, so existing scripts keep working.

---

## Purpose

**[ast-bro](https://github.com/aeroxy/ast-bro) exists to make LLM coding agents faster, cheaper, and smarter
when navigating unfamiliar code.**

Modern coding agents explore codebases by reading files directly. On a 1000-line file, an agent consumes 1000 lines of tokens to answer *"what methods exist here?"* Questions about imports, public APIs, cycles, and feature locations can require dozens of file reads or noisy `grep` results.

[ast-bro](https://github.com/aeroxy/ast-bro) collapses each of those questions into a single command:

1. **Shape over bytes.** `map` / `digest` / `show` give you signatures and line ranges instead of method bodies, typically saving **95% of the tokens** required to read the file. `implements` finds subclasses without `grep` false positives.
2. **Published API in one call.** `surface` resolves `pub use` re-exports (Rust), `__all__` (Python), barrel files (TypeScript), `export` clauses (Scala), and public `const` facades plus legacy `pub usingnamespace` composition (Zig), so you see the API available to downstream users.
3. **Dependency graph for free.** `deps` / `reverse-deps` / `cycles` / `graph` build a file-level import graph for 13 languages: Rust, Python, TypeScript, JavaScript, Java, C#, Kotlin, Scala, Go, C++, PHP, Ruby, and Zig. The shared cache lives at `.ast-bro/deps/graph.bin`. Use `reverse-deps` before refactoring to find affected files. `cycles` exits non-zero for a CI gate, and `graph` emits text or JSON.
4. **Symbol-level call graph.** `callers` / `callees` answer "who calls X" and "what does X call" across 13 source-code languages without matches from comments or string literals. SQL and Markdown are shape-only adapters and do not emit call edges. Both commands are kind-aware: functions return call sites, while types return implementors, constructions, or ancestors. A three-pass resolver (same-file -> global symbol table -> dep-graph disambiguation) tags every edge `Exact`, `Inferred`, or `Ambiguous`. `trace <FROM> <TO>` returns the shortest static call path between two symbols and includes each hop's body. The call graph uses the same on-disk cache as the dep graph.
5. **Hybrid semantic search.** `search` runs BM25 + dense embeddings via [`potion-code-16M`](https://huggingface.co/minishlab/potion-code-16M) (a static, no-inference model: ~64 MB, runs on CPU in microseconds). `find-related` returns structurally similar chunks and loads the dependency graph lazily for neighborhood-aware ranking.
6. **Blast radius in one shot.** `impact <symbol>` combines callers, callees, file-level deps, reverse-deps, transitive callers at `--depth N`, and test detection. It replaces four round-trips with one call. Its modes are `all` (default), `deps`, `dependents`, and `tests`; `--tests` / `--exclude-tests` narrow the filter. It works for callables and types.
7. **Token-budgeted context.** `context <symbol> --budget N` packs "everything an LLM needs to understand this symbol" into a caller-supplied token budget: target body first, then direct callees (bodies while budget permits, signatures otherwise), direct callers (signatures), transitive callees/callers at depth 2 (signatures only). For types: type body, implementors, methods, callers-of-methods. Flags `truncated` when budget ran short and `target_omitted` when even the target body didn't fit. Same data as four or five `show`/`callers`/`callees` calls, one round-trip, budget-bounded.
8. **Squeeze logs, not just code.** `squeeze` compresses repetitive logs and text into a reversible legend plus short tags. It returns the raw text when compression would not help. Use `map` / `digest` / `show` to reduce code instead.
9. **Nineteen native MCP tools.** Every analysis command is also exposed as an MCP tool: `ast-bro install --target <agent> --mcp` wires it into Claude Code, Cursor, Gemini, Codex, OpenCode, or VS Code Copilot in one line.

### The workflow

**Before [ast-bro](https://github.com/aeroxy/ast-bro):**

```
Agent: Read Player.cs            # 1200 lines of tokens
Agent: Read Enemy.cs             # 800 lines of tokens
Agent: Read DamageSystem.cs      # 400 lines of tokens
Agent: grep "IDamageable" src/   # noisy, lots of false matches
...
```

**With [ast-bro](https://github.com/aeroxy/ast-bro):**

```console
Agent: ast-bro surface .                  # one-page true public API of the crate/package
Agent: ast-bro digest src/Combat          # ~100 lines, whole module's structure
Agent: ast-bro implements IDamageable     # precise list, no grep noise
Agent: ast-bro search "damage handling"   # hybrid BM25 + dense semantic, ranked
Agent: ast-bro show Player.cs TakeDamage  # just the method body
Agent: ast-bro reverse-deps Player.cs     # who imports this: blast radius before refactor
Agent: ast-bro callers Player.TakeDamage  # AST-accurate call sites: no grep false positives
Agent: ast-bro callees Player.TakeDamage  # what TakeDamage itself calls
Agent: ast-bro impact Player.TakeDamage   # callers + callees + file deps + tests, one call
Agent: ast-bro context Player.TakeDamage --budget 2000  # everything an LLM needs, token-bounded
Agent: ast-bro cycles src/                # find import cycles via Tarjan SCC
```

The agent gets the same structural information with fewer tokens and round-trips. `surface` can replace dozens of reads when determining a package's public API. `callers` returns call sites for a method without matching homonyms elsewhere in the repository.

---

## Supported languages

| Language | Extensions |
| --- | --- |
| Rust       | `.rs` |
| C#         | `.cs` |
| C++        | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh` |
| Python     | `.py`, `.pyi` |
| TypeScript | `.ts`, `.tsx` |
| JavaScript | `.js`, `.jsx`, `.mjs`, `.cjs` |
| Java       | `.java` |
| Kotlin     | `.kt`, `.kts` |
| Scala      | `.scala`, `.sc` |
| Go         | `.go` |
| PHP        | `.php` |
| Ruby       | `.rb` |
| SQL        | `.sql`, `.ddl`, `.dml` |
| Zig        | `.zig` |
| Markdown   | `.md`, `.markdown`, `.mdx`, `.mdown` |

This table lists the 15 shape-command adapters. The dependency and call graphs cover the 13 source-code languages; SQL and Markdown do not emit graph edges. `run` supports ast-grep languages plus the bundled Zig grammar. SQL and Markdown remain unavailable for structural search and rewrite. See [Zig support](wiki/zig.md) for syntax coverage and static analysis limits.

Adding another `ast-grep` language starts with a new adapter file. Languages outside `ast-grep` also need native parser routing; see the [architecture guide](wiki/architecture.md#adding-a-new-language).

For Markdown the "symbols" are headings and fenced code blocks, plus a leading
YAML frontmatter block: `--- frontmatter` shows up in `map` and `digest` with
its line range, and `frontmatter` is a `show` handle
(`ast-bro show tasks/ frontmatter` collects every card's block in one call).
Only a `---` fence on the file's first line counts: a `---` further down stays
an ordinary horizontal rule. Trailing whitespace on either fence and a leading
UTF-8 BOM are tolerated; `...` closes a block as well as `---`. TOML
frontmatter (`+++`) is not surfaced.

---

## What gets walked

[ast-bro](https://github.com/aeroxy/ast-bro) deliberately skips files when walking a directory. Filters apply uniformly across directory-walking subcommands.

1. **`.gitignore` and friends**: every level's `.gitignore`, your global gitignore, `.git/info/exclude`, and `.ignore` files (the [`ignore`](https://crates.io/crates/ignore) crate's convention used by `ripgrep`/`fd`).
2. **Hardcoded denylist**: directories almost no one wants walked, even if `.gitignore` doesn't list them: `.git`, `node_modules`, `target`, `dist`, `build`, `__pycache__`, `.venv`, `venv`, `.cache`, `.idea`, `.vscode`, `.next`, `.nuxt`, `.turbo`, `.parcel-cache`, `.gradle`, `.tox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.eggs`, `.ast-bro`, and a few others.
3. **`.ast-bro-ignore`**: per-repo escape hatch. Same syntax as `.gitignore`. Useful for excluding paths from [ast-bro](https://github.com/aeroxy/ast-bro) that you *don't* want excluded from git itself, e.g. test fixtures or vendored corpora:

   ```gitignore
   # .ast-bro-ignore
   tests/fixtures/large_corpus/
   benches/data/
   *.generated.rs
   ```
4. **Extension allowlist**: files are only opened if ast-bro knows their extension (the table above for map/digest/show/implements; a broader set for search). Explicitly passed extensionless files fall back to shebang detection (`#!/usr/bin/env python3` -> Python, `#!/usr/bin/ruby` -> Ruby, `#!/usr/bin/env node` -> TypeScript, etc.). Directory walks do **not** open extensionless files; only explicit inputs use shebang detection.

Compare `ast-bro digest some/dir` with `rg --files some/dir` to inspect the walk. A path that appears only in `rg` was removed by one of the filters above.

---

## Install

### Homebrew (macOS)

```bash
brew install aeroxy/tap/ast-bro
```

### npm

```bash
npm install -g @ast-bro/cli
```

### pip

```bash
pip install ast-bro
```

### Cargo

```bash
cargo install ast-bro
```

This installs the [ast-bro](https://github.com/aeroxy/ast-bro) CLI globally into `~/.cargo/bin`, so make sure that directory is on your `PATH`.

### Nix

You can run [ast-bro](https://github.com/aeroxy/ast-bro) directly with Nix without installing:

```bash
nix run github:aeroxy/ast-bro
```

Or add it as a dependency in your Nix flake:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    ast-bro.url = "github:aeroxy/ast-bro";
  };

  outputs = { self, nixpkgs, ast-bro }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      devShells.${system}.default = pkgs.mkShell {
        buildInputs = [ ast-bro.packages.${system}.default ];
      };
    };
}
```

---

## Quick start

```bash
# Map the structure of one file
ast-bro map path/to/Player.rs
ast-bro map path/to/user_service.py

# Map a whole directory (recurses supported extensions in parallel)
ast-bro map src/

# Print the exact source of one specific method
ast-bro show Player.cs TakeDamage

# Don't know which file it's in? Point at the directory (or a quoted glob)
ast-bro show src/ TakeDamage
ast-bro show 'src/**/*.cs' TakeDamage

# Compact public-API map of a whole module
ast-bro digest src/Services

# True public surface (resolves `pub use` / `__all__`, not every `pub` item)
ast-bro surface .                  # auto-detect Cargo.toml / pyproject.toml / __init__.py / build.zig
ast-bro surface --tree --include-chain mycrate/

# Every class that inherits/implements a given type
ast-bro implements IDamageable src/

# Dependency graph: forward, reverse, cycles, full
ast-bro deps src/auth.rs --depth 2  # what auth.rs imports (transitively)
ast-bro reverse-deps src/auth.rs    # who imports auth.rs (refactor blast radius)
ast-bro cycles                      # find import cycles via Tarjan SCC
ast-bro graph .                     # full dependency graph (text)
ast-bro graph . --json              # same, as JSON (ast-bro.graph.v1)

# Call graph: who calls X, what does X call (13 source-code languages)
ast-bro callers TakeDamage              # function/method: in-edges
ast-bro callers --tests TakeDamage      # same, only test files
ast-bro callers --hide-ambiguous TakeDamage  # drop ambiguous call edges
ast-bro callers Player                  # type: implementors + constructions
ast-bro callees Player.TakeDamage       # function/method: out-edges
ast-bro callees --hide-external Player.TakeDamage  # drop unresolved + external callees
ast-bro callees Player --depth 2        # type: ancestor walk (transitive)
ast-bro callers src/Player.cs:TakeDamage
ast-bro trace handle_request render     # shortest static call path, each hop inlined

# Blast radius of touching a symbol (one command: callers + callees + file deps + tests)
ast-bro impact TakeDamage               # default --mode all (every section)
ast-bro impact Player --depth 3         # deeper transitive window
ast-bro impact TakeDamage --mode tests  # only the affected tests section
ast-bro impact TakeDamage --tests       # apply tests filter to every section
ast-bro impact TakeDamage --exclude-tests  # drop test files from every section

# Token-budgeted context pack for a symbol (target + callees + callers + transitive)
ast-bro context TakeDamage              # default budget 8000 tokens
ast-bro context TakeDamage --budget 2000  # tight pack for smaller LLM windows
ast-bro context Player --json           # schema: ast-bro.context.v1

# Hybrid BM25 + dense semantic search (builds an index on first call)
ast-bro search "how does login work"
ast-bro search "HandlerStack" -k 5

# Find code semantically similar to a given file:line
ast-bro find-related src/auth/login.rs:42

# Build / refresh / inspect the per-repo search index
ast-bro index            # build or refresh
ast-bro index --stats    # show chunk count, model, etc.
ast-bro index --rebuild  # drop cache and rebuild

# Squeeze a repetitive log/text file into a smaller, reversible form (logs, not code)
ast-bro squeeze app.log                 # compress; falls back to raw if it wouldn't help
ast-bro squeeze app.log 1000:2000       # only a line range
ast-bro squeeze app.log --raw           # skip compression (raw + header, for diffing)
ast-bro squeeze app.log --json          # ast-bro.squeeze.v1 (legend + body + sizes)

# Output a prompt snippet to steer LLM agents
ast-bro prompt >> AGENTS.md

# Machine-readable JSON (stable schema, great for tooling)
ast-bro map src/player.rs --json
ast-bro digest src/ --json
ast-bro show Player.cs TakeDamage --json
ast-bro implements IDamageable src/ --json
ast-bro search "json rendering" --json
```

---

## Using with LLM coding agents

This is the main use case. The fastest path is `ast-bro install`,
which writes the agent prompt snippet (and, where supported, a real
`Read`-interceptor hook) into your coding agent's config.

```bash
# Install into every supported CLI it can detect on your system.
ast-bro install --all

# Or pick a single target.
ast-bro install --target claude-code
ast-bro install --target gemini --min-lines 150

# OpenCode prompt, MCP server, and native skill.
ast-bro install --target opencode
ast-bro install --target opencode --mcp --skills

# See exactly what would change before writing.
ast-bro install --all --dry-run

# Per-repo install (default is global).
ast-bro install --target claude-code --local

# Remove everything we wrote.
ast-bro uninstall --all

# Quick visibility.
ast-bro status
```

Supported targets: `claude-code`, `gemini`, `tabnine`, `cursor`,
`aider`, `codex`, `copilot`, `opencode`. Claude Code, Gemini, and Tabnine also get
a tool-call hook that intercepts `Read` on supported source files above
`--min-lines` (default 200) and substitutes the map output. On Claude
Code it also covers a read the host refuses outright, at any line count.
The other targets do not install a read-interceptor hook.

The substitution reaches the agent as a blocked tool call whose first
line says that nothing failed. For a read that has not run yet, that is
the only channel available: the map has to arrive instead of the file, so
the read must be refused, and a refusal is what the host reports.

Claude Code has a second event that would report a success instead,
`PostToolUse` with `hookSpecificOutput.updatedToolOutput`, and
`ast-bro install` does not register it. Measured on Claude Code 2.1.223:
that field replaces the result of an MCP tool and is ignored for the
built-in `Read`, so registering it would deliver no map and send the
whole file to the model. Registering a `PostToolUse` entry by hand does
nothing useful for `Read` on that version, for the same reason. See
[issue #34](https://github.com/aeroxy/ast-bro/issues/34).

On Claude Code the hook also registers under `PostToolUseFailure` for reads
that exceed the host's file-size limit, which is 256 KB in Claude Code
2.1.223. That byte limit catches files no `--min-lines` threshold does; a
90-line, 355 KB file trips it. Without this hook, the agent gets an error and
no map. `PostToolUseFailure` cannot replace a result, but it can add the map as
context beside the host's error.

Whatever the channel, the map is capped at 64 KB. To fit that cap, the hook
compares two options. It can remove doc comments and attributes, then fields
and private items, and finally cap members per type. It can instead preserve
detail and drop whole declarations. The hook chooses the option that returns
more declarations. This avoids reducing a mostly private file to only its few
public declarations when a richer partial map fits.

The trim orders declarations by the same detail ladder, so it includes the
public surface first unless that surface alone exceeds 64 KB. Filling in file
order once returned 1,733 private items and none of the file's 500 public
items because the public items were at the end. A trim removes each
declaration with its doc comment and members, which prevents orphaned details.
The payload ends with a note that identifies missing content and gives the
`ast-bro map` command that returns it.

### Claude Code subagent shadowing

Claude Code has isolated subagents (Explore, Plan, general-purpose) that run in
their own context and cannot see the main `CLAUDE.md`. `ast-bro install` 
automatically shadows these subagents with `.claude/agents/<Name>.md` files 
containing the full ast-bro prompt.

When you run `ast-bro install --target claude-code`, you get:

- `CLAUDE.md`: main agent prompt (global or local per-repo)
- `.claude/settings.json`: `Read` hooks on `PreToolUse` and `PostToolUseFailure`
- `.claude/agents/Explore.md`: Explore subagent with the prompt injected

This solves the "why doesn't my subagent use ast-bro?" problem: subagents
now get the prompt automatically. Legacy manual `~/.claude/agents/Explore.md` files
are wrapped in marker blocks in-place (non-breaking).

### Skills for manual installation

A `skills/` folder is included in the repo for users who prefer manual setup:

```bash
# Clone or download the repo
git clone https://github.com/aeroxy/ast-bro.git
cd ast-bro

# Copy the skill to your user skills directory
cp -r skills/ast-bro ~/.claude/skills/ast-bro

# Then manually invoke from Claude Code
/ast-bro
```

This works alongside `ast-bro install`: the skill definition tells Claude Code
how to invoke the [ast-bro](https://github.com/aeroxy/ast-bro) CLI with proper tool schemas and documentation.

Manual install via `ast-bro prompt` (e.g. project-level):

```bash
ast-bro prompt >> AGENTS.md
ast-bro prompt | pbcopy   # macOS clipboard
```

### Works with

- Claude Code (+ custom subagents like `Explore`, `codebase-scout`)
- Cursor agent mode
- OpenCode
- Aider
- Copilot Chat / Workspace
- Any custom agent on the Claude / OpenAI / Gemini APIs
- Humans (the colored terminal format is highly readable; `show` is a nice alternative to `grep -A 20`)

---

## Output format

The format is designed to be **LLM-friendly**: Python-style indentation,
line-number suffixes in `L<start>-<end>` form, doc-comments preserved.
The header summarises scale and flags partial parses.

When you run it yourself, you'll see a gorgeous ANSI-colored output. Don't worry, the terminal colors are automatically stripped when piped to a file or consumed by an agent's shell hook!

### Rust

```
# src/core.rs (490 lines, 3 types, 12 methods, 5 fields)
pub struct Declaration  L10-120
    pub kind: DeclarationKind  L12
    pub name: String  L15
    pub fn lines_suffix(&self) -> String  L30-48
```

### Multi-file `show`

A `show` target is a file, a directory, or a quoted glob, so you can extract a
symbol you can name from a tree you haven't mapped yet:

```bash
ast-bro show src/ TakeDamage            # searches every parseable file
ast-bro show 'src/**/*.cs' TakeDamage   # quote it: see below
ast-bro show a.java b.java greet        # an explicit list works too
```

A multi-file answer leads with its coverage, and caps rendered bodies at
`--limit 20` while still reporting the true total:

```
# 3 match(es) for 'greet' in 2 of 47 file(s) searched
```

**Unquoted globs are recovered, not misread.** The shell expands
`show src/*.cs Widget` before `ast-bro` starts, so the extra file names arrive
in the argument list; they are taken as targets rather than as symbol names,
and a note reminds you to quote the glob. Previously that spelling searched
only the first file and dropped the symbol's other definitions silently.

### `show` with ancestor context

`ast-bro show <target> <Symbol>` prints a `# in: ...` breadcrumb
between the header and the body so you know what the extracted code is
nested inside, without a second `map` call:

```
# Player.cs:30-48  Game.Player.PlayerController.TakeDamage  (method)
# in: namespace Game.Player -> public class PlayerController : MonoBehaviour, IDamageable
/// <summary>Apply damage.</summary>
public void TakeDamage(int amount) { ... }
```

---

## JSON output

Add `--json` to any command to get the full symbol graph as stable,
structured JSON instead of formatted text for editors, language
servers, CI tooling, or any script that needs to consume the data
programmatically.

```bash
ast-bro map src/player.rs --json        # per-file map
ast-bro digest src/ --json              # digest view
ast-bro show Player.cs TakeDamage --json
ast-bro implements IDamageable src/ --json
ast-bro map src/ --json --compact       # single-line (no pretty-print)
```

Every JSON document includes a `schema` field that is bumped on breaking
changes, so downstream tooling can guard on it:

```json
{
  "schema": "ast-bro.map.v1",
  "files": [
    {
      "path": "src/player.rs",
      "language": "rust",
      "line_count": 312,
      "error_count": 0,
      "declarations": [
        {
          "kind": "struct",
          "name": "Player",
          "signature": "pub struct Player",
          "visibility": "pub",
          "start_line": 10,
          "end_line": 40,
          "children": [ ... ]
        }
      ]
    }
  ]
}
```

| Schema | Command |
|--------|----------|
| `ast-bro.map.v1` | `map --json`, `digest --json` |
| `ast-bro.show.v2` | `show --json` |
| `ast-bro.implements.v1` | `implements --json` |
| `ast-bro.surface.v1` | `surface --json` |
| `ast-bro.deps.v1` | `deps --json` |
| `ast-bro.reverse-deps.v1` | `reverse-deps --json` |
| `ast-bro.cycles.v1` | `cycles --json` |
| `ast-bro.graph.v1` | `graph --json` |
| `ast-bro.callers.v1` | `callers --json` |
| `ast-bro.callees.v1` | `callees --json` |
| `ast-bro.trace.v1` | `trace --json` |
| `ast-bro.impact.v1` | `impact --json` |
| `ast-bro.context.v1` | `context --json` |
| `ast-bro.search.v1` | `search --json` |
| `ast-bro.related.v1` | `find-related --json` |
| `ast-bro.index-stats.v1` | `index --stats --json` |
| `ast-bro.run.v1` | `run --json` |
| `ast-bro.squeeze.v1` | `squeeze --json` |
| `ast-bro.error.v1` | any rejected call under `--json` (on stderr) |

`show` is the one schema past v1. **v1 -> v2:** the top-level `path` /
`language` / `matches` keys moved into a `files` array (one entry per file,
same three keys), because a target can now be several files rather than only
one. Version 2 also adds the text coverage counters `files_scanned`,
`files_matched`, `total`, `shown`, and `truncated`. The `unmatched` array lists
requested symbols that matched nothing; the CLI reports those names as a
stderr note instead of a text-header counter. A v1 consumer reading one explicit file migrates by taking
`files[0]`.

---

## CLI contract

One rule set for every subcommand, so a consumer never needs a per-command table:

- **Channel**: stdout carries results only; every note, hint, and error goes to stderr. `--json` output always parses without preprocessing.
- **Exit codes**: `0` means the query ran, even if the answer is empty. Qualifications such as unresolved paths, display caps, and depth cutoffs appear as `# note:` messages on stderr. `2` means the query could not run as asked. `1` means an internal failure. Two commands add result-specific codes: `cycles` exits `3` when cycles exist, while `run` exits `1` when a valid pattern matched nothing. A rejected `run` still exits `2` with empty stdout.
- **Machine-readable rejections**: with `--json`, a rejected call also emits an `ast-bro.error.v1` object on stderr: `{schema, command, kind, detail, hint}`. `kind` is one of `no_input | path_not_found | symbol_not_found | unknown_flag | bad_argument | index_error`.
- **Unknown flags** exit 2 with the error on stderr; when the flag exists on a sibling subcommand, the message names it (`--glob is a map flag`).
- **Truncation is never silent**: when `--limit` / `--max-members` cut a list, the header reports the true total and the flag that lifts the cap, and JSON carries `total` / `truncated`. `--limit` bounds the *display*, not the work: `callers` / `impact` / `reverse-deps` walk the full reverse cone so the reported total is exact, which at `--depth 5` on a large repository is real work regardless of the cap.
- **Depth cutoffs**: a walk that ran out of `--depth` and one that ran out of graph both just stop, so every depth-bounded command reports which happened: `frontier_truncated` in JSON (`callers`, `callees`, `deps`, `reverse-deps`, `trace`, and each `impact` report), plus a stderr note in text mode. It is orthogonal to `truncated`: `truncated: false` with `frontier_truncated: true` means nothing was cut from the display and `total` itself counts only the cone inside `--depth`. On `trace`, it separates "no path" from "no path within `--depth`".

`map` and `digest` are one command. `digest` is an alias for `map --preset digest` (= `--detail names --no-private --no-fields --max-members 50`), and both accept the full flag set. Detail (`--detail names|signatures|full`), visibility (`--no-private`, `--no-fields`, `--no-docs`, `--include-private`, `--include-fields`, ...), and scope (`--glob`, `--max-members`) are independent controls. Explicit flags override the preset. At `names` and `signatures` detail, JSON omits doc comments, including under `digest`. A projected payload carries `{docs, line_numbers, attributes}` in a `projected` object so consumers can distinguish intentional omission. An unprojected payload has no such key and remains byte-identical to the previous form.

---

## MCP server

Run [ast-bro](https://github.com/aeroxy/ast-bro) as a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio so any MCP-aware coding agent can call the same operations
as native tools without shell parsing:

```bash
ast-bro mcp
```

The server speaks line-delimited JSON-RPC 2.0 on stdin/stdout and exposes nineteen
tools that map 1:1 to the CLI commands:

| Tool | Equivalent CLI | Returns |
|------|----------------|---------|
| `map`          | `ast-bro map <paths>`                | text, or `ast-bro.map.v1` with `json: true` |
| `digest`       | `ast-bro digest <paths>`             | text, or `ast-bro.map.v1` with `json: true` |
| `show`         | `ast-bro show <target> <syms>`       | text, or `ast-bro.show.v2` with `json: true`; target may be a file, directory, or glob |
| `implements`   | `ast-bro implements <type> <paths>`  | text, or `ast-bro.implements.v1` with `json: true` |
| `callers`      | `ast-bro callers <symbol>`           | text, or `ast-bro.callers.v1` with `json: true` |
| `callees`      | `ast-bro callees <symbol>`           | text, or `ast-bro.callees.v1` with `json: true` |
| `trace`        | `ast-bro trace <from> <to>`          | text, or `ast-bro.trace.v1` with `json: true` |
| `surface`      | `ast-bro surface [path]`             | text, or `ast-bro.surface.v1` with `json: true` |
| `impact`       | `ast-bro impact <symbol>`            | text, or `ast-bro.impact.v1` with `json: true` |
| `context`      | `ast-bro context <symbol>`           | text, or `ast-bro.context.v1` with `json: true` |
| `deps`         | `ast-bro deps <file>`                | text, or `ast-bro.deps.v1` with `json: true` |
| `reverse_deps` | `ast-bro reverse-deps <file>`        | text, or `ast-bro.reverse-deps.v1` with `json: true` |
| `cycles`       | `ast-bro cycles [path]`              | text, or `ast-bro.cycles.v1` with `json: true` |
| `graph`        | `ast-bro graph [path]`               | text by default; `json: true` for `ast-bro.graph.v1` |
| `search`       | `ast-bro search "<query>"`           | text, or `ast-bro.search.v1` with `json: true` |
| `find_related` | `ast-bro find-related <file>:<line>` | text, or `ast-bro.related.v1` with `json: true` |
| `index`        | `ast-bro index`                      | text, or `ast-bro.index-stats.v1` with `json: true` |
| `run`          | `ast-bro run -p <pattern>`           | text diff, or `ast-bro.run.v1` with `json: true` |
| `squeeze`      | `ast-bro squeeze <file>`             | text, or `ast-bro.squeeze.v1` with `json: true` |

Wire it into a client by pointing at the binary:

```jsonc
{
  "mcpServers": {
    "ast-bro": { "command": "ast-bro", "args": ["mcp"] }
  }
}
```

The server is fully synchronous, has no extra runtime dependencies, and adds
roughly 1% to the binary size. The CLI itself is unaffected: none of the MCP
code runs unless you invoke the `mcp` subcommand.

---

## Semantic search

`ast-bro search` runs hybrid retrieval over a per-repo index:

- **BM25** for exact identifier matches and keyword density.
- **Dense embeddings** use [`minishlab/potion-code-16M`](https://huggingface.co/minishlab/potion-code-16M), a static `vocab x 256` model with no inference step.
- **Reciprocal Rank Fusion** (k = 60) blends the two. Alpha resolves to 0.3 for symbol queries such as `HandlerStack` and `Sinatra::Base`, or 0.5 for natural-language queries.
- A ranking pass adds definition boosts (3× for chunks that *define* a queried symbol), file-coherence boosts (multi-chunk hits in the same file lift the top chunk), file-stem matches for NL queries, and path-based penalties (test files 0.3×, `.d.ts` stubs 0.7×, `__init__.py` 0.5×).

`ast-bro find-related <file>:<line>` uses the same engine in semantic-only mode, filters by language, and excludes the source chunk. Use it to find code with a similar structure.

```bash
ast-bro search "request validation" -k 5
ast-bro search "HandlerStack" --json
ast-bro find-related src/auth/login.rs:42 -k 3
```

### How indexing works

First call to `search` / `find-related` builds an index at `.ast-bro/index/`:

```text
.ast-bro/
  .gitignore           # auto-written, contents: "*"
  index/
    meta.json          # schema + model + chunk_count
    chunks.bin         # per-chunk content + line range + language
    embeddings.f32     # chunk_count × 256 little-endian f32, mmap-friendly
    bm25.bin           # vocab + idf + postings
    files.bin          # per-file mtime + xxhash + chunk range
    lock               # advisory lock for concurrent writers
```

Subsequent calls walk the tree, compare `(mtime, size)` against `files.bin`, and hash only files where the cheap check fails. A file delta tombstones replaced chunks and appends updated chunks. The index rebuilds fully only when the delta update fails or tombstones cross the compaction threshold. Steady-state cost on an unchanged 10k-file repo is about 30 ms of stat syscalls.

The model is downloaded once (~64 MB) on first use to `~/.cache/ast-bro/models/`. It tries HuggingFace first, falls back to `hf-mirror.com` if blocked. **TLS verification is disabled by default** so corporate MITM proxies don't break setup; integrity is enforced via SHA-256 on every cached file. Set `AST_OUTLINE_TLS_STRICT=1` to enforce strict TLS.

For more on what gets indexed (the five filter layers, `.ast-bro-ignore` syntax) see the "What gets walked" section above. For the security trade-offs around the TLS default, see the [network-security wiki page](https://github.com/aeroxy/ast-bro/blob/main/wiki/network-security.md) on GitHub.

`find-related` loads or builds the dep graph on its first boosted query. It boosts results within dependency depth 2 of the source, in either direction. Disable this with `--no-dep-boost`.

---

## Dependency graph

`ast-bro deps`, `reverse-deps`, `cycles`, and `graph` build a file-level import graph for the project and answer different questions on it:

```bash
ast-bro deps src/auth.rs --depth 2          # what does auth.rs pull in?
ast-bro reverse-deps src/auth.rs            # who imports auth.rs? (refactor blast radius)
ast-bro cycles                              # find import cycles via Tarjan SCC (exit 3 if any)
ast-bro graph .                              # full dependency graph (text)
ast-bro graph . --json                      # same, as JSON (ast-bro.graph.v1)
```

All four commands share `.ast-bro/deps/graph.bin`, which stores `UnifiedGraph { deps, calls: Option<CallGraph> }` for both dependency and call queries. The first call builds the dependency half; later calls use per-file delta detection, and `--rebuild` forces a fresh build. Inside `ast-bro mcp`, a registry entry for each canonical repository root is revalidated on every call. An unchanged tree reuses its in-memory `Arc<UnifiedGraph>` without another disk read; an edit patches the graph and swaps in a new `Arc`.

Resolution is per-language but shares one suffix-index resolver:

- **Rust**: `use crate::*` / `use super::*` / `mod foo;` (with `#[path]` attribute support).
- **Python**: relative imports (`from .x import y`), `__init__.py` packages, bare `import a.b`.
- **TypeScript / JavaScript**: relative paths with extension probing (`.ts -> .tsx -> .mts -> .cts -> .d.ts -> .js -> ... -> .json`), `index.*` fallback, `tsconfig.json` `paths` aliases.
- **Java / Kotlin / Scala / C#**: FQN suffix index built from each file's `package` / `namespace` declaration. Inner classes resolve via strip-and-retry.
- **Go**: strips the `go.mod` module prefix and resolves packages by directory. Every `go.mod` in the repository counts, including modules in subdirectories and side-by-side modules.
- **C++**: resolves quoted `#include` paths relative to the importer and leaves system headers external.
- **PHP**: resolves namespace imports through Composer PSR-4 mappings, suffix lookup, and a class-name fallback. Literal `include` / `require` paths resolve relative to the importer.
- **Ruby**: resolves literal `require_relative` calls and leaves `$LOAD_PATH` or gem imports external.
- **Zig**: resolves literal `.zig` and `.zon` imports relative to the importer, plus unambiguous literal module wiring in `build.zig`. Generated modules and external libraries such as `std` remain unresolved.

The four commands are also exposed as MCP tools for agents. For internals (suffix index, Tarjan SCC, per-file invalidation, find-related dep boost) see the [deps wiki page](https://github.com/aeroxy/ast-bro/blob/main/wiki/deps.md) on GitHub.

---

## Call graph

`ast-bro callers` and `ast-bro callees` answer "who calls X" and "what does X call" across 13 source-code languages. SQL and Markdown do not enter the call graph because their adapters intentionally emit no call sites. The commands avoid `grep` matches from comments and string literals.

```bash
ast-bro callers TakeDamage              # function/method: in-edges
ast-bro callees TakeDamage              # function/method: out-edges
ast-bro callers Player                  # type: implementors + constructions
ast-bro callees Player --depth 2        # type: ancestor walk (transitive)
ast-bro callers Player.TakeDamage --include-ambiguous --json
```

Both commands are **kind-aware**:

| Target kind | `callers X` | `callees X` |
|---|---|---|
| function / method / constructor | call sites that invoke `X` | call sites inside `X`'s body |
| class / struct / trait / interface / enum / record | implementors + constructions (covers `Foo()`, `new Foo()`, `Foo {}`, `Foo::new()`) | ancestor types and the methods they declare (transitive via `--depth N`) |

Symbol forms accepted by both: bare suffix (`TakeDamage`), dotted (`Player.TakeDamage`), file-scoped (`src/Player.cs:TakeDamage`), or flag form (`--file src/Player.cs --symbol TakeDamage`).

**Three-pass resolver.** Bare names are disambiguated in three increasing-cost passes:

1. **Same-file**: local definitions + per-file `import` / `use` / `using` bindings.
2. **Global symbol table**: single-match promotion across the project. Receiver-bearing calls (`obj.bar()`) skip this pass to avoid `builder.hidden()`-style false positives on global homonyms.
3. **Dep-graph disambiguation**: for ambiguous matches, filter candidates by the caller's transitive forward-dep closure.

Every edge carries a `Confidence` tag: `Exact` (passes A/B), `Inferred` (pass C narrowed to one), or `Ambiguous` (multiple candidates survive). Ambiguous callers and unresolved/external callees are shown by default (tagged); `--hide-ambiguous` (callers) and `--hide-external` (callees) drop them when you want the cleaner bucket.

**Cache.** The call graph uses the same `.ast-bro/deps/graph.bin` as the dep graph and builds lazily, so `deps` / `cycles` users do not pay the call-extraction cost. Per-file invalidation normally re-extracts only edited files. After any indexed source change, it also refreshes every unchanged Zig caller file so namespace aliases and receiver types match a cold build.

For internals (per-language node-kind tables, the call-shape pitfalls each adapter handles, the per-file patch path, cost numbers) see the [calls wiki page](https://github.com/aeroxy/ast-bro/blob/main/wiki/calls.md) on GitHub.

---

## Architecture and development

See the [wiki](https://github.com/aeroxy/ast-bro/blob/main/wiki/architecture.md) on GitHub for details on how [ast-bro](https://github.com/aeroxy/ast-bro) leverages `ast-grep` internally and how you can add new language adapters.

### Getting started

```bash
git clone https://github.com/aeroxy/ast-bro.git
cd ast-bro

# With Cargo
cargo run -- digest src/

# With Nix flake
nix develop        # Enter development shell
nix build          # Build the project
nix flake check    # Run all checks (tests, clippy, formatting)
```

Contributions welcome.

---

## License

[MIT](./LICENSE)
