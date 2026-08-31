//! Zig public-surface resolver.
//!
//! A Zig file is a namespace. Public `const` declarations may expose another
//! file's namespace (`pub const net = @import("net.zig")`) or republish one
//! declaration from it (`pub const Client = net.InternalClient`). In addition,
//! `pub usingnamespace` injects the public declarations of a namespace into
//! the current one. The generic visibility fallback cannot model any of those
//! forms, so this resolver follows them explicitly.
//!
//! Only relative, literal `.zig` imports are followed. Named modules are wired
//! by arbitrary `build.zig` code, and resolving them would require executing a
//! project's build program. They remain visible as alias fields but are not
//! expanded.

use crate::core::{Declaration, DeclarationKind};
use crate::parse_file;
use crate::surface::entry::{ReExportHop, SurfaceEntry};
use crate::surface::entry_point::EntryPoint;
use crate::surface::options::{SurfaceError, SurfaceOptions};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub fn resolve(
    entry: &EntryPoint,
    opts: &SurfaceOptions,
) -> Result<Vec<SurfaceEntry>, SurfaceError> {
    let root_file = match entry {
        EntryPoint::ZigModule { root_file } => root_file.clone(),
        _ => {
            return Err(SurfaceError::NoEntryPoint {
                path: PathBuf::from("."),
                hint: "zig::resolve called with non-Zig entry point".into(),
            });
        }
    };

    let mut walker = Walker::new(opts.max_depth, opts.include_private);
    if walker.snapshot(&root_file).is_none() {
        return Err(SurfaceError::Parse {
            path: root_file,
            message: "could not parse Zig module".into(),
        });
    }
    walker.walk_namespace(&root_file, &[], &[], 0, vec![], false);
    Ok(walker.entries)
}

#[derive(Clone)]
struct FileSnapshot {
    declarations: Vec<Declaration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Target {
    file: PathBuf,
    /// Empty means the file namespace; otherwise this names a declaration.
    path: Vec<String>,
}

struct ResolvedValue {
    target: Target,
    /// Private facade aliases are implementation details, but retaining them
    /// in the provenance makes an `--include-chain` result explainable.
    hops: Vec<ReExportHop>,
}

/// A declaration reached through Zig's public namespace semantics.
///
/// Call-graph resolution uses this narrow result rather than duplicating the
/// surface resolver's alias, visibility, ambiguity, and `usingnamespace`
/// rules.
pub(crate) struct PublicPath {
    pub file: PathBuf,
    pub path: Vec<String>,
}

/// Reusable public-member lookup for call-graph namespace receivers.
pub(crate) struct PublicLookup {
    walker: Walker,
}

impl PublicLookup {
    pub fn new(max_depth: usize) -> Self {
        Self {
            walker: Walker::new(max_depth, false),
        }
    }

    /// Resolve every segment as a publicly reachable namespace member.
    /// Public aliases may use private implementation aliases in their own
    /// file, but every declaration exposed across a file boundary must be
    /// public. Ambiguous direct/`usingnamespace` collisions return `None`.
    pub fn resolve(&mut self, file: &Path, path: &[String]) -> Option<PublicPath> {
        if path.is_empty() {
            return None;
        }
        let mut target = Target {
            file: file.to_path_buf(),
            path: Vec::new(),
        };
        let mut alias_stack = HashSet::new();
        let mut hops = Vec::new();
        for (index, member) in path.iter().enumerate() {
            target = self.walker.lookup_namespace_member(
                &target,
                member,
                index + 1,
                &mut alias_stack,
                &mut hops,
                true,
            )?;
            target =
                self.walker
                    .dereference_alias(target, index + 1, &mut alias_stack, &mut hops)?;
        }
        Some(PublicPath {
            file: target.file,
            path: target.path,
        })
    }
}

struct Walker {
    max_depth: usize,
    include_private: bool,
    loaded: HashMap<PathBuf, FileSnapshot>,
    /// Active stack rather than a global visited set: importing one module
    /// under two public names must expand it under both names.
    active_namespaces: HashSet<(PathBuf, String)>,
    /// A cycle may revisit one active namespace so its non-cyclic members are
    /// still visible under the new prefix. Further active edges are cut.
    cycle_closure_active: bool,
    /// Public names declared directly in an exposed namespace conflict with
    /// names injected by `usingnamespace`.
    direct_qualified: HashSet<String>,
    /// First source declaration injected under each qualified name. Repeating
    /// the same declaration is harmless; a distinct source is ambiguous.
    injected_qualified: HashMap<String, (PathBuf, String)>,
    ambiguous_qualified: HashSet<String>,
    seen_qualified: HashSet<String>,
    entries: Vec<SurfaceEntry>,
}

impl Walker {
    fn new(max_depth: usize, include_private: bool) -> Self {
        Self {
            max_depth,
            include_private,
            loaded: HashMap::new(),
            active_namespaces: HashSet::new(),
            cycle_closure_active: false,
            direct_qualified: HashSet::new(),
            injected_qualified: HashMap::new(),
            ambiguous_qualified: HashSet::new(),
            seen_qualified: HashSet::new(),
            entries: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_namespace(
        &mut self,
        file: &Path,
        source_scope: &[String],
        exposed_prefix: &[String],
        depth: usize,
        chain: Vec<ReExportHop>,
        via_glob: bool,
    ) {
        if depth > self.max_depth {
            return;
        }
        let key = (cache_key(file), source_scope.join("."));
        let inserted = self.active_namespaces.insert(key.clone());
        if !inserted && self.cycle_closure_active {
            return;
        }
        let closes_cycle = !inserted;
        if closes_cycle {
            self.cycle_closure_active = true;
        }

        if let Some(declarations) = self.declarations_in_scope(file, source_scope) {
            if !via_glob {
                self.reserve_direct_names(exposed_prefix, &declarations);
            }
            for declaration in &declarations {
                self.walk_declaration(
                    file,
                    source_scope,
                    declaration,
                    exposed_prefix,
                    depth,
                    chain.clone(),
                    via_glob,
                );
            }
        }
        if closes_cycle {
            self.cycle_closure_active = false;
        } else {
            self.active_namespaces.remove(&key);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_declaration(
        &mut self,
        file: &Path,
        source_scope: &[String],
        declaration: &Declaration,
        exposed_prefix: &[String],
        depth: usize,
        chain: Vec<ReExportHop>,
        via_glob: bool,
    ) {
        let usingnamespace = declaration.native_kind.as_deref() == Some("usingnamespace");
        if via_glob {
            if !is_declaration_public(declaration) {
                return;
            }
        } else if !self.is_visible(declaration) {
            return;
        }

        if !usingnamespace {
            let qualified = qualified_name(exposed_prefix, &declaration.name);
            if self.ambiguous_qualified.contains(&qualified) {
                return;
            }
            if via_glob {
                if self.direct_qualified.contains(&qualified) {
                    self.mark_ambiguous(&qualified);
                    return;
                }
                let identity = (
                    cache_key(file),
                    qualified_name(source_scope, &declaration.name),
                );
                if let Some(previous) = self.injected_qualified.get(&qualified) {
                    if previous == &identity {
                        return;
                    }
                    self.mark_ambiguous(&qualified);
                    return;
                }
                self.injected_qualified.insert(qualified.clone(), identity);
                if self.seen_qualified.contains(&qualified) {
                    // `--include-private` may have emitted an inaccessible
                    // direct declaration. The public injected name wins.
                    self.remove_qualified_prefix(&qualified);
                }
            } else if !is_declaration_public(declaration)
                && self.injected_qualified.contains_key(&qualified)
            {
                return;
            }
        }

        if usingnamespace {
            if !is_declaration_public(declaration) && !self.include_private {
                return;
            }
            if depth >= self.max_depth {
                return;
            }
            let Some(expression) = usingnamespace_expression(declaration) else {
                return;
            };
            let Some(resolved) = self.resolve_expression(file, source_scope, &expression) else {
                return;
            };
            let mut next_chain = chain;
            next_chain.push(re_export_hop(file, source_scope, declaration));
            next_chain.extend(resolved.hops);
            self.inject_target(&resolved.target, exposed_prefix, depth + 1, next_chain);
            return;
        }

        let alias_expression = declaration_alias_expression(declaration);
        if let Some(expression) = alias_expression {
            if depth < self.max_depth {
                if let Some(resolved) = self.resolve_expression(file, source_scope, &expression) {
                    let mut next_chain = chain.clone();
                    next_chain.push(re_export_hop(file, source_scope, declaration));
                    next_chain.extend(resolved.hops);
                    if resolved.target.path.is_empty() {
                        // The namespace value itself is a public constant, and
                        // its declarations are reachable below that constant.
                        self.emit(
                            exposed_prefix,
                            &declaration.name,
                            declaration,
                            file,
                            chain,
                            via_glob,
                        );
                        let mut nested_prefix = exposed_prefix.to_vec();
                        nested_prefix.push(declaration.name.clone());
                        self.walk_namespace(
                            &resolved.target.file,
                            &[],
                            &nested_prefix,
                            depth + 1,
                            next_chain,
                            via_glob,
                        );
                    } else {
                        self.emit_target(
                            &resolved.target,
                            exposed_prefix,
                            &declaration.name,
                            depth + 1,
                            next_chain,
                            via_glob,
                        );
                    }
                    return;
                }
            }
            // An external named module, dynamic expression, or a chain over
            // the depth limit is still part of the public API as a constant.
            self.emit(
                exposed_prefix,
                &declaration.name,
                declaration,
                file,
                chain,
                via_glob,
            );
            return;
        }

        self.emit(
            exposed_prefix,
            &declaration.name,
            declaration,
            file,
            chain.clone(),
            via_glob,
        );
        if !is_namespace_container(declaration.kind) {
            return;
        }

        let mut source_child_scope = source_scope.to_vec();
        source_child_scope.push(declaration.name.clone());
        let mut exposed_child_prefix = exposed_prefix.to_vec();
        exposed_child_prefix.push(declaration.name.clone());
        if !via_glob {
            self.reserve_direct_names(&exposed_child_prefix, &declaration.children);
        }
        for child in &declaration.children {
            self.walk_declaration(
                file,
                &source_child_scope,
                child,
                &exposed_child_prefix,
                depth,
                chain.clone(),
                via_glob,
            );
        }
    }

    fn inject_target(
        &mut self,
        target: &Target,
        exposed_prefix: &[String],
        depth: usize,
        chain: Vec<ReExportHop>,
    ) {
        if target.path.is_empty() {
            self.walk_namespace(&target.file, &[], exposed_prefix, depth, chain, true);
            return;
        }

        let Some(declaration) = self.declaration_at(target) else {
            return;
        };
        if !is_namespace_container(declaration.kind) {
            return;
        }
        for child in &declaration.children {
            self.walk_declaration(
                &target.file,
                &target.path,
                child,
                exposed_prefix,
                depth,
                chain.clone(),
                true,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_target(
        &mut self,
        target: &Target,
        exposed_prefix: &[String],
        exposed_name: &str,
        depth: usize,
        chain: Vec<ReExportHop>,
        via_glob: bool,
    ) {
        let Some(declaration) = self.declaration_at(target) else {
            return;
        };
        self.emit(
            exposed_prefix,
            exposed_name,
            &declaration,
            &target.file,
            chain.clone(),
            via_glob,
        );
        if !is_namespace_container(declaration.kind) || depth > self.max_depth {
            return;
        }

        let mut nested_prefix = exposed_prefix.to_vec();
        nested_prefix.push(exposed_name.to_string());
        if !via_glob {
            self.reserve_direct_names(&nested_prefix, &declaration.children);
        }
        for child in &declaration.children {
            self.walk_declaration(
                &target.file,
                &target.path,
                child,
                &nested_prefix,
                depth,
                chain.clone(),
                via_glob,
            );
        }
    }

    fn emit(
        &mut self,
        prefix: &[String],
        exposed_name: &str,
        declaration: &Declaration,
        source_file: &Path,
        chain: Vec<ReExportHop>,
        via_glob: bool,
    ) {
        if exposed_name.is_empty() {
            return;
        }
        let qualified_path = if prefix.is_empty() {
            exposed_name.to_string()
        } else {
            format!("{}.{}", prefix.join("."), exposed_name)
        };
        if !self.seen_qualified.insert(qualified_path.clone()) {
            return;
        }
        self.entries.push(SurfaceEntry {
            qualified_path,
            kind: declaration.kind,
            signature: declaration.signature.clone(),
            source_path: source_file.to_path_buf(),
            source_line: declaration.start_line,
            source_name: declaration.name.clone(),
            re_export_chain: chain,
            via_glob,
            docs: declaration.docs.clone(),
        });
    }

    fn reserve_direct_names(&mut self, prefix: &[String], declarations: &[Declaration]) {
        self.direct_qualified.extend(
            declarations
                .iter()
                .filter(|declaration| {
                    declaration.native_kind.as_deref() != Some("usingnamespace")
                        && !declaration.name.is_empty()
                        && is_declaration_public(declaration)
                })
                .map(|declaration| qualified_name(prefix, &declaration.name)),
        );
    }

    fn mark_ambiguous(&mut self, qualified: &str) {
        self.ambiguous_qualified.insert(qualified.to_string());
        self.remove_qualified_prefix(qualified);
    }

    fn remove_qualified_prefix(&mut self, qualified: &str) {
        let descendant_prefix = format!("{qualified}.");
        self.entries.retain(|entry| {
            entry.qualified_path != qualified
                && !entry.qualified_path.starts_with(&descendant_prefix)
        });
        self.seen_qualified
            .retain(|name| name != qualified && !name.starts_with(&descendant_prefix));
        self.injected_qualified
            .retain(|name, _| name != qualified && !name.starts_with(&descendant_prefix));
    }

    fn is_visible(&self, declaration: &Declaration) -> bool {
        self.include_private
            || declaration.visibility != "private"
            || declaration
                .modifiers
                .iter()
                .any(|modifier| modifier == "export")
    }

    fn resolve_expression(
        &mut self,
        file: &Path,
        source_scope: &[String],
        expression: &AliasExpression,
    ) -> Option<ResolvedValue> {
        let mut alias_stack = HashSet::new();
        let mut hops = Vec::new();
        let target = self.evaluate_expression(
            file,
            source_scope,
            expression,
            0,
            &mut alias_stack,
            &mut hops,
        )?;
        Some(ResolvedValue { target, hops })
    }

    fn evaluate_expression(
        &mut self,
        file: &Path,
        source_scope: &[String],
        expression: &AliasExpression,
        alias_depth: usize,
        alias_stack: &mut HashSet<(PathBuf, String)>,
        hops: &mut Vec<ReExportHop>,
    ) -> Option<Target> {
        if alias_depth > self.max_depth {
            return None;
        }
        let mut target = match &expression.root {
            AliasRoot::Import(specifier) => Target {
                file: resolve_relative_import(file, specifier)?,
                path: Vec::new(),
            },
            AliasRoot::This => Target {
                file: file.to_path_buf(),
                path: source_scope.to_vec(),
            },
            AliasRoot::Identifier(name) => self.lookup_lexical(file, source_scope, name)?,
        };
        target = self.dereference_alias(target, alias_depth, alias_stack, hops)?;

        for member in &expression.members {
            let require_public = cache_key(&target.file) != cache_key(file);
            let candidate = self.lookup_namespace_member(
                &target,
                member,
                alias_depth.saturating_add(1),
                alias_stack,
                hops,
                require_public,
            )?;
            target = self.dereference_alias(
                candidate,
                alias_depth.saturating_add(1),
                alias_stack,
                hops,
            )?;
        }
        Some(target)
    }

    fn dereference_alias(
        &mut self,
        target: Target,
        alias_depth: usize,
        alias_stack: &mut HashSet<(PathBuf, String)>,
        hops: &mut Vec<ReExportHop>,
    ) -> Option<Target> {
        if target.path.is_empty() {
            return Some(target);
        }
        if alias_depth > self.max_depth {
            return None;
        }
        let declaration = self.declaration_at(&target)?;
        let Some(expression) = declaration_alias_expression(&declaration) else {
            return Some(target);
        };

        let key = (cache_key(&target.file), target.path.join("."));
        if !alias_stack.insert(key.clone()) {
            return None;
        }
        let parent_scope = &target.path[..target.path.len() - 1];
        hops.push(re_export_hop(&target.file, parent_scope, &declaration));
        let resolved = self.evaluate_expression(
            &target.file,
            parent_scope,
            &expression,
            alias_depth.saturating_add(1),
            alias_stack,
            hops,
        );
        alias_stack.remove(&key);
        resolved
    }

    fn lookup_lexical(
        &mut self,
        file: &Path,
        source_scope: &[String],
        name: &str,
    ) -> Option<Target> {
        for scope_len in (0..=source_scope.len()).rev() {
            let scope = &source_scope[..scope_len];
            if self.lookup_declaration(file, scope, name).is_some() {
                let mut path = scope.to_vec();
                path.push(name.to_string());
                return Some(Target {
                    file: file.to_path_buf(),
                    path,
                });
            }
        }
        None
    }

    fn lookup_namespace_member(
        &mut self,
        namespace: &Target,
        name: &str,
        alias_depth: usize,
        alias_stack: &mut HashSet<(PathBuf, String)>,
        hops: &mut Vec<ReExportHop>,
        require_public: bool,
    ) -> Option<Target> {
        if alias_depth > self.max_depth {
            return None;
        }
        if !namespace.path.is_empty() {
            let declaration = self.declaration_at(namespace)?;
            if !is_namespace_container(declaration.kind) {
                return None;
            }
        }

        let checkpoint = hops.len();
        let mut candidates = Vec::<(Target, Vec<ReExportHop>)>::new();
        if let Some(declaration) = self.lookup_declaration(&namespace.file, &namespace.path, name) {
            if !require_public || is_declaration_public(&declaration) {
                let mut path = namespace.path.clone();
                path.push(name.to_string());
                candidates.push((
                    Target {
                        file: namespace.file.clone(),
                        path,
                    },
                    Vec::new(),
                ));
            }
        }

        let declarations = self.declarations_in_scope(&namespace.file, &namespace.path)?;
        for declaration in declarations.iter().filter(|declaration| {
            declaration.native_kind.as_deref() == Some("usingnamespace")
                && (!require_public || is_declaration_public(declaration))
        }) {
            let guard = (
                cache_key(&namespace.file),
                format!(
                    "{}::usingnamespace@{}",
                    namespace.path.join("."),
                    declaration.start_line
                ),
            );
            if !alias_stack.insert(guard.clone()) {
                continue;
            }
            hops.truncate(checkpoint);
            hops.push(re_export_hop(&namespace.file, &namespace.path, declaration));
            let resolved = usingnamespace_expression(declaration).and_then(|expression| {
                self.evaluate_expression(
                    &namespace.file,
                    &namespace.path,
                    &expression,
                    alias_depth.saturating_add(1),
                    alias_stack,
                    hops,
                )
            });
            let target = resolved.and_then(|injected| {
                self.lookup_namespace_member(
                    &injected,
                    name,
                    alias_depth.saturating_add(1),
                    alias_stack,
                    hops,
                    true,
                )
            });
            alias_stack.remove(&guard);
            if let Some(target) = target {
                candidates.push((target, hops[checkpoint..].to_vec()));
            }
        }
        hops.truncate(checkpoint);

        let mut unique = Vec::<(Target, Vec<ReExportHop>)>::new();
        for candidate in candidates {
            if !unique.iter().any(|(target, _)| target == &candidate.0) {
                unique.push(candidate);
            }
        }
        if unique.len() != 1 {
            return None;
        }
        let (target, trail) = unique.pop()?;
        hops.extend(trail);
        Some(target)
    }

    fn lookup_declaration(
        &mut self,
        file: &Path,
        source_scope: &[String],
        name: &str,
    ) -> Option<Declaration> {
        self.declarations_in_scope(file, source_scope)?
            .into_iter()
            .find(|declaration| {
                declaration.native_kind.as_deref() != Some("usingnamespace")
                    && declaration.name == name
            })
    }

    fn declaration_at(&mut self, target: &Target) -> Option<Declaration> {
        let (name, parent) = target.path.split_last()?;
        self.lookup_declaration(&target.file, parent, name)
    }

    fn declarations_in_scope(
        &mut self,
        file: &Path,
        source_scope: &[String],
    ) -> Option<Vec<Declaration>> {
        let snapshot = self.snapshot(file)?;
        if source_scope.is_empty() {
            return Some(snapshot.declarations);
        }
        let declaration = declaration_by_path(&snapshot.declarations, source_scope)?;
        Some(declaration.children.clone())
    }

    fn snapshot(&mut self, file: &Path) -> Option<FileSnapshot> {
        let key = cache_key(file);
        if !self.loaded.contains_key(&key) {
            let parsed = parse_file(file)?;
            if parsed.language != "zig" {
                return None;
            }
            self.loaded.insert(
                key.clone(),
                FileSnapshot {
                    declarations: parsed.declarations,
                },
            );
        }
        self.loaded.get(&key).cloned()
    }
}

fn declaration_by_path<'a>(
    declarations: &'a [Declaration],
    path: &[String],
) -> Option<&'a Declaration> {
    let (name, tail) = path.split_first()?;
    let declaration = declarations
        .iter()
        .find(|declaration| &declaration.name == name)?;
    if tail.is_empty() {
        Some(declaration)
    } else {
        declaration_by_path(&declaration.children, tail)
    }
}

fn is_namespace_container(kind: DeclarationKind) -> bool {
    matches!(
        kind,
        DeclarationKind::Namespace
            | DeclarationKind::Class
            | DeclarationKind::Struct
            | DeclarationKind::Interface
            | DeclarationKind::Record
            | DeclarationKind::Enum
    )
}

fn is_declaration_public(declaration: &Declaration) -> bool {
    declaration.visibility != "private"
        || declaration
            .modifiers
            .iter()
            .any(|modifier| modifier == "export")
}

fn qualified_name(prefix: &[String], name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", prefix.join("."), name)
    }
}

fn cache_key(file: &Path) -> PathBuf {
    std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf())
}

fn resolve_relative_import(from_file: &Path, specifier: &str) -> Option<PathBuf> {
    // `@import("std")`, generated modules, and names installed by build.zig
    // cannot be mapped to a source file without evaluating the build graph.
    if !specifier.ends_with(".zig") {
        return None;
    }
    let target = from_file.parent()?.join(specifier);
    if !target.is_file() {
        return None;
    }
    Some(std::fs::canonicalize(&target).unwrap_or(target))
}

fn re_export_hop(file: &Path, source_scope: &[String], declaration: &Declaration) -> ReExportHop {
    let module_path = if source_scope.is_empty() {
        file.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string()
    } else {
        source_scope.join(".")
    };
    ReExportHop {
        file: file.to_path_buf(),
        line: declaration.start_line,
        module_path,
        statement: declaration.signature.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AliasExpression {
    root: AliasRoot,
    members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AliasRoot {
    Import(String),
    This,
    Identifier(String),
}

fn declaration_alias_expression(declaration: &Declaration) -> Option<AliasExpression> {
    if declaration.native_kind.as_deref() != Some("const") {
        return None;
    }
    let rhs = assignment_rhs(&declaration.signature)?;
    parse_alias_expression(rhs)
}

fn usingnamespace_expression(declaration: &Declaration) -> Option<AliasExpression> {
    let mut statement = declaration.signature.trim();
    if let Some(rest) = statement.strip_prefix("pub ") {
        statement = rest.trim_start();
    }
    let expression = statement.strip_prefix("usingnamespace")?.trim();
    parse_alias_expression(expression.trim_end_matches(';').trim())
}

fn assignment_rhs(signature: &str) -> Option<&str> {
    let bytes = signature.as_bytes();
    let mut parens = 0usize;
    let mut brackets = 0usize;
    let mut braces = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, &byte) in bytes.iter().enumerate() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'(' => parens += 1,
            b')' => parens = parens.saturating_sub(1),
            b'[' => brackets += 1,
            b']' => brackets = brackets.saturating_sub(1),
            b'{' => braces += 1,
            b'}' => braces = braces.saturating_sub(1),
            b'=' if parens == 0 && brackets == 0 && braces == 0 => {
                let before = index.checked_sub(1).and_then(|i| bytes.get(i)).copied();
                let after = bytes.get(index + 1).copied();
                if !matches!(before, Some(b'=' | b'!' | b'<' | b'>'))
                    && !matches!(after, Some(b'=' | b'>'))
                {
                    return Some(signature[index + 1..].trim().trim_end_matches(';').trim());
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_alias_expression(expression: &str) -> Option<AliasExpression> {
    let bytes = expression.as_bytes();
    let mut position = skip_ascii_space(bytes, 0);
    let root = if bytes.get(position..)?.starts_with(b"@import") {
        position += "@import".len();
        position = skip_ascii_space(bytes, position);
        if bytes.get(position) != Some(&b'(') {
            return None;
        }
        position = skip_ascii_space(bytes, position + 1);
        let (specifier, next) = parse_quoted_string(expression, position)?;
        position = skip_ascii_space(bytes, next);
        if bytes.get(position) != Some(&b')') {
            return None;
        }
        position += 1;
        AliasRoot::Import(specifier)
    } else if bytes.get(position..)?.starts_with(b"@This") {
        position += "@This".len();
        position = skip_ascii_space(bytes, position);
        if bytes.get(position) != Some(&b'(') {
            return None;
        }
        position = skip_ascii_space(bytes, position + 1);
        if bytes.get(position) != Some(&b')') {
            return None;
        }
        position += 1;
        AliasRoot::This
    } else {
        let (identifier, next) = parse_identifier(expression, position)?;
        position = next;
        AliasRoot::Identifier(identifier)
    };

    let mut members = Vec::new();
    loop {
        position = skip_ascii_space(bytes, position);
        if position == bytes.len() {
            break;
        }
        if bytes.get(position) != Some(&b'.') {
            return None;
        }
        position = skip_ascii_space(bytes, position + 1);
        let (member, next) = parse_identifier(expression, position)?;
        members.push(member);
        position = next;
    }
    Some(AliasExpression { root, members })
}

fn parse_identifier(input: &str, position: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    if bytes.get(position..)?.starts_with(b"@\"") {
        let (_, end) = parse_quoted_string(input, position + 1)?;
        return Some((input[position..end].to_string(), end));
    }
    let first = *bytes.get(position)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = position + 1;
    while bytes
        .get(end)
        .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
    {
        end += 1;
    }
    Some((input[position..end].to_string(), end))
}

fn parse_quoted_string(input: &str, position: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    if bytes.get(position) != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    let mut end = position + 1;
    while let Some(&byte) = bytes.get(end) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some((input[position + 1..end].to_string(), end + 1));
        }
        end += 1;
    }
    None
}

fn skip_ascii_space(bytes: &[u8], mut position: usize) -> usize {
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    position
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_import_and_facade_alias_expressions() {
        assert_eq!(
            parse_alias_expression("@import(\"./inner.zig\").Outer.Inner"),
            Some(AliasExpression {
                root: AliasRoot::Import("./inner.zig".into()),
                members: vec!["Outer".into(), "Inner".into()],
            })
        );
        assert_eq!(
            parse_alias_expression("private_facade.Outer.Inner"),
            Some(AliasExpression {
                root: AliasRoot::Identifier("private_facade".into()),
                members: vec!["Outer".into(), "Inner".into()],
            })
        );
        assert_eq!(
            parse_alias_expression("@This().Outer.Inner"),
            Some(AliasExpression {
                root: AliasRoot::This,
                members: vec!["Outer".into(), "Inner".into()],
            })
        );
        assert!(parse_alias_expression("if (enabled) A else B").is_none());
    }

    #[test]
    fn finds_assignment_after_complex_type_annotation() {
        assert_eq!(
            assignment_rhs("pub const Alias: *const fn (u8) callconv(.c) type = module.Type"),
            Some("module.Type")
        );
        assert_eq!(
            assignment_rhs("pub const Plain = @import(\"x.zig\")"),
            Some("@import(\"x.zig\")")
        );
    }
}
