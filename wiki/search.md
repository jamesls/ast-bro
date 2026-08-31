# Semantic search

The `search` and `find-related` subcommands share a persistent index for each
repository. This page documents the internal architecture. For the user-facing
surface, see the README. For network and TLS behavior, see
[network-security.md](network-security.md). For indexed files, see
[file-filtering.md](file-filtering.md).

Every number quoted here comes from [tests/eval](../tests/eval/README.md), a
30-query set over pgjdbc at a pinned revision, with symbol-level ground truth.
Measure a change there before adopting it. Several changes that looked right
did not change the scores, one that looked wrong improved them, and the harness
caught the ranking non-determinism described under [Determinism](#determinism).

Scores are quoted at `--min-iou 0.3`: a result counts only when its line range
resembles the answer's. Bare overlap instead rewards coarse chunking, since a
result spanning a whole class overlaps every symbol in it by construction. The
pre-hierarchy chunker leads on that metric and collapses to 3% on this one.

## Pipeline

```
search "<query>":
  parse_query(query)                   peel lang:/path:/name: filters; rest = text
    -> mask = language AND path-substring AND name-substring AND query_scope AND live
  tokenize(text)
    |-- BM25.get_scores(tokens, mask) -> 100 candidates (fixed, see below)
    `-- encode_one(text) -> cosine_topk(embeddings, mask) -> same count
  RRF normalize each (k = 60)
  combine(alpha-weighted)              alpha auto: 0.3 symbol query, 0.5 NL
  boost_multi_chunk_files(...)         file coherence (+20% * file_sum/max_file_sum)
  file_prior(...)                      phase one: rank files by their Outline
                                       chunks alone; chunks in a liked file are
                                       multiplied by 1 + 1.0 * prior
  apply_query_boost(...)               definition (3x), embedded symbol (1.5x),
                                       file-stem matches for NL queries
  rerank_topk(top_k, penalise_paths=True)
                                       chunk kind: Enclosing 0.7x, Outline 0.8x;
                                       test files 0.3x, compat dirs 0.3x,
                                       generated files 0.3x, __init__.py 0.5x,
                                       .d.ts 0.7x, file-saturation decay (0.5^extra);
                                       an Enclosing chunk drops when one of its
                                       own Parts is also a candidate

find-related <file>:<line>:
  resolve_chunk(file, line)             prefer chunks where start <= line < end
  encode chunk.content
  cosine_topk(embeddings, mask)         mask: same language, exclude self
  return top_k
```

The candidate count is fixed at 100, not derived from `top_k`. Deriving it made
`top_k` change the *order* of results and not only how many come back, since
the reranker sees a different candidate set. A *floor* of 100 over `top_k * 5`
was not enough: it only moved the coupling past `k = 20`, where the multiplier
overtakes the floor again. Eight of thirty eval queries still had a
`k`-dependent top-3 and three a `k`-dependent top-1. With the pool fixed it is
zero of thirty. It moves neither recall@5 nor MRR@10, and costs one wider
selection over scores both retrievers compute for every chunk anyway.

## Chunk hierarchy

Chunking emits overlapping levels, not a partition. `ChunkKind` says which:

| Kind | What it covers |
| --- | --- |
| `Source` | A slice of the file packed at member boundaries |
| `Enclosing` | A member too large for one chunk, kept whole |
| `Part` | A body-level slice of an `Enclosing` member |
| `Outline` | Synthesized signature list, no body text |

`Source` and `Enclosing` tile the file exactly once. `Part` chunks re-cover the
inside of a large member on purpose, so a query can match the statement or the
method containing it; ranking prefers the `Part` and drops the `Enclosing` when
both are candidates.

Splitting descends through *type containers*, including class bodies, `impl`
blocks, and inline modules, but never into a callable. That keeps a chunk
boundary off the middle of an expression. Descending into anything oversized
instead put `list.forEach` and `(item -> {` in different chunks, and separated
a nested class header from its body.

Zig uses the same hierarchy through its raw tree-sitter grammar because
ast-grep does not expose Zig as a `SupportLang`. Variable-bound containers such
as `const Client = struct { ... }` contribute their type name to nested method
breadcrumbs. Typed anonymous structs, union payloads, enum tags, and error-set
members also appear in their enclosing outline. Functions, tests, and
`comptime` blocks can produce body parts, and file outlines include Zig
declarations instead of treating the file as blank-line-delimited text.

"Callable" has to include constructors and destructors, which are named
`*_declaration` rather than anything containing `method`. Leaving them out made
them containers: descent entered the body, and `local_variable_declaration`
carries the `_declaration` suffix too, so it became a split point in the middle
of a constructor. `is_member_kind` rejects `local_*` for the same reason. These
nodes are statements whose kinds have a declaration suffix.

The effect is largest where a language forces one top-level declaration per
file. Splitting only at top level left a whole Java class as a single chunk: on
pgjdbc, 85% of indexed characters sat in chunks above the size target, and
`PgResultSet.java` was one 139 KB chunk of 4397 lines. Per-member splitting
brings that to 41%, and p90 chunk height from 196 lines to 57. Rust gains less
(53% -> 39%), since it already has many top-level items and only `impl` blocks
are affected. Go and C barely change.

### Breadcrumbs

Every chunk carries the declaration path that encloses it (`QueryExecutorBase >
sendQueryCancel`), rendered in results and present in `--json`. It also goes
into the text handed to the embedder. `potion-code-16M` is static and never saw
such headers in training, but this choice measures well. Dropping the breadcrumb
costs recall@5 23% -> 17% and MRR@10 0.226 -> 0.148.
Averaging is likely why it works, since a body-level chunk that never names its
own method gets those tokens pulled into its mean. BM25 keeps the breadcrumb
too, where it carries the effect alone but adds nothing on top of the embedder.

### Outline documents

One synthesized chunk per file, listing member signatures with no bodies. A
signature ends where the AST says the body starts, not at the first `{` or
newline. A brace can sit inside a parameter list, as in `fn unpack(Config {
host, port }: Config)`, or inside a destructured TypeScript prop. Stopping at
the newline dropped the parameters of every signature wrapped across lines.
Declarations with no body at all, such as interface methods, keep their whole
text.

Outlines answer "which file exposes this API" for queries that name a type or
method, where body-level chunks each match only weakly. They sit beside the
source chunks rather than replacing them.

A wrapper node is transparent, detected by shape rather than by kind name: a
node holding a member that ends where it ends. Left opaque, `export_statement`
hid every exported declaration, so a TypeScript outline listed exactly the ones
that were *not* part of the public API, and Python's `decorated_definition`
listed `@decorator` as a member of its own.

Markdown gets outlines built from its headings. A heading identifies a document
section as a signature identifies a class member. Heading detection implements
inexpensive CommonMark rules: at most three spaces of indentation (four makes
it a code block), setext underlines as well as `#`, nothing inside a fenced
block, and no YAML front matter. Without the front-matter exclusion, the closing
`---` would read as a setext underline and promote the last metadata line into
a heading. Markdown outlines affect ranking because the phase-one prior can
only *raise* a file's score. A file without an outline could not compete with a
file that had one, so documents lost to code on queries that the documents
answered.

A long outline is split at `OUTLINE_MAX_CHARS = 3000`, well under the
embedder's own cap. An outline above that cap is dropped from the dense index
entirely. The files with the longest outlines are the large central ones, so
leaving them unembedded made phase-one file retrieval miss
`QueryExecutorImpl`, `PgConnection` and `PgResultSet`. Splitting them raised
phase-one file recall@5 from 33% to 60%, and top-1 from 23% to 40%.

The pieces do not overlap. Carrying signatures across the boundary measured
worse at every setting tried: an outline is a list of independent signatures
rather than prose, so no evidence straddles the cut, and overlap only
duplicates terms and makes near-identical pieces compete.

## Two-phase retrieval

`file_prior` ranks *files* using only their `Outline` chunks, then phase two
multiplies each chunk by `1 + 1.0 * prior[file]`. The prior only ever promotes:
a hard filter would zero out recall for every query whose file the outlines
miss, and phase one finds the right file in roughly half of them.

Measured on the pinned pgjdbc set: MRR@10 0.204 -> 0.226 at unchanged recall@5,
with two queries improving and none regressing. Anything from 0.5 to 2.0
performs about the same, so the weight sits on a plateau. Latency is unchanged
because phase one is a masked pass over embeddings already in memory.

## Field-qualified queries

`parse_query` (in `query.rs`) peels structured filters out of the raw query
before retrieval, so an agent can narrow a search inline instead of via
separate flags. Given:

```text
lang:rust path:src/auth name:login how is the token refreshed
```

it splits into `lang=rust`, path-substring `src/auth`, name-substring
`login`, and the free text `how is the token refreshed`. The filters become a
post-filter mask (composed with the existing `--lang` flag and the
path-derived `query_scope`); the free text is what BM25 + the embedder
actually score, over the narrowed set.

- `lang:` / `language:` selects the chunk language and **unions** with the
  `--lang` flag.
- `path:` selects a case-insensitive substring of the chunk's repository-relative
  path.
- `name:` selects a case-insensitive substring of the file's name. The index is
  chunk-based (no per-symbol name), so `name:` matches the *file*, not a
  symbol.

Repeated fields OR together (`lang:rust lang:go` -> either). Quoted values
keep spaces (`path:"src/some path"`). An unknown prefix (`TODO:`,
`http://x`) falls through to the free text, so a literal colon still
searches. `build_combined_mask` (`index.rs`) applies the filters through the
same path as language, scope, and tombstone filtering.

## Module layout

```
src/search/
|-- tokens.rs      identifier extraction + camel/snake split
|-- bm25.rs        sparse BM25 (lucene variant), get_scores(tokens, mask)
|-- chunker.rs     chunking: ast-grep declarations, raw tree-sitter for Zig and
|                  Markdown, blank-line paragraphs for TOML and PowerShell
|-- download.rs    HF probe + hf-mirror fallback + sha256 manifest
|-- embed.rs       safetensors mmap + tokenizer.json + cosine_topk (SIMD via wide)
|-- fusion.rs      RRF (k=60) + alpha resolver
|-- ranking.rs     boosting + penalties + greedy top-k
|-- cache.rs       mtime + xxhash3 delta detection
|-- query.rs       field-qualified query parser (lang:/path:/name:)
|-- index.rs       orchestrator: build / open / search / find_related / persist
|-- format.rs      text + JSON renderers (shared by CLI and MCP)
|-- cli.rs         clap-side handlers, called from main.rs
`-- mcp.rs         MCP-side handlers, called from mcp/tools.rs
```

The `cli.rs` and `mcp.rs` shims keep dispatch in [main.rs](../src/main.rs) and
[mcp/tools.rs](../src/mcp/tools.rs) thin. Each subcommand forwards directly to
the shared `Index::search` or `Index::find_related` method.

## Embedding model

The model2vec `potion-code-16M` model is a static embedder. It uses a
`vocab * 256` float32 lookup table instead of neural-network inference:

```
encode_one(text):
  ids = tokenizer.encode(text, add_special_tokens=False)
  mean = average(embeddings[id] for id in ids)
  return L2_normalize(mean)
```

Tokenization takes most of the time (about 10-100 microseconds); embedding
lookup takes little time by comparison. The output is always L2-normalized, so
cosine similarity reduces to a dot product.

`Embedder::open(model_dir)` mmaps `model.safetensors` (about 64 MB). The matrix
stays paged in but is never copied. `vocab * 256 * 4 bytes` is about 64 MB
regardless of repository size.

`f16` tensors are also accepted and decoded once into an owned `f32` buffer at
open time, so nothing downstream of `Embedder` has to know the on-disk dtype.

### Tokenizer

The model uses WordPiece with `lowercase: true` and a vocabulary of about
62,000 tokens. The normalizer lowercases *before* WordPiece splits, so
`sendQueryCancel` becomes
`sendquerycancel`. The case boundaries, which are the only signal of where the
words are, disappear before splitting. `send`, `query` and `cancel` are
all in the vocabulary; the model just cannot reach them.

Feeding the word-split form alongside the identifier exposes those tokens. The
measured gain is small: one additional recall@5 query out of 30, MRR@10 within
run-to-run noise, and one regression where splitting diluted the rare term
`SASL` across many chunks. This split is not enabled. The lexical half already
splits identifiers via `split_identifier`, which accounts for most of the small
dense-side gain.

## Cosine top-k

`cosine_topk(query, embeddings, mask, k, tie_break)` walks every row of the
chunk-embedding matrix:

- Pre-load the query into 32 `wide::f32x8` SIMD lanes (256 dimensions = 32
  groups of 8).
- Read each row as one cache-friendly `&[f32; 256]` slice from the contiguous
  matrix.
- Compute each row's dot product with 32 8-lane FMA operations and a horizontal
  sum.
- Parallelize matrices with at least 4096 rows through rayon over row blocks of
  256.
- Select the top k indices with `select_nth_unstable_by`, then sort the prefix.
  Equal scores use the caller's persistent chunk-identity comparator rather
  than the embedding row id.

A 10,000-chunk repository takes about 25 ms on one thread and 5 ms across eight
cores.

The index does not use HNSW or another ANN structure. At repository scale (up
to 100,000 chunks for monorepos), brute-force SIMD is faster than ANN setup and
requires less maintenance.

## BM25

We use `bm25s.BM25(method="lucene")`'s exact formula:

```text
idf(t) = ln(1 + (N - df(t) + 0.5) / (df(t) + 0.5))
score(d, q) = sum(idf(t) * tf(t,d) * (k1+1)
                          / (tf(t,d) + k1 * (1 - b + b * |d| / avgdl)))
                          k1 = 1.5, b = 0.75
```

`get_scores(tokens, mask)` returns one f32 per chunk. The mask is a *post-filter
score multiplier* that matches `bm25s`'s `weight_mask` semantics. It is not a
slice, so filtering by language preserves IDF normalization over the full corpus.

The implementation is about 150 lines and controls the mask semantics. The
`bm25` crate does not expose them.

## RRF + ranking

BM25 and dense scores use different scales, so raw-magnitude combination does
not work. RRF (`1 / (k + rank)` with `k = 60`) normalizes both into the same
band before alpha-weighted blending.

Four boosting and penalty passes follow:

1. **`boost_multi_chunk_files`** lifts the top chunk for files with multiple
   high-scoring chunks by `0.2 * max_score * (file_sum / max_file_sum)`.
2. **Symbol queries** trigger `_boost_symbol_definitions`. Chunks that *define*
   the queried name get `3 * max_score`, or a 1.5x multiplier if the file stem
   matches the symbol. The pass also scans non-candidate chunks whose file stem
   matches. The definition matcher recognizes Zig `const` and `var`
   declarations, including `const Name = struct`, `enum`, `union`, `opaque`,
   and `error` types.
3. **NL queries** trigger `_boost_stem_matches` for file or directory names that
   overlap query keywords, plus `_boost_embedded_symbols` for PascalCase or
   camelCase identifiers in the query. The latter uses half the definition boost.
4. **`rerank_topk`** applies chunk-kind penalties (`Enclosing` 0.7x and
   `Outline` 0.8x), path penalties (test files 0.3x, compatibility or legacy
   directories 0.3x, examples 0.3x, generated files 0.3x, `.d.ts` 0.7x, and
   `__init__.py` or `package-info.java` 0.5x), and greedy file-saturation decay
   (second chunk from the same file 0.5x, third 0.25x, and so on).

### Determinism

Ranking must not depend on `HashMap` iteration order, which Rust randomizes per
process, or on a chunk's physical row id. Incremental indexes retain tombstoned
rows and append replacements, whereas a compact rebuild packs the same live
chunks back into file order. Five identical runs on the pinned corpus once
spanned recall@5 23-27% and MRR@10 0.196-0.223. That range could hide or invent
a change worth adopting.

- `boost_multi_chunk_files` accumulated a per-file score sum in map order.
  Float addition is not associative, so the sum differed each run; on an exact
  tie the "best chunk" of a file also went to whichever id came first.
- `file_prior` folded a file's several `Outline` pieces into one entry, letting
  the last piece in iteration order set the file's prior.
- Dense and BM25 candidate selection, file-coherence accumulation, and
  `rerank_topk` all need a stable tiebreak. Ties are common because RRF maps
  scores onto the small discrete set `1 / (k + rank)`.

These passes now compare a persistent logical identity made from the chunk's
repository path, byte and line range, kind, breadcrumb, language, and content.
Physical ids are used only when two chunks are otherwise indistinguishable.
`warm_and_compact_layouts_have_identical_logical_ranking` constructs both
layouts with a tombstone and tied candidate scores, then requires the complete
dense + BM25 + RRF + boost + rerank path to return bit-identical logical
results. Query-aware definition expansion receives the same eligibility mask,
so it cannot reintroduce tombstones or chunks excluded by language, scope,
path, or name filters.

**Generated-file down-ranking.** `generated_file_re` (`ranking.rs`) classifies
machine-generated paths by suffix or marker. It covers protobuf and gRPC stubs
(`.pb.go`, `_grpc.pb.go`, `_pb2.py`, `.pb.{cc,h,dart,ts}`, and others), Dart and
Flutter codegen (`.g.dart`, `.freezed.dart`), C# designer output (`.Designer.cs`,
`.g.cs`), gomock files (`_mock(s).go`, `mock_*.go`), minified bundles (`.min.js`,
`.bundle.js`), and anything under a `generated/` or `__generated__/` directory.
Suffixes are anchored to `$`, and the directory marker requires an exact path
component, so look-alikes (`general.rs`, `genesis.rs`, `codegen.rs`) never match.
This stops a query like `Send` against a protobuf-heavy repository from burying
the hand-written implementation under a dozen generated stubs.

## On-disk format

```
.ast-bro/
|-- .gitignore               # auto-written: "*"
`-- index/
    |-- meta.json            # ~2 KB, schema, model id+revision, chunk_count, tombstones
    |-- chunks.bin           # bincode Vec<Chunk> (~1.5 KB/chunk * N)
    |-- embeddings.f32       # N * 256 * 4 bytes, header-less, little-endian
    |-- bm25.bin             # bincode Bm25Index (vocab + idf + postings)
    |-- files.bin            # bincode Vec<FileRecord> (path + mtime + size + hash + chunk range)
    `-- lock                 # advisory exclusive lock during writes
```

The loader refuses the index if `meta.json.schema != "ast-bro.search-index.v3"`,
the model id mismatches, or
`chunks.len() * 256 * 4 != len(embeddings.f32)`. Bincode reads each binary with
`serde::Deserialize`. Embeddings are read into memory. A later mmap change will
not alter the format.

Schema v2 adds `breadcrumb` and `kind` to `Chunk`, which changes the bincode
layout of `chunks.bin`. A v1 index cannot be decoded, so the loader rejects it
outright, including the pre-rename `ast-outline.*` names. The caller rebuilds
the index because it is a cache; the format has no migration path.

Schema v3 keeps the same binary layout but changes Zig from plain-text chunks
to declaration-aware chunks. Rejecting v2 ensures an unchanged Zig file is
re-chunked instead of preserving stale cache entries during a delta update.

A chunk above `MAX_EMBED_CHARS = 6000` gets a non-finite embedding rather than a
real one, so `cosine_topk` drops it and only BM25 can retrieve it. A zero vector
would not do: its dot product is a finite `0.0`, which outranks every negative
similarity and enters the pool whenever the corpus is smaller than the pool.
`potion-code-16M` encodes a chunk as the mean of its token vectors, so a 40 KB
class drifts toward the centroid of the language and matches everything weakly.
Past that size, the vector carries less signal than the noise it adds. The
content stays reachable densely through the member's `Part` chunks. On pgjdbc
this affects 1.4% of chunks.

`embeddings.f32` is row-major, so a single chunk's vector is one cache-friendly
slice for both the in-memory and future-mmap paths.

The format carries the fields incremental updates need:

- `meta.json.tombstones: Vec<u32>` records logically deleted chunk ids, such as
  the old chunks for a removed or modified file, until compaction removes them.
- `FileRecord.chunk_start` / `chunk_end` stores each file's `[start, end)` range
  in `chunks.bin`, so a delta can drop one file's chunks without rewriting the rest.

On `Index::open`, `apply_delta` applies a non-empty delta incrementally. It
tombstones the changed files' old chunks, re-chunks and re-embeds added or
modified files, appends their chunks, and rebuilds BM25 over the live set.
Tombstone slots remain for chunk-id alignment but do not contribute to BM25's
document count, average document length, or postings. Retrieval and ranking
break score ties on logical chunk identity, so the warm layout and a compact
rebuild produce the same answer. It does not re-embed the untouched corpus. A
full rebuild happens only as a fallback
when `apply_delta` errors, or as compaction once tombstones exceed
`AST_BRO_COMPACTION_RATIO` (default 30%) of all chunk slots. The detection path
uses mtime and size, then hashes only on mismatch, which reduces the cost of
each open.

## Concurrency

The `fs2` advisory lock at `.ast-bro/index/lock` is exclusive during writes.
Two simultaneous `search` calls during a rebuild serialize; the second call
sees the first call's update on its next read. All writes use `.tmp` plus an
atomic rename, so a SIGKILL during a write leaves the previous index intact.

## Adding a new model

`ModelInfo::potion_code_16m()` is the only model wired in. To add another:

1. Add a constructor to `download::ModelInfo` that lists `config.json`,
   `tokenizer.json`, and `model.safetensors`.
2. Verify that the safetensors embedding tensor is named `embeddings` and uses
   `f32` or `f16`, as model2vec requires. `decode_f16_le` in `embed.rs` decodes
   `f16` tensors to `f32` once at open time; the embedder rejects other dtypes.
3. If the dimension differs from 256, update the `DIM` constant in
   [`src/search/embed.rs`](../src/search/embed.rs). Most of the code is generic
   over `DIM`, but this constant is the single source of truth. Changing it
   requires re-indexing existing repositories. The schema check in
   `Meta::model.dim` detects the change and forces a rebuild.

Measure the swap on [tests/eval](../tests/eval/README.md) before taking it. A
better public benchmark is not enough on its own: `potion-code-16M-v2` shares
this model's dimension and teacher and scores higher on CoIR (NDCG@10 39.08 vs
37.05), yet dropped recall@5 from 27% to 17% and MRR@10 from 0.223 to 0.117 on
the pinned pgjdbc set. CoIR leans on docstring-to-function tasks, and these
queries describe behavior over a real codebase.

The `AST_OUTLINE_MODEL_SOURCE` environment variable lets operators specify a
custom HF-compatible mirror without code changes.
