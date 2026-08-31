//! Zig declarations parsed directly with tree-sitter.
//!
//! Zig is not available through ast-grep's `SupportLang`, so this adapter
//! walks `tree_sitter::Node`s and produces the same declaration IR as the
//! ast-grep-backed adapters.

use super::base::collapse_ws;
use crate::core::{CallKind, CallSite, Declaration, DeclarationKind, ImportBinding, ParseResult};
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::Node;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    Root,
    Type,
    Enum,
}

/// Parses Zig source into the shared declaration representation.
pub fn parse_zig(path: &Path, source: &[u8]) -> ParseResult {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_zig::LANGUAGE.into())
        .expect("the bundled Zig grammar must load");
    let tree = parser
        .parse(source, None)
        .expect("an uncancelled tree-sitter parse must produce a tree");
    let root = tree.root_node();
    let mut declarations = Vec::new();
    walk_container(root, source, ContainerKind::Root, &mut declarations);
    let imports = extract_import_bindings(root, source);

    ParseResult {
        path: path.to_path_buf(),
        language: "zig",
        source: source.to_vec(),
        line_count: source.iter().filter(|&&byte| byte == b'\n').count() + 1,
        declarations,
        error_count: count_parse_errors(root),
        imports,
    }
}

fn walk_container(
    node: Node<'_>,
    source: &[u8],
    container_kind: ContainerKind,
    declarations: &mut Vec<Declaration>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let declaration = match child.kind() {
            "variable_declaration" => variable_to_decl(child, source),
            "function_declaration" => {
                function_to_decl(child, source, container_kind != ContainerKind::Root)
            }
            "container_field" => container_field_to_decl(child, source, container_kind),
            "test_declaration" => test_to_decl(child, source),
            "comptime_declaration" => comptime_to_decl(child, source),
            "using_namespace_declaration" => usingnamespace_to_decl(child, source),
            _ => None,
        };
        if let Some(declaration) = declaration {
            declarations.push(declaration);
        }
    }
}

fn variable_to_decl(node: Node<'_>, source: &[u8]) -> Option<Declaration> {
    let name_node = first_named_child_of_kind(node, "identifier")?;
    let name = node_text(name_node, source).trim().to_string();
    if name.is_empty() {
        return None;
    }

    if let Some(container) = initializer_container(node) {
        let (kind, native_kind, child_kind) = container_shape(container.kind())?;

        let children = if container.kind() == "error_set_declaration" {
            error_members(container, source)
        } else {
            let mut children = Vec::new();
            walk_container(container, source, child_kind, &mut children);
            children
        };

        let mut declaration = make_declaration(
            node,
            source,
            kind,
            name,
            container_signature(node, container, source),
            Some(native_kind.to_string()),
            children,
        );
        extend_unique(
            &mut declaration.modifiers,
            direct_modifiers(container, source),
        );
        apply_container_docs(&mut declaration, container, source);
        return Some(declaration);
    }

    let native_kind = declaration_keyword(node, source)?;
    let children = if let Some(container) = type_container(node) {
        container_children(container, source)
    } else {
        let mut children = Vec::new();
        if let Some(initializer) = initializer_after_equals(node) {
            collect_initializer_containers(initializer, source, true, &mut children);
        }
        children
    };
    Some(make_declaration(
        node,
        source,
        DeclarationKind::Field,
        name,
        collapsed_node_text(node, source, &[',', ';']),
        Some(native_kind.to_string()),
        children,
    ))
}

fn function_to_decl(node: Node<'_>, source: &[u8], is_method: bool) -> Option<Declaration> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source).trim().to_string();
    if name.is_empty() {
        return None;
    }

    let signature_end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    let signature = collapse_ws(&String::from_utf8_lossy(
        &source[node.start_byte()..signature_end],
    ))
    .trim_end_matches(';')
    .trim()
    .to_string();

    let mut declaration = make_declaration(
        node,
        source,
        if is_method {
            DeclarationKind::Method
        } else {
            DeclarationKind::Function
        },
        name,
        signature,
        None,
        node.child_by_field_name("body")
            .map_or_else(Vec::new, |body| extract_local_declarations(body, source)),
    );
    declaration.calls = node
        .child_by_field_name("body")
        .map_or_else(Vec::new, |body| extract_call_sites(body, source));
    Some(declaration)
}

fn container_field_to_decl(
    node: Node<'_>,
    source: &[u8],
    container_kind: ContainerKind,
) -> Option<Declaration> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source).trim().to_string();
    if name.is_empty() || node.is_missing() || name_node.is_missing() {
        return None;
    }

    let kind =
        if container_kind == ContainerKind::Enum && node.child_by_field_name("type").is_none() {
            DeclarationKind::EnumMember
        } else {
            DeclarationKind::Field
        };
    let children = type_container(node)
        .map(|container| container_children(container, source))
        .unwrap_or_default();
    let mut declaration = make_declaration(
        node,
        source,
        kind,
        name,
        collapsed_node_text(node, source, &[',']),
        None,
        children,
    );
    // Zig has no field-level visibility syntax. Container fields and enum
    // tags inherit their container's reach, represented by the shared IR's
    // empty visibility rather than the explicit `private` marker.
    declaration.visibility = String::new();
    Some(declaration)
}

fn test_to_decl(node: Node<'_>, source: &[u8]) -> Option<Declaration> {
    let mut cursor = node.walk();
    let name_node = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "string" | "identifier"));
    let name = name_node
        .map(|child| {
            let text = node_text(child, source).trim();
            if child.kind() == "string" {
                text.strip_prefix('"')
                    .and_then(|text| text.strip_suffix('"'))
                    .unwrap_or(text)
                    .to_string()
            } else {
                text.to_string()
            }
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("test@{}", node.start_position().row + 1));
    let body = first_named_child_of_kind(node, "block")?;
    let signature = collapse_ws(&String::from_utf8_lossy(
        &source[node.start_byte()..body.start_byte()],
    ));

    let mut declaration = make_declaration(
        node,
        source,
        DeclarationKind::Function,
        name,
        signature,
        Some("test".to_string()),
        extract_local_declarations(body, source),
    );
    declaration.calls = extract_call_sites(body, source);
    Some(declaration)
}

fn extract_local_declarations(body: Node<'_>, source: &[u8]) -> Vec<Declaration> {
    let mut declarations = Vec::new();
    walk_local_declarations(body, source, &mut declarations);
    declarations
}

fn walk_local_declarations(node: Node<'_>, source: &[u8], declarations: &mut Vec<Declaration>) {
    match node.kind() {
        "variable_declaration" => {
            if let Some(declaration) = variable_to_decl(node, source) {
                if initializer_container(node).is_some() || !declaration.children.is_empty() {
                    declarations.push(declaration);
                    return;
                }
            }
        }
        "comptime_statement" => {
            if let Some(declaration) = comptime_to_decl(node, source) {
                declarations.push(declaration);
            }
            return;
        }
        "function_declaration" | "test_declaration" | "comptime_declaration" => return,
        kind if is_container_kind(kind) => {
            if let Some(declaration) = anonymous_container_to_decl(node, source) {
                declarations.push(declaration);
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_local_declarations(child, source, declarations);
    }
}

fn anonymous_container_to_decl(node: Node<'_>, source: &[u8]) -> Option<Declaration> {
    let (kind, native_kind, child_kind) = container_shape(node.kind())?;
    let children = if node.kind() == "error_set_declaration" {
        error_members(node, source)
    } else {
        let mut children = Vec::new();
        walk_container(node, source, child_kind, &mut children);
        children
    };
    if children.is_empty() {
        return None;
    }

    let line = node.start_position().row + 1;
    let column = node.start_position().column + 1;
    let name = format!("anonymous_{native_kind}@L{line}C{column}").replace(' ', "_");
    let mut declaration = make_declaration(
        node,
        source,
        kind,
        name,
        bare_container_signature(node, source),
        Some(native_kind.to_string()),
        children,
    );
    extend_unique(&mut declaration.modifiers, direct_modifiers(node, source));
    apply_container_docs(&mut declaration, node, source);
    Some(declaration)
}

fn collect_initializer_containers(
    node: Node<'_>,
    source: &[u8],
    flatten_error_sets: bool,
    declarations: &mut Vec<Declaration>,
) {
    if is_container_kind(node.kind()) {
        if node.kind() == "error_set_declaration" && flatten_error_sets {
            declarations.extend(error_members(node, source));
        } else if let Some(declaration) = anonymous_container_to_decl(node, source) {
            declarations.push(declaration);
        }
        return;
    }
    if matches!(
        node.kind(),
        "function_declaration" | "test_declaration" | "comptime_declaration"
    ) {
        return;
    }

    let flatten_children = flatten_error_sets
        && !matches!(
            node.kind(),
            "array_initializer"
                | "struct_initializer"
                | "anonymous_struct_initializer"
                | "initializer_list"
        );
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_initializer_containers(child, source, flatten_children, declarations);
    }
}

fn comptime_to_decl(node: Node<'_>, source: &[u8]) -> Option<Declaration> {
    let mut cursor = node.walk();
    let body = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "block" | "expression_statement"))?;
    let line = node.start_position().row + 1;
    let column = node.start_position().column + 1;
    let name = format!("comptime@L{line}C{column}");
    let signature = if body.kind() == "block" {
        collapse_ws(&String::from_utf8_lossy(
            &source[node.start_byte()..body.start_byte()],
        ))
    } else {
        collapsed_node_text(node, source, &[';'])
    };

    let mut declaration = make_declaration(
        node,
        source,
        DeclarationKind::Function,
        name,
        signature,
        Some("comptime".to_string()),
        extract_local_declarations(body, source),
    );
    declaration.calls = extract_call_sites(body, source);
    Some(declaration)
}

fn usingnamespace_to_decl(node: Node<'_>, source: &[u8]) -> Option<Declaration> {
    let mut cursor = node.walk();
    let target = node.named_children(&mut cursor).next()?;
    let name = collapse_ws(node_text(target, source)).trim().to_string();
    if name.is_empty() {
        return None;
    }

    Some(make_declaration(
        node,
        source,
        DeclarationKind::Field,
        name,
        collapsed_node_text(node, source, &[';']),
        Some("usingnamespace".to_string()),
        Vec::new(),
    ))
}

fn extract_call_sites(body: Node<'_>, source: &[u8]) -> Vec<CallSite> {
    let mut calls = Vec::new();
    walk_calls_in_body(body, source, &mut calls);
    rewrite_typed_receivers(body, source, &mut calls);
    rewrite_local_import_calls(body, source, &mut calls);
    calls
}

#[derive(Clone)]
struct ScopedType {
    local: String,
    type_name: String,
    line: u32,
    end_line: u32,
}

fn rewrite_typed_receivers(body: Node<'_>, source: &[u8], calls: &mut [CallSite]) {
    let types = local_types(body, source);
    for call in calls {
        let Some(receiver) = call.receiver.as_deref() else {
            continue;
        };
        let Some(mut members) = dotted_identifiers(receiver) else {
            continue;
        };
        let local = members.remove(0);
        if matches!(local, "self" | "Self") {
            continue;
        }
        let Some(binding) = types
            .iter()
            .filter(|binding| {
                binding.local == local && binding.line <= call.line && call.line <= binding.end_line
            })
            .max_by_key(|binding| binding.line)
        else {
            continue;
        };
        let mut receiver = binding.type_name.clone();
        for member in members {
            receiver.push('.');
            receiver.push_str(member);
        }
        call.receiver = Some(receiver);
    }
}

fn local_types(body: Node<'_>, source: &[u8]) -> Vec<ScopedType> {
    let end_line = body.end_position().row as u32 + 1;
    let mut types = Vec::new();

    if let Some(owner) = body.parent() {
        let mut cursor = owner.walk();
        if let Some(parameters) = owner
            .named_children(&mut cursor)
            .find(|child| child.kind() == "parameters")
        {
            let mut parameter_cursor = parameters.walk();
            for parameter in parameters.named_children(&mut parameter_cursor) {
                let (Some(name), Some(value_type)) = (
                    parameter.child_by_field_name("name"),
                    parameter.child_by_field_name("type"),
                ) else {
                    continue;
                };
                let Some(type_name) = callable_receiver_type(value_type, source) else {
                    continue;
                };
                types.push(ScopedType {
                    local: node_text(name, source).trim().to_string(),
                    type_name,
                    line: 0,
                    end_line,
                });
            }
        };
    }

    let mut variables = Vec::new();
    collect_local_variables(body, end_line, &mut variables);
    variables.sort_by_key(|(node, _)| node.start_byte());
    for (variable, scope_end) in variables {
        let Some(name) = first_named_child_of_kind(variable, "identifier") else {
            continue;
        };
        let value_type = variable
            .child_by_field_name("type")
            .or_else(|| initializer_after_equals(variable));
        let Some(type_name) = value_type.and_then(|node| callable_receiver_type(node, source))
        else {
            continue;
        };
        types.push(ScopedType {
            local: node_text(name, source).trim().to_string(),
            type_name,
            line: variable.start_position().row as u32 + 1,
            end_line: scope_end,
        });
    }
    types
}

fn callable_receiver_type(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source).trim();
            name.bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_uppercase())
                .then(|| name.to_string())
        }
        "field_expression" => {
            let name = qualified_type_path(node, source)?;
            dotted_identifiers(&name).is_some().then_some(name)
        }
        "struct_initializer" => {
            let mut cursor = node.walk();
            let value_type = node.named_children(&mut cursor).next()?;
            callable_receiver_type(value_type, source)
        }
        "type_expression"
        | "primary_type_expression"
        | "nullable_type"
        | "pointer_type"
        | "slice_type"
        | "array_type"
        | "error_union_type"
        | "parenthesized_expression" => {
            let mut cursor = node.walk();
            let children = node.named_children(&mut cursor).collect::<Vec<_>>();
            children
                .into_iter()
                .rev()
                .find_map(|child| callable_receiver_type(child, source))
        }
        _ => None,
    }
}

fn qualified_type_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source).trim();
            is_identifier(name).then(|| name.to_string())
        }
        "field_expression" => {
            let object = qualified_type_path(node.child_by_field_name("object")?, source)?;
            let member = node_text(node.child_by_field_name("member")?, source).trim();
            if !is_identifier(member) {
                return None;
            }
            Some(format!("{object}.{member}"))
        }
        "type_expression"
        | "primary_type_expression"
        | "nullable_type"
        | "pointer_type"
        | "slice_type"
        | "array_type"
        | "error_union_type"
        | "parenthesized_expression" => {
            let mut cursor = node.walk();
            let children = node.named_children(&mut cursor).collect::<Vec<_>>();
            children
                .into_iter()
                .rev()
                .find_map(|child| qualified_type_path(child, source))
        }
        _ => None,
    }
}

#[derive(Clone)]
struct ScopedImport {
    binding: ImportBinding,
    end_line: u32,
}

fn rewrite_local_import_calls(body: Node<'_>, source: &[u8], calls: &mut [CallSite]) {
    let imports = local_imports(body, source);
    for call in calls {
        if let Some(receiver) = call.receiver.as_deref() {
            let Some(mut members) = dotted_identifiers(receiver) else {
                continue;
            };
            let local = members.remove(0);
            let Some(import) = visible_import(&imports, local, call.line) else {
                continue;
            };
            let mut path = import.binding.member_path.clone();
            path.extend(members.into_iter().map(str::to_string));
            call.receiver = Some(inline_import_receiver(&import.binding.module, &path));
            continue;
        }

        // A callable itself may be aliased: `const run = module.run; run()`.
        let Some(import) = visible_import(&imports, &call.name, call.line) else {
            continue;
        };
        let Some(callable) = import.binding.member_path.last().cloned() else {
            continue;
        };
        let scope = &import.binding.member_path[..import.binding.member_path.len() - 1];
        call.name = callable;
        call.receiver = Some(inline_import_receiver(&import.binding.module, scope));
    }
}

fn local_imports(body: Node<'_>, source: &[u8]) -> Vec<ScopedImport> {
    let mut nodes = Vec::new();
    collect_local_variables(
        body,
        body.end_position().row as u32 + 1,
        &mut nodes,
    );
    nodes.sort_by_key(|(node, _)| node.start_byte());

    let mut resolved = Vec::<ScopedImport>::new();
    for (node, end_line) in nodes {
        if declaration_keyword(node, source) != Some("const") {
            continue;
        }
        let Some(local_node) = first_named_child_of_kind(node, "identifier") else {
            continue;
        };
        let local = node_text(local_node, source).trim();
        if local.is_empty() {
            continue;
        }
        let line = node.start_position().row as u32 + 1;
        let visible = resolved
            .iter()
            .filter(|import| import.binding.line <= line && line <= import.end_line)
            .map(|import| (import.binding.local.clone(), import.binding.clone()))
            .collect::<HashMap<_, _>>();
        let Some(initializer) = initializer_after_equals(node) else {
            continue;
        };
        let Some((module, member_path)) = import_value(initializer, source, &visible) else {
            continue;
        };
        resolved.push(ScopedImport {
            binding: ImportBinding {
                local: local.to_string(),
                module,
                member_path,
                line,
            },
            end_line,
        });
    }
    resolved
}

fn collect_local_variables<'tree>(
    node: Node<'tree>,
    scope_end_line: u32,
    nodes: &mut Vec<(Node<'tree>, u32)>,
) {
    if matches!(
        node.kind(),
        "function_declaration" | "test_declaration" | "comptime_declaration" | "comptime_statement"
    ) || is_container_kind(node.kind())
    {
        return;
    }

    let scope_end_line = if node.kind() == "block" {
        node.end_position().row as u32 + 1
    } else {
        scope_end_line
    };
    if node.kind() == "variable_declaration" {
        nodes.push((node, scope_end_line));
        if initializer_container(node).is_some() {
            return;
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_variables(child, scope_end_line, nodes);
    }
}

fn visible_import<'a>(imports: &'a [ScopedImport], local: &str, line: u32) -> Option<&'a ScopedImport> {
    imports
        .iter()
        .filter(|import| {
            import.binding.local == local
                && import.binding.line <= line
                && line <= import.end_line
        })
        .max_by_key(|import| import.binding.line)
}

fn dotted_identifiers(value: &str) -> Option<Vec<&str>> {
    let mut members = Vec::new();
    let bytes = value.as_bytes();
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
            members.push(value[start..index].trim());
            start = index + 1;
        }
    }
    if in_escaped_identifier || escaped {
        return None;
    }
    members.push(value[start..].trim());
    (!members.is_empty() && members.iter().all(|member| is_identifier(member))).then_some(members)
}

fn inline_import_receiver(module: &str, members: &[String]) -> String {
    let mut receiver = format!("@import(\"{module}\")");
    for member in members {
        receiver.push('.');
        receiver.push_str(member);
    }
    receiver
}

fn walk_calls_in_body(node: Node<'_>, source: &[u8], calls: &mut Vec<CallSite>) {
    if matches!(node.kind(), "function_declaration" | "test_declaration")
        || matches!(node.kind(), "comptime_declaration" | "comptime_statement")
        || is_container_kind(node.kind())
    {
        return;
    }

    let call = match node.kind() {
        "call_expression" => call_site_from_call(node, source),
        "builtin_function" => call_site_from_builtin(node, source),
        "struct_initializer" => call_site_from_struct_initializer(node, source),
        _ => None,
    };
    if let Some(call) = call {
        calls.push(call);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_calls_in_body(child, source, calls);
    }
}

fn call_site_from_call(node: Node<'_>, source: &[u8]) -> Option<CallSite> {
    let function = node.child_by_field_name("function")?;
    let (name, receiver) = split_callee(function, source)?;
    Some(CallSite {
        name,
        receiver,
        line: node.start_position().row as u32 + 1,
        kind: CallKind::Call,
    })
}

fn call_site_from_builtin(node: Node<'_>, source: &[u8]) -> Option<CallSite> {
    let builtin = first_named_child_of_kind(node, "builtin_identifier")?;
    let name = node_text(builtin, source).trim();
    if name.is_empty() || name == "@import" {
        // `@import` establishes a compile-time module dependency. Zig import
        // bindings are recorded separately in `ParseResult::imports`; adding
        // an external runtime call as well would duplicate that relationship
        // and make every importing callable report a noisy `@import` callee.
        return None;
    }
    Some(CallSite {
        name: name.to_string(),
        receiver: None,
        line: node.start_position().row as u32 + 1,
        kind: CallKind::Call,
    })
}

fn call_site_from_struct_initializer(node: Node<'_>, source: &[u8]) -> Option<CallSite> {
    let mut cursor = node.walk();
    let type_node = node.named_children(&mut cursor).next()?;
    let (name, receiver) = split_callee(type_node, source)?;
    Some(CallSite {
        name,
        receiver,
        line: node.start_position().row as u32 + 1,
        kind: CallKind::Construct,
    })
}

fn split_callee(node: Node<'_>, source: &[u8]) -> Option<(String, Option<String>)> {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source).to_string();
            (!name.is_empty()).then_some((name, None))
        }
        "field_expression" => {
            let object = node.child_by_field_name("object")?;
            let member = node.child_by_field_name("member")?;
            let name = node_text(member, source).to_string();
            let receiver = node_text(object, source).to_string();
            (!name.is_empty() && !receiver.is_empty()).then_some((name, Some(receiver)))
        }
        // A builtin used directly as a callee is represented by a
        // `builtin_function`, not a `call_expression`, and is handled by
        // `call_site_from_builtin`. Other expression-shaped callees stay
        // unresolved rather than manufacturing a misleading symbol name.
        "builtin_function" => None,
        _ => None,
    }
}

fn extract_import_bindings(root: Node<'_>, source: &[u8]) -> Vec<ImportBinding> {
    let mut declarations = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "variable_declaration"
            && declaration_keyword(child, source) == Some("const")
        {
            declarations.push(child);
        }
    }

    // Resolve aliases to a fixed point so declaration order does not matter.
    // Zig facade files commonly bind an import once and then expose renamed
    // module, type, or namespace aliases from it.
    let mut bindings = HashMap::<String, ImportBinding>::new();
    for _ in 0..declarations.len() {
        let mut changed = false;
        for declaration in &declarations {
            let Some(local_node) = first_named_child_of_kind(*declaration, "identifier") else {
                continue;
            };
            let local = node_text(local_node, source).trim();
            if local.is_empty() || bindings.contains_key(local) {
                continue;
            }
            let Some(initializer) = initializer_after_equals(*declaration) else {
                continue;
            };
            let Some((module, member_path)) = import_value(initializer, source, &bindings) else {
                continue;
            };
            bindings.insert(
                local.to_string(),
                ImportBinding {
                    local: local.to_string(),
                    module,
                    member_path,
                    line: declaration.start_position().row as u32 + 1,
                },
            );
            changed = true;
        }
        if !changed {
            break;
        }
    }

    // Preserve source order even when a forward alias needed a later pass.
    declarations
        .iter()
        .filter_map(|declaration| {
            let local = first_named_child_of_kind(*declaration, "identifier")?;
            bindings.get(node_text(local, source).trim()).cloned()
        })
        .collect()
}

fn import_value(
    node: Node<'_>,
    source: &[u8],
    bindings: &HashMap<String, ImportBinding>,
) -> Option<(String, Vec<String>)> {
    match node.kind() {
        "builtin_function" => import_module(node, source).map(|module| (module, Vec::new())),
        "identifier" => {
            let binding = bindings.get(node_text(node, source).trim())?;
            Some((binding.module.clone(), binding.member_path.clone()))
        }
        "field_expression" => {
            let object = node.child_by_field_name("object")?;
            let member = node.child_by_field_name("member")?;
            let member = node_text(member, source).trim();
            if !is_identifier(member) {
                return None;
            }
            let (module, mut member_path) = import_value(object, source, bindings)?;
            member_path.push(member.to_string());
            Some((module, member_path))
        }
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            let expression = node.named_children(&mut cursor).next()?;
            import_value(expression, source, bindings)
        }
        _ => None,
    }
}

fn import_module(node: Node<'_>, source: &[u8]) -> Option<String> {
    let builtin = first_named_child_of_kind(node, "builtin_identifier")?;
    if node_text(builtin, source) != "@import" {
        return None;
    }
    let arguments = first_named_child_of_kind(node, "arguments")?;
    let string = first_named_child_of_kind(arguments, "string")?;
    let raw_string = node_text(string, source);
    let raw_spec = raw_string.strip_prefix('"')?.strip_suffix('"')?;
    if raw_spec.is_empty() {
        return None;
    }
    Some(if raw_spec.ends_with(".zig")
        && !raw_spec.starts_with("./")
        && !raw_spec.starts_with("../")
    {
        format!("./{raw_spec}")
    } else {
        raw_spec.to_string()
    })
}

fn initializer_after_equals(node: Node<'_>) -> Option<Node<'_>> {
    let mut after_equals = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "=" {
            after_equals = true;
        } else if after_equals && child.is_named() {
            return Some(child);
        }
    }
    None
}

fn error_members(node: Node<'_>, source: &[u8]) -> Vec<Declaration> {
    let mut declarations = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "identifier" || child.is_missing() {
            continue;
        }
        let name = node_text(child, source).trim().to_string();
        if name.is_empty() {
            continue;
        }
        let mut declaration = make_declaration(
            child,
            source,
            DeclarationKind::EnumMember,
            name.clone(),
            name,
            None,
            Vec::new(),
        );
        declaration.visibility = String::new();
        declarations.push(declaration);
    }
    declarations
}

fn make_declaration(
    node: Node<'_>,
    source: &[u8],
    kind: DeclarationKind,
    name: String,
    signature: String,
    native_kind: Option<String>,
    children: Vec<Declaration>,
) -> Declaration {
    let (docs, doc_start_byte) = leading_docs(node, source);
    Declaration {
        kind,
        name,
        signature,
        bases: Vec::new(),
        attrs: declaration_attrs(node, source),
        docs,
        docs_inside: false,
        visibility: visibility(node, source),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        doc_start_byte,
        native_kind,
        modifiers: direct_modifiers(node, source),
        deprecated: false,
        children,
        calls: Vec::new(),
    }
}

/// Finds a container used as the initializer, excluding container-shaped types.
fn initializer_container(node: Node<'_>) -> Option<Node<'_>> {
    let mut after_equals = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "=" {
            after_equals = true;
            continue;
        }
        if after_equals && is_container_kind(child.kind()) {
            return Some(child);
        }
    }
    None
}

fn type_container(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("type")
        .and_then(first_container_descendant)
}

fn first_container_descendant(node: Node<'_>) -> Option<Node<'_>> {
    if is_container_kind(node.kind()) {
        return Some(node);
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find_map(first_container_descendant);
    found
}

fn container_children(container: Node<'_>, source: &[u8]) -> Vec<Declaration> {
    if container.kind() == "error_set_declaration" {
        return error_members(container, source);
    }
    let Some((_, _, kind)) = container_shape(container.kind()) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    walk_container(container, source, kind, &mut children);
    children
}

fn is_container_kind(kind: &str) -> bool {
    matches!(
        kind,
        "struct_declaration"
            | "enum_declaration"
            | "union_declaration"
            | "opaque_declaration"
            | "error_set_declaration"
    )
}

fn container_shape(kind: &str) -> Option<(DeclarationKind, &'static str, ContainerKind)> {
    match kind {
        "struct_declaration" => Some((DeclarationKind::Struct, "struct", ContainerKind::Type)),
        "enum_declaration" => Some((DeclarationKind::Enum, "enum", ContainerKind::Enum)),
        "union_declaration" => Some((DeclarationKind::Struct, "union", ContainerKind::Type)),
        "opaque_declaration" => Some((DeclarationKind::Struct, "opaque", ContainerKind::Type)),
        "error_set_declaration" => Some((DeclarationKind::Enum, "error set", ContainerKind::Enum)),
        _ => None,
    }
}

fn container_signature(node: Node<'_>, container: Node<'_>, source: &[u8]) -> String {
    let brace_start = container_header_end(container);
    collapse_ws(&String::from_utf8_lossy(
        &source[node.start_byte()..brace_start],
    ))
}

fn bare_container_signature(node: Node<'_>, source: &[u8]) -> String {
    collapse_ws(&String::from_utf8_lossy(
        &source[node.start_byte()..container_header_end(node)],
    ))
}

fn container_header_end(container: Node<'_>) -> usize {
    let mut cursor = container.walk();
    let end = container
        .children(&mut cursor)
        .find(|child| child.kind() == "{")
        .map_or(container.end_byte(), |brace| brace.start_byte());
    end
}

fn declaration_attrs(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut attrs = Vec::new();
    if node.kind() == "function_declaration" || is_container_kind(node.kind()) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if is_declaration_qualifier(child.kind()) {
                attrs.push(collapse_ws(node_text(child, source)));
            }
        }
        return attrs;
    }
    collect_declaration_attrs(node, source, true, &mut attrs);
    attrs
}

fn collect_declaration_attrs(
    node: Node<'_>,
    source: &[u8],
    is_root: bool,
    attrs: &mut Vec<String>,
) {
    if is_declaration_qualifier(node.kind()) {
        attrs.push(collapse_ws(node_text(node, source)));
        return;
    }

    if node.kind() == "pointer_type" {
        if let Some(alignment) = direct_pointer_alignment(node, source) {
            attrs.push(alignment);
        }
    }

    if !is_root
        && (matches!(
            node.kind(),
            "block"
                | "parameters"
                | "call_expression"
                | "builtin_function"
                | "struct_initializer"
                | "anonymous_struct_initializer"
                | "initializer_list"
        ) || is_container_kind(node.kind()))
    {
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_declaration_attrs(child, source, false, attrs);
    }
}

fn direct_pointer_alignment(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let children = node.children(&mut cursor).collect::<Vec<_>>();
    let align_index = children.iter().position(|child| child.kind() == "align")?;
    let start = children[align_index].start_byte();
    let mut depth = 0_usize;
    for child in &children[align_index + 1..] {
        match child.kind() {
            "(" => depth += 1,
            ")" if depth == 1 => {
                return Some(collapse_ws(&String::from_utf8_lossy(
                    &source[start..child.end_byte()],
                )));
            }
            ")" => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

fn is_declaration_qualifier(kind: &str) -> bool {
    matches!(
        kind,
        "byte_alignment" | "address_space" | "link_section" | "calling_convention"
    )
}

fn is_identifier(value: &str) -> bool {
    if let Some(body) = value
        .strip_prefix("@\"")
        .and_then(|value| value.strip_suffix('"'))
    {
        if body.is_empty() {
            return false;
        }
        let mut escaped = false;
        for byte in body.bytes() {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if matches!(byte, b'"' | b'\n' | b'\r') {
                return false;
            }
        }
        return !escaped;
    }
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn direct_modifiers(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut modifiers = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            continue;
        }
        let modifier = node_text(child, source).trim();
        if matches!(
            modifier,
            "export" | "extern" | "inline" | "noinline" | "threadlocal" | "comptime" | "packed"
        ) && !modifiers.iter().any(|existing| existing == modifier)
        {
            modifiers.push(modifier.to_string());
        }
    }
    modifiers
}

fn extend_unique(target: &mut Vec<String>, additions: Vec<String>) {
    for addition in additions {
        if !target.contains(&addition) {
            target.push(addition);
        }
    }
}

fn declaration_keyword(node: Node<'_>, source: &[u8]) -> Option<&'static str> {
    node_text(node, source)
        .split_whitespace()
        .find_map(|token| match token {
            "const" => Some("const"),
            "var" => Some("var"),
            _ => None,
        })
}

fn visibility(node: Node<'_>, source: &[u8]) -> String {
    if node_text(node, source)
        .split_whitespace()
        .take_while(|token| !matches!(*token, "fn" | "const" | "var"))
        .any(|token| matches!(token, "pub" | "export"))
    {
        "public".to_string()
    } else {
        "private".to_string()
    }
}

fn leading_docs(node: Node<'_>, source: &[u8]) -> (Vec<String>, usize) {
    let mut docs = Vec::new();
    let mut doc_start_byte = node.start_byte();
    let mut sibling = node.prev_named_sibling();
    let mut next_start_row = node.start_position().row;
    while let Some(comment) = sibling {
        let text = node_text(comment, source);
        if comment.kind() != "comment"
            || !text.starts_with("///")
            || comment.end_position().row + 1 < next_start_row
        {
            break;
        }
        docs.push(text.to_string());
        doc_start_byte = comment.start_byte();
        next_start_row = comment.start_position().row;
        sibling = comment.prev_named_sibling();
    }
    docs.reverse();
    (docs, doc_start_byte)
}

fn apply_container_docs(declaration: &mut Declaration, container: Node<'_>, source: &[u8]) {
    // The shared IR has one doc vector and one placement bit. When a type has
    // both outer `///` docs and inner `//!` container docs, preserve the outer
    // docs rather than merging text from two semantically distinct locations.
    if !declaration.docs.is_empty() {
        return;
    }

    let mut docs = Vec::new();
    let mut inside_body = false;
    let mut cursor = container.walk();
    for child in container.children(&mut cursor) {
        if child.kind() == "{" {
            inside_body = true;
            continue;
        }
        if !inside_body || !child.is_named() {
            continue;
        }
        let text = node_text(child, source);
        if child.kind() != "comment" || !text.starts_with("//!") {
            break;
        }
        docs.push(text.to_string());
    }
    if !docs.is_empty() {
        declaration.docs = docs;
        declaration.docs_inside = true;
    }
}

fn first_named_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let child = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    child
}

fn collapsed_node_text(node: Node<'_>, source: &[u8], trailing: &[char]) -> String {
    collapse_ws(node_text(node, source))
        .trim_end_matches(trailing)
        .trim()
        .to_string()
}

fn node_text<'source>(node: Node<'_>, source: &'source [u8]) -> &'source str {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()]).unwrap_or("")
}

fn count_parse_errors(root: Node<'_>) -> usize {
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "ERROR" || node.is_missing() {
            count += 1;
        }
        for index in 0..node.child_count() as u32 {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_declaration<'a>(
        declarations: &'a [Declaration],
        name: &str,
    ) -> Option<&'a Declaration> {
        declarations.iter().find_map(|declaration| {
            (declaration.name == name)
                .then_some(declaration)
                .or_else(|| find_declaration(&declaration.children, name))
        })
    }

    fn declaration_named<'a>(declarations: &'a [Declaration], name: &str) -> &'a Declaration {
        find_declaration(declarations, name)
            .unwrap_or_else(|| panic!("declaration {name:?} not found"))
    }

    #[test]
    fn extracts_direct_calls_and_typed_struct_initializers() {
        let source = br#"fn leaf() void {}
fn helper() u8 { return 1; }
fn outer() void {
    leaf();
    std.debug.print("x", .{});
    _ = geom.Point{ .x = 1 };
    _ = Widget{};
    _ = @as(u8, helper());
    const Nested = struct {
        fn nested() void { hidden(); }
    };
}
test "direct calls" { leaf(); }
"#;
        let parsed = parse_zig(Path::new("calls.zig"), source);
        let outer = declaration_named(&parsed.declarations, "outer");
        let calls: Vec<_> = outer
            .calls
            .iter()
            .map(|call| (call.name.as_str(), call.receiver.as_deref(), call.kind))
            .collect();

        assert_eq!(
            calls,
            vec![
                ("leaf", None, CallKind::Call),
                ("print", Some("std.debug"), CallKind::Call),
                ("Point", Some("geom"), CallKind::Construct),
                ("Widget", None, CallKind::Construct),
                ("@as", None, CallKind::Call),
                ("helper", None, CallKind::Call),
            ]
        );
        assert!(outer.calls.iter().all(|call| call.name != "hidden"));

        let nested = declaration_named(&outer.children, "nested");
        assert_eq!(nested.calls.len(), 1);
        assert_eq!(nested.calls[0].name, "hidden");

        let test = declaration_named(&parsed.declarations, "direct calls");
        assert_eq!(test.calls.len(), 1);
        assert_eq!(test.calls[0].name, "leaf");
    }

    #[test]
    fn rewrites_escaped_qualified_parameter_receivers() {
        let source = br#"const helper = @import("helper.zig");
pub fn typed(value: *helper.@"Type.With-Dash") void {
    value.ping();
}
"#;
        let parsed = parse_zig(Path::new("main.zig"), source);
        let typed = declaration_named(&parsed.declarations, "typed");
        assert_eq!(typed.calls.len(), 1);
        assert_eq!(typed.calls[0].name, "ping");
        assert_eq!(
            typed.calls[0].receiver.as_deref(),
            Some("helper.@\"Type.With-Dash\"")
        );
    }

    #[test]
    fn extracts_only_top_level_import_bindings() {
        let source = br#"const std = @import("std");
const helper: type = @import("lib/helper.zig");
const sibling = @import("../sibling.zig");
const helper_alias = helper;
const Config = helper.Config;
const Inline = @import("inline.zig").Thing;
const Forward = late.Worker;
const late = @import("late.zig");
const payload = @embedFile("asset.zig");
fn inner() void {
    const local = @import("inner.zig");
    _ = local;
}
"#;
        let parsed = parse_zig(Path::new("imports.zig"), source);
        let imports: Vec<_> = parsed
            .imports
            .iter()
            .map(|import| (import.local.as_str(), import.module.as_str(), import.line))
            .collect();

        assert_eq!(
            imports,
            vec![
                ("std", "std", 1),
                ("helper", "./lib/helper.zig", 2),
                ("sibling", "../sibling.zig", 3),
                ("helper_alias", "./lib/helper.zig", 4),
                ("Config", "./lib/helper.zig", 5),
                ("Inline", "./inline.zig", 6),
                ("Forward", "./late.zig", 7),
                ("late", "./late.zig", 8),
            ]
        );
        for (local, expected) in [
            ("helper_alias", Vec::<String>::new()),
            ("Config", vec!["Config".to_string()]),
            ("Inline", vec!["Thing".to_string()]),
            ("Forward", vec!["Worker".to_string()]),
        ] {
            assert_eq!(
                parsed
                    .imports
                    .iter()
                    .find(|import| import.local == local)
                    .expect("alias import")
                    .member_path,
                expected,
                "wrong member path for {local}"
            );
        }
        assert!(parsed.imports.iter().all(|import| import.local != "local"));
    }

    #[test]
    fn local_imports_are_rewritten_only_inside_their_lexical_owner() {
        let source = br#"fn unrelated(format: anytype) void {
    format.work();
}
test {
    const format = @import("helper.zig");
    format.work();
    const run = format.run;
    run();
}
"#;
        let parsed = parse_zig(Path::new("local-imports.zig"), source);
        let unrelated = declaration_named(&parsed.declarations, "unrelated");
        assert_eq!(unrelated.calls[0].receiver.as_deref(), Some("format"));

        let test = declaration_named(&parsed.declarations, "test@4");
        assert_eq!(test.calls[0].receiver.as_deref(), Some("@import(\"./helper.zig\")"));
        assert_eq!(test.calls[0].name, "work");
        assert_eq!(test.calls[1].receiver.as_deref(), Some("@import(\"./helper.zig\")"));
        assert_eq!(test.calls[1].name, "run");
        assert!(parsed.imports.is_empty(), "local imports must not become file bindings");
    }

    #[test]
    fn unnamed_tests_have_unique_names() {
        let source = b"test { first(); }\n\ntest { second(); }\n";
        let parsed = parse_zig(Path::new("tests.zig"), source);
        let first = declaration_named(&parsed.declarations, "test@1");
        let second = declaration_named(&parsed.declarations, "test@3");
        assert_eq!(first.calls[0].name, "first");
        assert_eq!(second.calls[0].name, "second");
    }
}
