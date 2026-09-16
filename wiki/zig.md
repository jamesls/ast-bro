# Zig support

ast-bro parses Zig 0.16 source with its [bundled grammar](../vendor/tree-sitter-zig/README.md).
The declaration adapter, dependency extractor, search chunker, and structural
`run` commands use the same grammar. Legacy `usingnamespace` syntax remains
readable.

## Declarations and imports

Shapes include structs, enums, tagged and untagged unions, opaque types, error
sets, functions, fields, tests, comptime blocks, and anonymous containers inside
functions and initializers. Signatures preserve escaped identifier spellings;
lookup compares their decoded values.

Dependencies come from literal `@import` syntax nodes. Comments and strings
cannot create edges, and imports can span lines. Both `.zig` and `.zon` files
resolve relative to their importer. ZON files participate in dependency
fingerprints and text search; shape commands still operate on Zig source.

Literal `addModule` and `addImport` wiring in the nearest `build.zig` can resolve
named modules when every observed binding identifies the same source file.
The supported construction forms are `createModule` and `addModule` with
`.root_source_file = b.path("file.zig")`, including local module aliases. Unknown or
conflicting bindings remain unresolved. A single literal compilation root also
provides a surface entry point when the conventional roots are absent.

Directory walks include source under `src/build`. Root build output, `zig-out`,
`.zig-cache`, and `zig-cache` remain excluded.

## Calls and public surfaces

Call extraction follows lexical scopes by AST ancestry and byte positions.
Imports and aliases remain visible in nested types, functions, and comptime
blocks. A shadowing binding blocks an outer name even when its value is unknown.

Static resolution handles these forms:

- File and nested type aliases for `@This()`.
- Annotated receivers and values constructed with a known local return type.
- Declaration literals such as `.init()` in annotations, returns, fields, and
  arguments whose expected type is visible in the same file.
- Parenthesized aliases, callable aliases, literal `@call` targets, and literal
  `@field` member names.
- Generic type factories that directly return one anonymous container.
  Specializations point to that container's source methods; ast-bro does not
  create separate method bodies for each type argument.

Public facades expand parentheses and generic specializations. Unknown `if`
and `switch` conditions expose candidate definitions marked `conditional` in
surface JSON and `[conditional]` in text. Call edges with multiple known targets
carry `Ambiguous` confidence and a candidate list.

Tests use `test@L<line>C<column>` identities. Their original descriptions remain
in signatures. This separates a doctest from its subject and preserves test
status for nested helpers. `callers --tests`, `--exclude-tests`, and impact
analysis use that status alongside the shared file-name heuristics.

## Search and caches

Search records members of generic and local containers. `run --lang zig` and
extension detection support structural search and rewrite, including `$NAME`
and `$$$ARGS` metavariables. Rewrites are previews unless `--write` is supplied.

Graph and search cache schemas are version 4. Older caches rebuild on their next
query. Source changes refresh Zig dependency and call resolution so changes to
facades and literal build wiring also update unchanged importers.

## Static analysis limits

ast-bro does not execute Zig code or build scripts. Generated options, dependency
packages outside the indexed project, arbitrary comptime factories, and module
roots computed by helper programs can remain unresolved. Generic factories with
multiple possible returned containers require more analysis than the supported
single-container form.

`@cImport` declarations require C preprocessing and compiler configuration.
Runtime callback values, reflection with computed member names, and mutable
dispatch tables also lack a concrete static target. Calls through such values
remain visible as unresolved edges. These limits apply even when the source
parses without errors.
