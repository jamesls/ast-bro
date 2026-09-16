//! Three-pass resolver: bare call names → qualified targets.
//!
//! Pass A — same-file/imports: a call site `foo()` resolves if `foo` is
//! defined in the same file (a `Qn` we already collected) or imported
//! directly. Zig namespace calls resolve bound imports (`helper.foo()`),
//! inline imports (`@import("helper.zig").foo()`), declaration scopes
//! (`helper.Type.init()`), and renamed facade chains. Bare callable aliases
//! resolve through the same binding graph. These forms reuse the existing
//! `src/deps/resolver` suffix index.
//!
//! Pass B — global symbol-table: any remaining bare name with exactly one
//! global qn match promotes to `Resolved(qn)`. Multiple matches defer.
//!
//! Pass C — dep-graph disambiguation: ambiguous names filter to candidates
//! whose file appears in the source file's transitive forward-dep closure.
//! Single survivor → `Inferred`; otherwise → `Ambiguous` with all
//! candidates kept under `CallEdge::candidates`.

use crate::calls::graph::{CallEdge, CallTarget, Confidence, Qn};
use crate::calls::pass::{file_rel, raw_to_edge, FilePass, RawEdge};
use crate::core::ImportBinding;
use crate::deps::manifest::{detect_aliases, ProjectAliases};
use crate::deps::resolver::{
    build_suffix_index, resolve as resolve_spec, Lang, ResolveCtx, SuffixIndex,
};
use crate::deps::traverse;
use crate::deps::DepGraph;
use crate::surface::zig::PublicLookup;
use crate::symbol_path::{
    first_qualified_separator, is_zig_identifier, last_qualified_separator, namespace_to_qualified,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Resolved {
    pub forward: HashMap<Qn, Vec<CallEdge>>,
    pub symbol_table: HashMap<String, Vec<Qn>>,
}

/// Result of interpreting a Zig call receiver as an imported namespace.
enum ZigNamespaceResolution {
    /// The receiver does not begin with an import binding.
    NotNamespace,
    /// The complete namespace path leads to a real callable.
    Resolved(Qn),
    /// The receiver is an import namespace, but its path or callable is absent.
    Unresolved,
    Candidates(Vec<Qn>),
}

/// Resolves Zig namespace receivers through local import bindings and the
/// language's public namespace rules.
///
/// The caller's own binding may be private (`const helper = @import(...)`).
/// Once resolution crosses that boundary, [`PublicLookup`] enforces `pub` on
/// every exposed alias, container, and callable, including members injected by
/// `pub usingnamespace`. This keeps same-file private calls legal without
/// claiming that another file can access a private declaration.
struct ZigNamespaceResolver<'a> {
    root: &'a Path,
    aliases: &'a ProjectAliases,
    suffix_idx: &'a SuffixIndex,
    symbol_table: &'a HashMap<String, Vec<Qn>>,
    pass_imports: HashMap<String, HashMap<String, ImportBinding>>,
    public_lookup: PublicLookup,
}

impl<'a> ZigNamespaceResolver<'a> {
    fn new(
        root: &'a Path,
        passes: &[FilePass],
        aliases: &'a ProjectAliases,
        suffix_idx: &'a SuffixIndex,
        symbol_table: &'a HashMap<String, Vec<Qn>>,
    ) -> Self {
        let mut pass_imports = HashMap::new();
        for pass in passes
            .iter()
            .filter(|pass| Lang::from_path(&pass.file) == Some(Lang::Zig))
        {
            pass_imports.insert(
                file_rel(root, &pass.file),
                binding_map(pass.imports.iter().cloned()),
            );
        }

        Self {
            root,
            aliases,
            suffix_idx,
            symbol_table,
            pass_imports,
            public_lookup: PublicLookup::new(64),
        }
    }

    fn resolve(
        &mut self,
        from_file: &Path,
        receiver: &str,
        bare_name: &str,
    ) -> ZigNamespaceResolution {
        let receiver = receiver.trim();
        if receiver == "@This()" || receiver.starts_with("@This().") {
            let mut members = receiver
                .strip_prefix("@This().")
                .map(|path| {
                    crate::symbol_path::split_dotted(path)
                        .into_iter()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            members.push(bare_name.to_string());
            return self.namespace_targets(from_file, &members, true);
        }
        let (target_file, mut members) = if receiver.starts_with("@import") {
            if let Some((spec, members)) = parse_zig_inline_import(receiver) {
                let Some(target) = self.resolve_spec(from_file, &spec) else {
                    return ZigNamespaceResolution::Unresolved;
                };
                (target, members)
            } else {
                return ZigNamespaceResolution::Unresolved;
            }
        } else {
            let Some(mut members) = zig_namespace_members(receiver) else {
                return ZigNamespaceResolution::NotNamespace;
            };
            let first = members.remove(0);
            let Some((target, member_path)) = self.resolve_binding(from_file, first) else {
                return if self.has_binding(from_file, first) {
                    ZigNamespaceResolution::Unresolved
                } else {
                    ZigNamespaceResolution::NotNamespace
                };
            };
            let mut pending = member_path;
            pending.extend(members.into_iter().map(str::to_string));
            (target, pending)
        };
        members.push(bare_name.to_string());
        self.namespace_targets(&target_file, &members, false)
    }

    /// Resolve `const local = imported.path.toCallable; local()` aliases.
    fn resolve_callable_alias(&mut self, from_file: &Path, local: &str) -> ZigNamespaceResolution {
        let Some((target_file, members)) = self.resolve_binding(from_file, local) else {
            return if self.has_binding(from_file, local) {
                ZigNamespaceResolution::Unresolved
            } else {
                ZigNamespaceResolution::NotNamespace
            };
        };
        if members.is_empty() {
            return ZigNamespaceResolution::NotNamespace;
        }

        self.exact_public_target(&target_file, &members).map_or(
            ZigNamespaceResolution::Unresolved,
            ZigNamespaceResolution::Resolved,
        )
    }

    fn has_binding(&self, from_file: &Path, local: &str) -> bool {
        let file = file_rel(self.root, from_file);
        self.pass_imports
            .get(&file)
            .is_some_and(|bindings| bindings.contains_key(local))
    }

    fn resolve_binding(&self, from_file: &Path, local: &str) -> Option<(PathBuf, Vec<String>)> {
        let file = file_rel(self.root, from_file);
        let binding = self
            .pass_imports
            .get(&file)
            .and_then(|bindings| bindings.get(local))?;
        self.resolve_spec(from_file, &binding.module)
            .map(|target| (target, binding.member_path.clone()))
    }

    fn resolve_spec(&self, from_file: &Path, spec: &str) -> Option<PathBuf> {
        let spec = normalize_zig_import_spec(spec);
        let ctx = ResolveCtx {
            from_file,
            lang: Lang::Zig,
            path_aliases: &self.aliases.ts_path_aliases,
            php_psr4: &self.aliases.php_psr4,
        };
        resolve_spec(&spec, &ctx, self.suffix_idx)
    }

    fn exact_public_target(&mut self, target_file: &Path, path: &[String]) -> Option<Qn> {
        let target = self.public_lookup.resolve(target_file, path)?;
        let bare_name = target.path.last()?;
        let file = file_rel(self.root, &target.file);
        let wanted = format!("{file}::{}", target.path.join("::"));
        self.symbol_table
            .get(bare_name)?
            .iter()
            .find(|candidate| candidate.as_str() == wanted)
            .cloned()
    }

    fn namespace_targets(
        &mut self,
        file: &Path,
        path: &[String],
        local: bool,
    ) -> ZigNamespaceResolution {
        if local {
            if let Some(name) = path.last() {
                let wanted = format!("{}::{}", file_rel(self.root, file), path.join("::"));
                if let Some(target) = self.symbol_table.get(name).and_then(|candidates| {
                    candidates.iter().find(|candidate| candidate.0 == wanted)
                }) {
                    return ZigNamespaceResolution::Resolved(target.clone());
                }
            }
        }
        let candidates = self
            .public_lookup
            .candidates(file, path, local)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|target| {
                let name = target.path.last()?;
                let wanted = format!(
                    "{}::{}",
                    file_rel(self.root, &target.file),
                    target.path.join("::")
                );
                self.symbol_table
                    .get(name)?
                    .iter()
                    .find(|candidate| candidate.0 == wanted)
                    .cloned()
            })
            .collect::<Vec<_>>();
        match candidates.len() {
            0 => ZigNamespaceResolution::Unresolved,
            1 => ZigNamespaceResolution::Resolved(candidates[0].clone()),
            _ => ZigNamespaceResolution::Candidates(candidates),
        }
    }
}

fn binding_map(bindings: impl Iterator<Item = ImportBinding>) -> HashMap<String, ImportBinding> {
    bindings
        .filter(|binding| !binding.local.is_empty())
        .map(|binding| (binding.local.clone(), binding))
        .collect()
}

/// Parse a dotted Zig namespace receiver into identifier members. Escaped
/// identifiers may contain dots, so separators are recognized only outside
/// `@"..."` strings.
fn zig_namespace_members(receiver: &str) -> Option<Vec<&str>> {
    let mut members = Vec::new();
    let bytes = receiver.as_bytes();
    let mut start = 0usize;
    let mut in_escaped_identifier = false;
    let mut escaped = false;

    for (index, &byte) in bytes.iter().enumerate() {
        if in_escaped_identifier {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_escaped_identifier = false;
            }
            continue;
        }
        if byte == b'"' && index > start && bytes[index - 1] == b'@' {
            in_escaped_identifier = true;
        } else if byte == b'.' {
            members.push(receiver[start..index].trim());
            start = index + 1;
        }
    }
    if in_escaped_identifier || escaped {
        return None;
    }
    members.push(receiver[start..].trim());
    (!members.is_empty() && members.iter().all(|member| is_zig_identifier(member)))
        .then_some(members)
}

/// Parse `@import("file.zig")` and optional namespace members after it.
fn parse_zig_inline_import(receiver: &str) -> Option<(String, Vec<String>)> {
    let rest = receiver
        .strip_prefix("@import")?
        .trim_start()
        .strip_prefix('(')?
        .trim_start();
    let mut escaped = false;
    let end = rest.bytes().enumerate().skip(1).find_map(|(index, byte)| {
        if escaped {
            escaped = false;
            None
        } else if byte == b'\\' {
            escaped = true;
            None
        } else {
            (byte == b'"').then_some(index)
        }
    })?;
    let spec = crate::zig_syntax::string_value(&rest[..=end])?;
    let suffix = rest[end + 1..].trim_start().strip_prefix(')')?.trim();
    let members = if suffix.is_empty() {
        Vec::new()
    } else {
        zig_namespace_members(suffix.strip_prefix('.')?)?
            .into_iter()
            .map(crate::zig_syntax::identifier)
            .collect()
    };
    Some((spec, members))
}

fn normalize_zig_import_spec(spec: &str) -> String {
    crate::zig_syntax::import_spec(spec)
}

/// Build the global `bare-name → Vec<Qn>` table from a slice of passes.
/// Lifted out of `run` so incremental updates can rebuild the table from
/// (cached + new) passes without going through the full resolver.
pub fn build_symbol_table(passes: &[FilePass]) -> HashMap<String, Vec<Qn>> {
    let mut symbol_table: HashMap<String, Vec<Qn>> = HashMap::new();
    for fp in passes {
        for qn in &fp.defined {
            symbol_table
                .entry(qn.name().to_string())
                .or_default()
                .push(qn.clone());
        }
    }
    for v in symbol_table.values_mut() {
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.dedup();
    }
    symbol_table
}

pub fn run(root: &Path, deps: &DepGraph, passes: Vec<FilePass>) -> Resolved {
    let symbol_table = build_symbol_table(&passes);
    run_with_table(root, deps, passes, symbol_table)
}

/// Resolve `passes`'s raw edges against a *prebuilt* symbol_table (which
/// must include every qn the resolver should be allowed to see). The
/// incremental path passes the full project's symbol_table here while
/// only handing in raw edges from changed files — so new edges in changed
/// files still resolve to qns defined elsewhere in the project.
pub fn run_with_table(
    root: &Path,
    deps: &DepGraph,
    passes: Vec<FilePass>,
    symbol_table: HashMap<String, Vec<Qn>>,
) -> Resolved {
    // ---------- Suffix index for import resolution (reused from deps). ----------
    let aliases = detect_aliases(root);
    let suffix_idx = build_suffix_index(root);
    let mut zig_namespaces =
        ZigNamespaceResolver::new(root, &passes, &aliases, &suffix_idx, &symbol_table);

    // ---------- Pass A + B per-file, then pass C with the dep graph. ----------
    let mut forward: HashMap<Qn, Vec<CallEdge>> = HashMap::new();
    let mut ambiguous_buffer: Vec<(RawEdge, PathBuf, Vec<Qn>)> = Vec::new();

    for fp in passes {
        let file_qns: HashSet<String> = fp.defined.iter().map(|q| q.name().to_string()).collect();
        let local_qn_by_name: HashMap<String, Qn> = fp
            .defined
            .iter()
            .map(|q| (q.name().to_string(), q.clone()))
            .collect();

        // Build a quick `local_name -> module spec` lookup for pass A.
        let import_lookup: HashMap<String, String> = fp
            .imports
            .iter()
            .map(|b| (b.local.clone(), b.module.clone()))
            .collect();

        let lang = Lang::from_path(&fp.file);

        for raw in fp.raw_edges {
            // An explicit receiver disqualifies same-file binding: a local
            // homonym must not shadow `connection.getCtx()` — the receiver
            // says the target lives on another object (issue #31). Self-like
            // receivers (`self`, `this`, …) and receiver-less calls still
            // bind locally, and a type-qualified call (`Foo::bar()`,
            // `Foo.bar()`) binds only to a local qn actually scoped under
            // that type.
            let self_like = receiver_is_self_like_for_lang(raw.receiver.as_deref(), lang);
            // `crate`/`super` name a *different* scope than the caller's
            // own: `crate::helper()` means the crate root and
            // `super::helper()` the parent module, never a sibling. Route
            // them through the anchored SelfRel walk below instead of
            // sibling preference, which would hand `crate::helper()` to the
            // caller-scope homonym tagged Exact. (In OO languages a bare
            // `super.m()` receiver names the parent type — also never the
            // caller's own scope.)
            let scope_shifting =
                lang != Some(Lang::Zig) && receiver_is_scope_shifting(raw.receiver.as_deref());

            // -------- Pass A: Zig namespace import resolution -------- //
            // A Zig `@import` names the receiver rather than the callee. The
            // namespace resolver handles bound imports (`helper.work()`),
            // inline imports (`@import("helper.zig").work()`), and re-export
            // chains (`s3.ls.lsMain()`). Check it before generic same-file
            // handling because Zig permits imports named `self`, `Self`,
            // `this`, `cls`, `crate`, or `super`; those spellings are
            // self-like only in other languages. A recognized namespace is
            // consumed even when it fails to resolve, preventing pass C from
            // binding an unrelated top-level or nested homonym.
            if lang == Some(Lang::Zig) {
                let resolution = if let Some(receiver) = raw.receiver.as_deref() {
                    zig_namespaces.resolve(&fp.file, receiver, &raw.bare_name)
                } else {
                    zig_namespaces.resolve_callable_alias(&fp.file, &raw.bare_name)
                };
                let resolved = match resolution {
                    ZigNamespaceResolution::NotNamespace => None,
                    ZigNamespaceResolution::Resolved(target) => {
                        Some((CallTarget::Resolved(target), Confidence::Exact, Vec::new()))
                    }
                    ZigNamespaceResolution::Unresolved => Some((
                        CallTarget::Bare(raw.bare_name.clone()),
                        Confidence::Ambiguous,
                        Vec::new(),
                    )),
                    ZigNamespaceResolution::Candidates(candidates) => Some((
                        CallTarget::Bare(raw.bare_name.clone()),
                        Confidence::Ambiguous,
                        candidates,
                    )),
                };
                if let Some((target, confidence, candidates)) = resolved {
                    let edge = raw_to_edge(
                        raw.clone(),
                        target,
                        confidence,
                        rel_path(root, &fp.file),
                        candidates,
                    );
                    forward.entry(edge.source.clone()).or_default().push(edge);
                    continue;
                }
            }

            // -------- Pass A (cont): same-file -------- //
            if file_qns.contains(&raw.bare_name) {
                let local_target = if self_like && !scope_shifting {
                    // Prefer the sibling under the caller's own scope:
                    // `self.shared()` inside `Greeter::caller` is
                    // `Greeter::shared`, not a same-file homonym from
                    // another class.
                    let sibling = last_qualified_separator(&raw.source.0)
                        .map(|i| format!("{}::{}", &raw.source.0[..i], raw.bare_name))
                        .and_then(|want| fp.defined.iter().find(|q| q.0 == want).cloned());
                    if raw.receiver.is_some() {
                        // An explicit `self`/`Self` is anchored to the
                        // caller's lexical scope. A same-file declaration in
                        // another type is a decoy, not a fallback target.
                        sibling
                    } else {
                        sibling.or_else(|| local_qn_by_name.get(&raw.bare_name).cloned())
                    }
                } else {
                    raw.receiver.as_deref().and_then(|recv| {
                        // Receivers can arrive namespace-qualified
                        // (`Foo\Greeter::m()` in PHP, `a::b::Type::m()`
                        // elsewhere). Normalize the separators and require
                        // the *complete* receiver path to match a local qn.
                        // No terminal-segment fallback: `other::Type::m()`
                        // explicitly names another scope's `Type`, and
                        // discarding the qualifiers would hand the edge to
                        // an unrelated same-file homonym tagged Exact. A
                        // miss falls through to pass B/C, where the dep
                        // graph either confirms the target (`Inferred`) or
                        // the edge stays honestly `Ambiguous`.
                        let normalized = namespace_to_qualified(recv);
                        if lang == Some(Lang::Zig)
                            && (recv == "@This()" || recv.starts_with("@This()."))
                        {
                            let scope = normalized.strip_prefix("@This()::").unwrap_or("");
                            let tail = if scope.is_empty() {
                                raw.bare_name.clone()
                            } else {
                                format!("{scope}::{}", raw.bare_name)
                            };
                            let want = format!("{}::{tail}", file_rel(root, &fp.file));
                            return fp.defined.iter().find(|q| q.0 == want).cloned();
                        }
                        // Rust self-relative prefixes (`crate::`, `self::`,
                        // `super::…`) never appear inside qns — those start
                        // with the repo-relative file path. Each prefix
                        // picks its own anchor (see the match arms below);
                        // matching is always *anchored equality*, never a
                        // bare suffix, and a miss falls through to pass
                        // B/C — no terminal-segment fallback.
                        #[derive(Clone, Copy)]
                        enum SelfRel {
                            Crate,
                            SelfMod,
                            Super(usize),
                        }
                        let mut rest = normalized.as_str();
                        let mut rel: Option<SelfRel> = None;
                        loop {
                            if lang == Some(Lang::Zig) {
                                break;
                            }
                            if let Some(r) = rest.strip_prefix("crate::") {
                                rest = r;
                                rel = Some(SelfRel::Crate);
                            } else if let Some(r) = rest.strip_prefix("self::") {
                                rest = r;
                                rel.get_or_insert(SelfRel::SelfMod);
                            } else if let Some(r) = rest.strip_prefix("super::") {
                                rest = r;
                                rel = Some(match rel {
                                    Some(SelfRel::Super(n)) => SelfRel::Super(n + 1),
                                    _ => SelfRel::Super(1),
                                });
                            } else if rest == "super" {
                                // A chain ending in the bare keyword has no
                                // trailing `::` for the arm above to strip:
                                // `super::super::helper()` arrives with
                                // receiver "super::super", and a plain
                                // `super::helper()` as just "super". Consume
                                // it like its prefixed form, leaving an
                                // empty rest.
                                rest = "";
                                rel = Some(match rel {
                                    Some(SelfRel::Super(n)) => SelfRel::Super(n + 1),
                                    _ => SelfRel::Super(1),
                                });
                            } else if rest == "crate" {
                                // Same bare-keyword form for the crate
                                // anchor: `crate::helper()` arrives with
                                // receiver "crate".
                                rest = "";
                                rel = Some(SelfRel::Crate);
                            } else {
                                break;
                            }
                        }
                        if let Some(rel) = rel {
                            // rest is empty when the receiver was nothing
                            // but prefix keywords ("super::super") — the
                            // target then sits directly in the anchored
                            // scope, with no path segments in between.
                            let want_tail = if rest.is_empty() {
                                format!("::{}", raw.bare_name)
                            } else {
                                format!("::{}::{}", rest, raw.bare_name)
                            };
                            let caller = raw.source.0.as_str();
                            return match rel {
                                // `crate::P` anchors at the *crate root*
                                // (`src/lib.rs` / `src/main.rs`), which is
                                // the caller's own file only when the caller
                                // lives there. Anywhere else the two differ,
                                // and anchoring at the caller's file binds
                                // `crate::inner::Foo::method()` to a
                                // same-file `mod inner` decoy while the real
                                // `crate::inner` is another file — so the
                                // arm fires exactly where it would be wrong.
                                // Off the crate root the edge falls through
                                // to pass B/C, which resolve it against the
                                // whole project.
                                SelfRel::Crate => {
                                    let file = first_qualified_separator(caller)
                                        .map_or(caller, |index| &caller[..index]);
                                    if !is_crate_root(file) {
                                        return None;
                                    }
                                    let want = format!("{}{}", file, want_tail);
                                    fp.defined.iter().find(|q| q.0 == want).cloned()
                                }
                                // `self::P` starts at the caller's
                                // enclosing scope; each `super::` skips one
                                // more level before the walk begins, so an
                                // ancestor decoy at the caller's own level
                                // cannot shadow the parent. (A method's qn
                                // carries type segments the resolver can't
                                // tell from modules, so the walk continues
                                // upward past the start — anchored equality
                                // keeps any hit real.)
                                SelfRel::SelfMod | SelfRel::Super(_) => {
                                    let skip = match rel {
                                        SelfRel::Super(n) => n,
                                        _ => 0,
                                    };
                                    let mut cur = caller;
                                    std::iter::from_fn(|| {
                                        let i = last_qualified_separator(cur)?;
                                        cur = &cur[..i];
                                        Some(cur)
                                    })
                                    .skip(skip)
                                    .find_map(|base| {
                                        let want = format!("{}{}", base, want_tail);
                                        fp.defined.iter().find(|q| q.0 == want).cloned()
                                    })
                                }
                            };
                        }
                        let full = format!("::{}::{}", normalized, raw.bare_name);
                        // Bind only a candidate whose lexical scope encloses
                        // the *caller* (innermost wins) — that is what the
                        // path means in the source. The filter applies to a
                        // single match too: one `Foo::method` declared in a
                        // sibling module is a decoy, not the target, and
                        // must defer to pass B/C rather than take same-file
                        // Exact just for being alone. With several matches
                        // (`mod a`/`mod b` each with `Foo::method`) the same
                        // rule keeps the edge honestly Ambiguous instead of
                        // taking whichever match came first.
                        let caller = raw.source.0.as_str();
                        fp.defined
                            .iter()
                            .filter(|q| q.0.ends_with(&full))
                            .filter(|q| {
                                let scope = &q.0[..q.0.len() - full.len()];
                                caller == scope || caller.starts_with(&format!("{}::", scope))
                            })
                            .max_by_key(|q| q.0.len())
                            .cloned()
                    })
                };
                if let Some(qn) = local_target {
                    let edge = raw_to_edge(
                        raw.clone(),
                        CallTarget::Resolved(qn),
                        Confidence::Exact,
                        rel_path(root, &fp.file),
                        Vec::new(),
                    );
                    forward.entry(edge.source.clone()).or_default().push(edge);
                    continue;
                }
                // Explicit receiver with no locally-scoped match: fall
                // through to pass B/C rather than mis-bind.
            }

            if lang == Some(Lang::Zig) && raw.receiver.is_some() {
                // Zig receivers that survive namespace and exact lexical
                // matching have no trustworthy target type. Dependency-only
                // inference can bind a parameter or local value to an
                // unrelated homonym, including one imported in a different
                // function. Keep the edge unresolved until the adapter can
                // supply a concrete receiver type.
                let edge = raw_to_edge(
                    raw.clone(),
                    CallTarget::Bare(raw.bare_name.clone()),
                    Confidence::Ambiguous,
                    rel_path(root, &fp.file),
                    Vec::new(),
                );
                forward.entry(edge.source.clone()).or_default().push(edge);
                continue;
            }

            // -------- Pass A (cont): direct import resolution -------- //
            // Same gate: `obj.parse()` must not bind to an imported free
            // function `parse`. Scope-shifting receivers bypass imports too:
            // `crate::helper()` explicitly names a path, not the imported
            // `helper` binding.
            if self_like && !scope_shifting {
                if let Some(spec) = import_lookup.get(&raw.bare_name) {
                    if let Some(target) = resolve_via_imports(
                        spec,
                        &raw.bare_name,
                        &fp.file,
                        lang,
                        &aliases,
                        &suffix_idx,
                        root,
                        &symbol_table,
                        true,
                    ) {
                        let edge = raw_to_edge(
                            raw.clone(),
                            CallTarget::Resolved(target),
                            Confidence::Exact,
                            rel_path(root, &fp.file),
                            Vec::new(),
                        );
                        forward.entry(edge.source.clone()).or_default().push(edge);
                        continue;
                    }
                }
            }

            // -------- Pass B: global symbol table -------- //
            //
            // Receiver-bearing calls (`obj.method()`) only get pass B
            // promotion when the dep graph confirms a relationship — without
            // a resolved type for `obj`, single-name matches are too noisy
            // (e.g. `builder.hidden()` would resolve to any project method
            // happening to be called `hidden`). Defer them to pass C.
            // Scope-shifting receivers defer too: `crate::helper()` whose
            // one global `helper` sits in a sibling module names a symbol
            // that does not exist — promoting the single match binds Exact
            // to a scope the keyword explicitly rules out. Pass B cannot
            // check the anchor, so the edge goes to pass C's dep filter.
            let has_receiver = !self_like || scope_shifting;
            match symbol_table.get(&raw.bare_name) {
                Some(cands) if cands.len() == 1 && !has_receiver => {
                    let edge = raw_to_edge(
                        raw.clone(),
                        CallTarget::Resolved(cands[0].clone()),
                        Confidence::Exact,
                        rel_path(root, &fp.file),
                        Vec::new(),
                    );
                    forward.entry(edge.source.clone()).or_default().push(edge);
                }
                Some(cands) if !cands.is_empty() => {
                    // Defer to pass C — either ambiguous (>1) or
                    // receiver-bearing single match.
                    ambiguous_buffer.push((raw.clone(), fp.file.clone(), cands.clone()));
                }
                _ => {
                    // 0 candidates: keep as Bare. Could be external, could
                    // be dynamically dispatched. We don't try to distinguish
                    // External here (would require deeper import tracking).
                    let edge = raw_to_edge(
                        raw.clone(),
                        CallTarget::Bare(raw.bare_name.clone()),
                        Confidence::Ambiguous,
                        rel_path(root, &fp.file),
                        Vec::new(),
                    );
                    forward.entry(edge.source.clone()).or_default().push(edge);
                }
            }
        }

        // Make sure every defined function shows up in `forward`, even the
        // leaves that called nothing — `callees` should return a clean
        // "no callees" instead of "function not found".
        for qn in fp.defined {
            forward.entry(qn).or_default();
        }
    }

    // ---------- Pass C: dep-graph disambiguation for ambiguous bares. ----------
    let mut closures = ClosureCache::default();
    for (raw, src_file, cands) in ambiguous_buffer {
        let (target, confidence, candidates) =
            disambiguate(deps, root, &src_file, &raw.bare_name, &cands, &mut closures);
        let edge = raw_to_edge(
            raw,
            target,
            confidence,
            rel_path(root, &src_file),
            candidates,
        );
        forward.entry(edge.source.clone()).or_default().push(edge);
    }

    Resolved {
        forward,
        symbol_table,
    }
}

/// Is this repo-relative file a Rust crate root — the module `crate::`
/// names? Only there does the caller's file segment coincide with the crate
/// root, which is what the `crate::` anchor needs (`src/bin/*.rs` binaries
/// are roots of their own crates too). The `bin` test is anchored to Cargo's
/// layout: `tools/bin/helper.rs` and `vendor/bin/x.rs` are ordinary files,
/// not crate roots, and anchoring `crate::` at them mis-binds every call.
fn is_crate_root(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    matches!(name, "lib.rs" | "main.rs")
        || file.contains("/src/bin/")
        || file.starts_with("src/bin/")
}

/// A receiver that is a scope keyword rather than an object: absent
/// (`foo()`), or one of the language self/scope keywords. Anything else —
/// a variable, field, or type name — means the call targets another object,
/// so same-file and single-global-match promotion must not claim it.
/// (`self`/`Self`/`crate`/`super` — Rust; `this` — Java/TS/C#/C++/Kotlin/
/// Scala; `$this` — PHP; `cls` — Python, the conventional first parameter of
/// a `@classmethod`, where `cls.helper()` means the enclosing class exactly
/// as `self.helper()` does. PHP's `self::`/`static::`/`parent::` scoped calls
/// never reach here — the adapter normalizes those receivers to `None` —
/// so listing the bare words would only ever match user variables named
/// `static`/`parent`, which are common and NOT self-like.)
///
/// Nuance: `crate` and `super` are keywords but name a scope *other than*
/// the caller's own, so both pass A and pass B refine this gate with
/// [`receiver_is_scope_shifting`]: same-file binding routes them through
/// the anchored SelfRel walk instead of sibling preference, and pass B
/// withholds single-global-match promotion — the one match may sit in a
/// scope the keyword explicitly rules out, and pass B cannot check the
/// anchor. `delta.rs` mirrors pass B through both functions.
pub(crate) fn receiver_is_self_like(recv: Option<&str>) -> bool {
    matches!(
        recv,
        None | Some("self")
            | Some("Self")
            | Some("cls")
            | Some("crate")
            | Some("super")
            | Some("this")
            | Some("$this")
    )
}

/// Apply the receiver spelling rules for a specific source language.
///
/// Zig uses `self` and `Self` by convention. The other generic spellings are
/// ordinary Zig identifiers and must retain explicit-object semantics.
pub(crate) fn receiver_is_self_like_for_lang(recv: Option<&str>, lang: Option<Lang>) -> bool {
    if lang == Some(Lang::Zig) {
        matches!(recv, None | Some("self") | Some("Self"))
    } else {
        receiver_is_self_like(recv)
    }
}

/// A scope-keyword receiver that names a scope *other than* the caller's
/// own — the refinement pass A and pass B apply on top of
/// [`receiver_is_self_like`] (see its doc). Keyword *chains*
/// ("super::super") never appear here: they are not self-like in the first
/// place, so both passes already treat them as receiver-bearing.
pub(crate) fn receiver_is_scope_shifting(recv: Option<&str>) -> bool {
    matches!(recv, Some("crate") | Some("super"))
}

/// Resolve `from <module> import <name>` (or equivalent) by mapping `module`
/// to a project file via the suffix index, then qualifying `name` inside it.
/// Returns `None` when the module isn't resolvable to a project file
/// (likely an external dep — we leave that to pass-B/C).
#[allow(clippy::too_many_arguments)] // resolution context is genuinely wide
fn resolve_via_imports(
    spec: &str,
    bare_name: &str,
    from_file: &Path,
    lang: Option<Lang>,
    aliases: &crate::deps::manifest::ProjectAliases,
    idx: &crate::deps::resolver::SuffixIndex,
    root: &Path,
    symbol_table: &HashMap<String, Vec<Qn>>,
    allow_fallback: bool,
) -> Option<Qn> {
    let lang = lang?;
    let ctx = ResolveCtx {
        from_file,
        lang,
        path_aliases: &aliases.ts_path_aliases,
        php_psr4: &aliases.php_psr4,
    };
    let target_file = resolve_spec(spec, &ctx, idx)?;
    let rel = file_rel(root, &target_file);
    let file_scope_qn = format!("{}::{}", rel, bare_name);

    // A namespace member names the imported file's top-level declaration.
    // Prefer that exact qn before considering any legacy direct-import
    // fallback; symbol-table ordering must not turn `helper.work()` into
    // `helper.zig::SomeType::work`.
    if let Some(cands) = symbol_table.get(bare_name) {
        if let Some(cand) = cands.iter().find(|cand| cand.as_str() == file_scope_qn) {
            return Some(cand.clone());
        }
        if !allow_fallback {
            return None;
        }
        for cand in cands {
            if cand.file() == rel {
                return Some(cand.clone());
            }
        }
    }
    if !allow_fallback {
        return None;
    }
    // Fall back: synthesize a file-scope qn (not always correct for nested
    // declarations, but better than dropping the edge).
    Some(Qn::new(file_scope_qn))
}

/// Per-file forward-dep closures, memoized. A file's closure is the same for
/// every ambiguous edge inside it, and the incremental updater re-asks for it
/// across a whole sweep.
#[derive(Default)]
pub(crate) struct ClosureCache(HashMap<PathBuf, HashSet<PathBuf>>);

impl ClosureCache {
    fn get(&mut self, deps: &DepGraph, from: &Path) -> &HashSet<PathBuf> {
        self.0
            .entry(from.to_path_buf())
            .or_insert_with(|| forward_closure_files(deps, from))
    }
}

/// Pass C's decision for one ambiguous bare edge: filter the candidates to
/// those the caller's file can actually reach through the dep graph, then
/// resolve (single survivor) or stay honestly ambiguous.
///
/// Shared with the incremental updater in `graph_cache::delta` so a partial
/// update and a cold build of the same content produce the same edge. That
/// mattered concretely: the updater used to assign the *unfiltered* global
/// symbol-table entry as an edge's candidate set, so one unrelated file edit
/// gave every `Vec::new()` in the project a candidate the dep filter had
/// ruled out — inflating `callers CliError.new`'s unresolved-site count from
/// 1023 to 1073 with no source change behind it.
pub(crate) fn disambiguate(
    deps: &DepGraph,
    root: &Path,
    src_file: &Path,
    bare_name: &str,
    cands: &[Qn],
    closures: &mut ClosureCache,
) -> (CallTarget, Confidence, Vec<Qn>) {
    let closure = closures.get(deps, src_file);
    let filtered: Vec<Qn> = cands
        .iter()
        .filter(|qn| {
            let cand_file = root.join(qn.file().replace('/', std::path::MAIN_SEPARATOR_STR));
            closure.contains(&cand_file)
        })
        .cloned()
        .collect();

    if filtered.len() == 1 {
        (
            CallTarget::Resolved(filtered[0].clone()),
            Confidence::Inferred,
            Vec::new(),
        )
    } else if filtered.is_empty() {
        // No dep-relationship — fall back to ambiguous over the full set.
        (
            CallTarget::Bare(bare_name.to_string()),
            Confidence::Ambiguous,
            cands.to_vec(),
        )
    } else {
        // Multiple survivors — keep as ambiguous with the surviving set.
        (
            CallTarget::Bare(bare_name.to_string()),
            Confidence::Ambiguous,
            filtered,
        )
    }
}

fn forward_closure_files(deps: &DepGraph, from: &Path) -> HashSet<PathBuf> {
    let hits = traverse::forward(deps, from, 8);
    let mut out: HashSet<PathBuf> = hits.into_iter().map(|h| h.file).collect();
    out.insert(from.to_path_buf());
    out
}

fn rel_path(root: &Path, file: &Path) -> PathBuf {
    file.strip_prefix(root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| file.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::graph::CallKindCompat;
    use crate::core::ImportBinding;

    fn empty_pass(file: PathBuf, defined: Vec<Qn>) -> FilePass {
        FilePass {
            file,
            defined,
            callable_locations: Vec::new(),
            imports: Vec::new(),
            raw_edges: Vec::new(),
            types: Vec::new(),
        }
    }

    #[test]
    fn namespace_import_receiver_resolves_exactly_despite_homonyms() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        let main_file = root.join("main.zig");
        let helper_file = root.join("helper.zig");
        let decoy_file = root.join("decoy.zig");
        std::fs::write(
            &main_file,
            "const helper = @import(\"./helper.zig\");\npub fn caller() void { helper.work(); }\n",
        )
        .expect("write Zig caller");
        std::fs::write(&helper_file, "pub fn work() void {}\n").expect("write imported Zig file");
        std::fs::write(&decoy_file, "pub fn work() void {}\n").expect("write decoy Zig file");

        let source = Qn::new("main.zig::caller");
        let mut main = empty_pass(main_file, vec![source.clone(), Qn::new("main.zig::work")]);
        main.imports.push(ImportBinding {
            local: "helper".to_string(),
            module: "./helper.zig".to_string(),
            member_path: Vec::new(),
            line: 1,
        });
        main.raw_edges.push(RawEdge {
            source: source.clone(),
            bare_name: "work".to_string(),
            receiver: Some("helper".to_string()),
            kind: CallKindCompat::Call,
            line: 2,
        });

        let helper = empty_pass(
            helper_file,
            vec![
                Qn::new("helper.zig::Nested::work"),
                Qn::new("helper.zig::work"),
            ],
        );
        let decoy = empty_pass(decoy_file, vec![Qn::new("decoy.zig::work")]);
        let deps = DepGraph::empty(root.to_path_buf());

        let resolved = run(root, &deps, vec![main, helper, decoy]);
        let edge = resolved
            .forward
            .get(&source)
            .and_then(|edges| edges.first())
            .expect("caller edge");

        assert_eq!(edge.confidence, Confidence::Exact);
        match &edge.target {
            CallTarget::Resolved(target) => {
                assert_eq!(target, &Qn::new("helper.zig::work"));
            }
            target => panic!("expected resolved namespace call, got {}", target.display()),
        }
    }

    #[test]
    fn namespace_import_does_not_bind_nested_or_invented_target() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        let main_file = root.join("main.zig");
        let helper_file = root.join("helper.zig");
        std::fs::write(
            &main_file,
            "const helper = @import(\"./helper.zig\");\npub fn caller() void { helper.work(); }\n",
        )
        .expect("write Zig caller");
        std::fs::write(
            &helper_file,
            "pub const Nested = struct { pub fn work() void {} };\n",
        )
        .expect("write imported Zig file");

        let source = Qn::new("main.zig::caller");
        let mut main = empty_pass(main_file, vec![source.clone()]);
        main.imports.push(ImportBinding {
            local: "helper".to_string(),
            module: "./helper.zig".to_string(),
            member_path: Vec::new(),
            line: 1,
        });
        main.raw_edges.push(RawEdge {
            source: source.clone(),
            bare_name: "work".to_string(),
            receiver: Some("helper".to_string()),
            kind: CallKindCompat::Call,
            line: 2,
        });
        let helper = empty_pass(helper_file, vec![Qn::new("helper.zig::Nested::work")]);

        let resolved = run(
            root,
            &DepGraph::empty(root.to_path_buf()),
            vec![main, helper],
        );
        let edge = resolved
            .forward
            .get(&source)
            .and_then(|edges| edges.first())
            .expect("caller edge");

        assert!(matches!(&edge.target, CallTarget::Bare(name) if name == "work"));
        assert_eq!(edge.confidence, Confidence::Ambiguous);
        assert!(edge.candidates.is_empty());
    }

    #[test]
    fn inline_import_receiver_resolves_exact_top_level_target() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        let main_file = root.join("main.zig");
        let helper_file = root.join("helper.zig");
        let decoy_file = root.join("decoy.zig");
        std::fs::write(
            &main_file,
            "pub fn caller() void { @import(\"helper.zig\").work(); }\n",
        )
        .expect("write Zig caller");
        std::fs::write(
            &helper_file,
            "pub fn work() void {}\npub const Nested = struct { pub fn work() void {} };\n",
        )
        .expect("write imported Zig file");
        std::fs::write(&decoy_file, "pub fn work() void {}\n").expect("write decoy Zig file");

        let source = Qn::new("main.zig::caller");
        let mut main = empty_pass(main_file, vec![source.clone()]);
        main.raw_edges.push(RawEdge {
            source: source.clone(),
            bare_name: "work".to_string(),
            receiver: Some("@import(\"helper.zig\")".to_string()),
            kind: CallKindCompat::Call,
            line: 1,
        });
        let helper = empty_pass(
            helper_file,
            vec![
                Qn::new("helper.zig::Nested::work"),
                Qn::new("helper.zig::work"),
            ],
        );
        let decoy = empty_pass(decoy_file, vec![Qn::new("decoy.zig::work")]);

        let resolved = run(
            root,
            &DepGraph::empty(root.to_path_buf()),
            vec![main, helper, decoy],
        );
        let edge = resolved
            .forward
            .get(&source)
            .and_then(|edges| edges.first())
            .expect("caller edge");

        assert_eq!(edge.confidence, Confidence::Exact);
        assert!(matches!(
            &edge.target,
            CallTarget::Resolved(target) if target == &Qn::new("helper.zig::work")
        ));
    }

    #[test]
    fn namespace_import_can_end_in_exact_type_scope() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        let main_file = root.join("main.zig");
        let helper_file = root.join("helper.zig");
        let decoy_file = root.join("decoy.zig");
        std::fs::write(
            &main_file,
            "const helper = @import(\"helper.zig\");\npub fn caller() void { helper.Probe.init(); }\n",
        )
        .expect("write Zig caller");
        std::fs::write(
            &helper_file,
            "pub const Probe = struct { pub fn init() void {} };\npub const Other = struct { pub fn init() void {} };\n",
        )
        .expect("write imported Zig file");
        std::fs::write(&decoy_file, "pub fn init() void {}\n").expect("write decoy Zig file");

        let source = Qn::new("main.zig::caller");
        let mut main = empty_pass(main_file, vec![source.clone()]);
        main.imports.push(ImportBinding {
            local: "helper".to_string(),
            module: "./helper.zig".to_string(),
            member_path: Vec::new(),
            line: 1,
        });
        main.raw_edges.push(RawEdge {
            source: source.clone(),
            bare_name: "init".to_string(),
            receiver: Some("helper.Probe".to_string()),
            kind: CallKindCompat::Call,
            line: 2,
        });
        let helper = empty_pass(
            helper_file,
            vec![
                Qn::new("helper.zig::Other::init"),
                Qn::new("helper.zig::Probe::init"),
            ],
        );
        let decoy = empty_pass(decoy_file, vec![Qn::new("decoy.zig::init")]);

        let resolved = run(
            root,
            &DepGraph::empty(root.to_path_buf()),
            vec![main, helper, decoy],
        );
        let edge = resolved
            .forward
            .get(&source)
            .and_then(|edges| edges.first())
            .expect("caller edge");

        assert_eq!(edge.confidence, Confidence::Exact);
        assert!(matches!(
            &edge.target,
            CallTarget::Resolved(target) if target == &Qn::new("helper.zig::Probe::init")
        ));
    }

    #[test]
    fn namespace_chain_follows_imports_across_file_passes() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        let main_file = root.join("main.zig");
        let facade_file = root.join("s3.zig");
        let command_file = root.join("s3/ls.zig");
        let decoy_file = root.join("decoy.zig");
        std::fs::create_dir_all(command_file.parent().expect("command parent"))
            .expect("create s3 directory");
        std::fs::write(
            &main_file,
            "const s3 = @import(\"s3.zig\");\npub fn caller() void { s3.ls.lsMain(); }\n",
        )
        .expect("write Zig caller");
        std::fs::write(&facade_file, "pub const ls = @import(\"s3/ls.zig\");\n")
            .expect("write Zig facade");
        std::fs::write(
            &command_file,
            "pub fn lsMain() void {}\npub const Nested = struct { pub fn lsMain() void {} };\n",
        )
        .expect("write Zig command");
        std::fs::write(&decoy_file, "pub fn lsMain() void {}\n").expect("write decoy Zig file");

        let source = Qn::new("main.zig::caller");
        let mut main = empty_pass(main_file, vec![source.clone()]);
        main.imports.push(ImportBinding {
            local: "s3".to_string(),
            module: "./s3.zig".to_string(),
            member_path: Vec::new(),
            line: 1,
        });
        main.raw_edges.push(RawEdge {
            source: source.clone(),
            bare_name: "lsMain".to_string(),
            receiver: Some("s3.ls".to_string()),
            kind: CallKindCompat::Call,
            line: 2,
        });
        let mut facade = empty_pass(facade_file, Vec::new());
        facade.imports.push(ImportBinding {
            local: "ls".to_string(),
            module: "./s3/ls.zig".to_string(),
            member_path: Vec::new(),
            line: 1,
        });
        let command = empty_pass(
            command_file,
            vec![
                Qn::new("s3/ls.zig::Nested::lsMain"),
                Qn::new("s3/ls.zig::lsMain"),
            ],
        );
        let decoy = empty_pass(decoy_file, vec![Qn::new("decoy.zig::lsMain")]);

        let resolved = run(
            root,
            &DepGraph::empty(root.to_path_buf()),
            vec![main, facade, command, decoy],
        );
        let edge = resolved
            .forward
            .get(&source)
            .and_then(|edges| edges.first())
            .expect("caller edge");

        assert_eq!(edge.confidence, Confidence::Exact);
        assert!(matches!(
            &edge.target,
            CallTarget::Resolved(target) if target == &Qn::new("s3/ls.zig::lsMain")
        ));
    }

    #[test]
    fn namespace_chain_does_not_fall_through_to_decoy() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        let main_file = root.join("main.zig");
        let facade_file = root.join("s3.zig");
        let decoy_file = root.join("decoy.zig");
        std::fs::write(
            &main_file,
            "const s3 = @import(\"s3.zig\");\npub fn caller() void { s3.missing.lsMain(); }\n",
        )
        .expect("write Zig caller");
        std::fs::write(&facade_file, "pub const other = 1;\n").expect("write Zig facade");
        std::fs::write(&decoy_file, "pub fn lsMain() void {}\n").expect("write decoy Zig file");

        let source = Qn::new("main.zig::caller");
        let mut main = empty_pass(main_file, vec![source.clone()]);
        main.imports.push(ImportBinding {
            local: "s3".to_string(),
            module: "./s3.zig".to_string(),
            member_path: Vec::new(),
            line: 1,
        });
        main.raw_edges.push(RawEdge {
            source: source.clone(),
            bare_name: "lsMain".to_string(),
            receiver: Some("s3.missing".to_string()),
            kind: CallKindCompat::Call,
            line: 2,
        });
        let facade = empty_pass(facade_file, Vec::new());
        let decoy = empty_pass(decoy_file, vec![Qn::new("decoy.zig::lsMain")]);

        let resolved = run(
            root,
            &DepGraph::empty(root.to_path_buf()),
            vec![main, facade, decoy],
        );
        let edge = resolved
            .forward
            .get(&source)
            .and_then(|edges| edges.first())
            .expect("caller edge");

        assert!(matches!(&edge.target, CallTarget::Bare(name) if name == "lsMain"));
        assert_eq!(edge.confidence, Confidence::Ambiguous);
        assert!(edge.candidates.is_empty());
    }
}
