# Bundled Zig grammar

Based on [tree-sitter-grammars/tree-sitter-zig](https://github.com/tree-sitter-grammars/tree-sitter-zig/tree/6479aa13f32f701c383083d8b28360ebd682fb7d)
at commit `6479aa13f32f701c383083d8b28360ebd682fb7d`, under the included MIT license.

Local changes accept Zig 0.16 identifiers `usingnamespace`, `async`, and
`await`, assembly clobber initializers, and error-set types in container
fields. Legacy keyword syntax remains parseable. Field initializers prefer
their dedicated rule when the grammar can distinguish them from assignments.

The root `build.rs` compiles the checked-in parser. Builds require a C compiler;
they do not require Node.js or the tree-sitter CLI.

To regenerate after editing `grammar.js`, run tree-sitter CLI 0.26.8 in this
directory:

```sh
tree-sitter generate
```

Commit `grammar.js`, `src/parser.c`, `src/grammar.json`, `src/node-types.json`,
and the generated headers together. Run the Zig adapter, graph, surface, and
structural-search tests after regeneration.
