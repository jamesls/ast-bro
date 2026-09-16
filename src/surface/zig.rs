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

struct FileSnapshot {
    declarations: Vec<Declaration>,
    aliases: HashMap<usize, AliasExpression>,
    returned_containers: HashMap<usize, (usize, usize)>,
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
            path: target
                .path
                .into_iter()
                .map(|name| {
                    if name.starts_with("@\"") {
                        crate::zig_syntax::identifier(&name)
                    } else {
                        name
                    }
                })
                .collect(),
        })
    }

    /// Return every statically named conditional branch, without selecting a
    /// build configuration. Private names are allowed only in the source file.
    pub fn candidates(
        &mut self,
        file: &Path,
        path: &[String],
        local: bool,
    ) -> Option<Vec<PublicPath>> {
        let mut targets = vec![Target {
            file: file.to_path_buf(),
            path: Vec::new(),
        }];
        for member in path {
            let mut next = Vec::new();
            for target in targets {
                let mut stack = HashSet::new();
                let mut hops = Vec::new();
                let public = !local || cache_key(&target.file) != cache_key(file);
                if let Some(target) = self
                    .walker
                    .lookup_namespace_member(&target, member, 0, &mut stack, &mut hops, public)
                {
                    next.extend(self.walker.alias_candidates(target, 0, &mut stack)?);
                } else {
                    return None;
                }
            }
            next.sort_by(|a, b| (&a.file, &a.path).cmp(&(&b.file, &b.path)));
            next.dedup();
            targets = next;
        }
        Some(
            targets
                .into_iter()
                .map(|target| PublicPath {
                    file: target.file,
                    path: target
                        .path
                        .into_iter()
                        .map(|name| {
                            if name.starts_with("@\"") {
                                crate::zig_syntax::identifier(&name)
                            } else {
                                name
                            }
                        })
                        .collect(),
                })
                .collect(),
        )
    }
}

struct Walker {
    conditional_depth: usize,
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
            conditional_depth: 0,
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

        let alias_expression = self.alias_expression(file, declaration);
        if let Some(expression) = alias_expression {
            let conditional_choices = match &expression.root {
                AliasRoot::Choices(choices) if depth < self.max_depth => Some(choices),
                _ => None,
            };
            if let Some(choices) = conditional_choices {
                self.conditional_depth += 1;
                let mut exposed = false;
                for choice in choices {
                    if let Some(resolved) = self.resolve_expression(file, source_scope, choice) {
                        let mut next_chain = chain.clone();
                        next_chain.push(re_export_hop(file, source_scope, declaration));
                        next_chain.extend(resolved.hops);
                        // Distinct conditional definitions intentionally share
                        // their public name; their source and chain identify
                        // each candidate. Do not silently choose one branch.
                        let qualified = qualified_name(exposed_prefix, &declaration.name);
                        self.seen_qualified.retain(|name| {
                            name != &qualified && !name.starts_with(&format!("{qualified}."))
                        });
                        if resolved.target.path.is_empty() {
                            self.emit(
                                exposed_prefix,
                                &declaration.name,
                                declaration,
                                file,
                                next_chain.clone(),
                                via_glob,
                            );
                            let mut prefix = exposed_prefix.to_vec();
                            prefix.push(declaration.name.clone());
                            self.walk_namespace(
                                &resolved.target.file,
                                &[],
                                &prefix,
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
                        exposed = true;
                    }
                }
                self.conditional_depth -= 1;
                if exposed {
                    return;
                }
            }
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
            conditional: self.conditional_depth > 0,
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
            AliasRoot::Choices(choices) => {
                let mut candidates = Vec::new();
                for choice in choices {
                    candidates.push(self.evaluate_expression(
                        file,
                        source_scope,
                        choice,
                        alias_depth + 1,
                        alias_stack,
                        hops,
                    )?);
                }
                let first = candidates.first()?.clone();
                if candidates.iter().any(|candidate| candidate != &first) {
                    return None;
                }
                first
            }
            AliasRoot::Apply(function) => {
                let mut target = self.evaluate_expression(
                    file,
                    source_scope,
                    function,
                    alias_depth + 1,
                    alias_stack,
                    hops,
                )?;
                let declaration = self.declaration_at(&target)?;
                if !matches!(
                    declaration.kind,
                    DeclarationKind::Function | DeclarationKind::Method
                ) || !declaration.signature.trim_end().ends_with(" type")
                {
                    return None;
                }
                let range = *self
                    .snapshot(&target.file)?
                    .returned_containers
                    .get(&declaration.start_byte)?;
                let container = declaration
                    .children
                    .iter()
                    .find(|child| child.start_byte == range.0 && child.end_byte == range.1)?;
                target.path.push(container.name.clone());
                target
            }
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
        let Some(expression) = self.alias_expression(&target.file, &declaration) else {
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

    fn alias_candidates(
        &mut self,
        target: Target,
        depth: usize,
        stack: &mut HashSet<(PathBuf, String)>,
    ) -> Option<Vec<Target>> {
        if depth > self.max_depth {
            return None;
        }
        let Some(declaration) = self.declaration_at(&target) else {
            return Some(vec![target]);
        };
        let Some(expression) = self.alias_expression(&target.file, &declaration) else {
            return Some(vec![target]);
        };
        let key = (cache_key(&target.file), target.path.join("."));
        if !stack.insert(key.clone()) {
            return None;
        }
        let scope = &target.path[..target.path.len() - 1];
        let choices = if let AliasRoot::Choices(choices) = expression.root.clone() {
            choices
        } else {
            vec![expression]
        };
        let mut result = Vec::new();
        for choice in choices {
            if let Some(resolved) = self.evaluate_expression(
                &target.file,
                scope,
                &choice,
                depth + 1,
                stack,
                &mut Vec::new(),
            ) {
                result.push(resolved);
            } else {
                stack.remove(&key);
                return None;
            }
        }
        stack.remove(&key);
        Some(result)
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
                    && crate::zig_syntax::identifier(&declaration.name)
                        == crate::zig_syntax::identifier(name)
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
            return Some(snapshot.declarations.clone());
        }
        let declaration = declaration_by_path(&snapshot.declarations, source_scope)?;
        Some(declaration.children.clone())
    }

    fn alias_expression(
        &mut self,
        file: &Path,
        declaration: &Declaration,
    ) -> Option<AliasExpression> {
        self.snapshot(file)?
            .aliases
            .get(&declaration.start_byte)
            .cloned()
    }

    fn snapshot(&mut self, file: &Path) -> Option<&FileSnapshot> {
        let key = cache_key(file);
        if !self.loaded.contains_key(&key) {
            let parsed = parse_file(file)?;
            if parsed.language != "zig" {
                return None;
            }
            let tree = crate::zig_syntax::parse(&parsed.source);
            let source = std::str::from_utf8(&parsed.source).ok()?;
            let mut aliases = HashMap::new();
            let mut returned_containers = HashMap::new();
            let mut pending = vec![tree.root_node()];
            while let Some(node) = pending.pop() {
                if node.kind() == "variable_declaration" {
                    let mut cursor = node.walk();
                    let children: Vec<_> = node.children(&mut cursor).collect();
                    if children.iter().any(|node| node.kind() == "const") {
                        if let Some(equals) = children.iter().position(|node| node.kind() == "=") {
                            if let Some(initializer) = children[equals + 1..]
                                .iter()
                                .find(|node| node.is_named() && node.kind() != "comment")
                            {
                                if let Some(alias) = alias_from_node(*initializer, source) {
                                    aliases.insert(node.start_byte(), alias);
                                }
                            }
                        }
                    }
                }
                if node.kind() == "function_declaration" {
                    if let Some(body) = node.child_by_field_name("body") {
                        if let [container] = crate::zig_syntax::returned_containers(body).as_slice()
                        {
                            returned_containers.insert(
                                node.start_byte(),
                                (container.start_byte(), container.end_byte()),
                            );
                        }
                    }
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            self.loaded.insert(
                key.clone(),
                FileSnapshot {
                    declarations: parsed.declarations,
                    aliases,
                    returned_containers,
                },
            );
        }
        self.loaded.get(&key)
    }
}

fn declaration_by_path<'a>(
    declarations: &'a [Declaration],
    path: &[String],
) -> Option<&'a Declaration> {
    let (name, tail) = path.split_first()?;
    let declaration = declarations.iter().find(|declaration| {
        crate::zig_syntax::identifier(&declaration.name) == crate::zig_syntax::identifier(name)
    })?;
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
    // Named modules resolve only when literal build wiring identifies a file.
    if !specifier.ends_with(".zig") {
        return crate::zig_syntax::build::resolve_named_import(from_file, specifier);
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
    Apply(Box<AliasExpression>),
    Choices(Vec<AliasExpression>),
}

fn usingnamespace_expression(declaration: &Declaration) -> Option<AliasExpression> {
    let mut statement = declaration.signature.trim();
    if let Some(rest) = statement.strip_prefix("pub ") {
        statement = rest.trim_start();
    }
    let expression = statement.strip_prefix("usingnamespace")?.trim();
    parse_alias_expression(expression.trim_end_matches(';').trim())
}

#[cfg(test)]
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
    let source = format!("const alias = {expression};");
    let tree = crate::zig_syntax::parse(source.as_bytes());
    if tree.root_node().has_error() {
        return None;
    }
    let declaration = tree.root_node().named_child(0)?;
    let mut cursor = declaration.walk();
    let value = declaration.named_children(&mut cursor).last()?;
    alias_from_node(value, &source)
}

fn alias_from_node(node: tree_sitter::Node<'_>, source: &str) -> Option<AliasExpression> {
    let text = |node: tree_sitter::Node<'_>| node.utf8_text(source.as_bytes()).ok();
    let root = match node.kind() {
        "identifier" => AliasRoot::Identifier(crate::zig_syntax::identifier(text(node)?)),
        "parenthesized_expression" => {
            return alias_from_node(crate::zig_syntax::first_expression(node)?, source)
        }
        "field_expression" => {
            let mut alias = alias_from_node(node.child_by_field_name("object")?, source)?;
            let member = crate::zig_syntax::identifier(text(node.child_by_field_name("member")?)?);
            if let AliasRoot::Choices(choices) = &mut alias.root {
                for choice in choices {
                    choice.members.push(member.clone());
                }
            } else {
                alias.members.push(member);
            }
            return Some(alias);
        }
        "builtin_function" => match text(node.named_child(0)?)? {
            "@This" => AliasRoot::This,
            "@import" => {
                let mut cursor = node.walk();
                let args = node
                    .named_children(&mut cursor)
                    .find(|n| n.kind() == "arguments")?;
                AliasRoot::Import(crate::zig_syntax::string_value(text(
                    crate::zig_syntax::first_expression(args)?,
                )?)?)
            }
            _ => return None,
        },
        "call_expression" => AliasRoot::Apply(Box::new(alias_from_node(
            node.child_by_field_name("function")?,
            source,
        )?)),
        "if_expression" | "if_type_expression" => {
            let mut cursor = node.walk();
            let children: Vec<_> = node
                .named_children(&mut cursor)
                .filter(|child| !matches!(child.kind(), "comment" | "payload"))
                .collect();
            let [condition, yes, no] = children.as_slice() else {
                return None;
            };
            if text(*condition)? == "true" {
                return alias_from_node(*yes, source);
            }
            if text(*condition)? == "false" {
                return alias_from_node(*no, source);
            }
            AliasRoot::Choices(vec![
                alias_from_node(*yes, source)?,
                alias_from_node(*no, source)?,
            ])
        }
        "switch_expression" => {
            let mut cursor = node.walk();
            let choices: Option<Vec<_>> = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "switch_case")
                .map(|case| {
                    let mut cursor = case.walk();
                    let value = case.named_children(&mut cursor).last()?;
                    alias_from_node(value, source)
                })
                .collect();
            let choices = choices?;
            if choices.is_empty() {
                return None;
            }
            AliasRoot::Choices(choices)
        }
        _ => return None,
    };
    Some(AliasExpression {
        root,
        members: Vec::new(),
    })
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
        assert!(matches!(
            parse_alias_expression("if (enabled) A else B")
                .unwrap()
                .root,
            AliasRoot::Choices(_)
        ));
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
