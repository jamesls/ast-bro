//! Zig declarations parsed directly with tree-sitter.
//!
//! Zig is not available through ast-grep's `SupportLang`, so this adapter
//! walks `tree_sitter::Node`s and produces the same declaration IR as the
//! ast-grep-backed adapters.

use super::base::collapse_ws;
use crate::core::{CallKind, CallSite, Declaration, DeclarationKind, ImportBinding, ParseResult};
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
        let (kind, native_kind, child_kind) = match container.kind() {
            "struct_declaration" => (DeclarationKind::Struct, "struct", ContainerKind::Type),
            "enum_declaration" => (DeclarationKind::Enum, "enum", ContainerKind::Enum),
            "union_declaration" => (DeclarationKind::Struct, "union", ContainerKind::Type),
            "opaque_declaration" => (DeclarationKind::Struct, "opaque", ContainerKind::Type),
            "error_set_declaration" => (DeclarationKind::Enum, "error set", ContainerKind::Enum),
            _ => return None,
        };

        let children = if container.kind() == "error_set_declaration" {
            error_members(container, source)
        } else {
            let mut children = Vec::new();
            walk_container(container, source, child_kind, &mut children);
            children
        };

        return Some(make_declaration(
            node,
            source,
            kind,
            name,
            container_signature(node, container, source),
            Some(native_kind.to_string()),
            children,
        ));
    }

    let native_kind = declaration_keyword(node, source)?;
    Some(make_declaration(
        node,
        source,
        DeclarationKind::Field,
        name,
        collapsed_node_text(node, source, &[',', ';']),
        Some(native_kind.to_string()),
        Vec::new(),
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
        Vec::new(),
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
    Some(make_declaration(
        node,
        source,
        kind,
        name,
        collapsed_node_text(node, source, &[',']),
        None,
        Vec::new(),
    ))
}

fn test_to_decl(node: Node<'_>, source: &[u8]) -> Option<Declaration> {
    let mut cursor = node.walk();
    let name_node = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "string" | "identifier"));
    let name = name_node
        .map(|child| {
            node_text(child, source)
                .trim()
                .trim_matches('"')
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "test".to_string());
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
        Vec::new(),
    );
    declaration.calls = extract_call_sites(body, source);
    Some(declaration)
}

fn extract_call_sites(body: Node<'_>, source: &[u8]) -> Vec<CallSite> {
    let mut calls = Vec::new();
    walk_calls_in_body(body, source, &mut calls);
    calls
}

fn walk_calls_in_body(node: Node<'_>, source: &[u8], calls: &mut Vec<CallSite>) {
    if matches!(node.kind(), "function_declaration" | "test_declaration")
        || is_container_kind(node.kind())
    {
        return;
    }

    let call = match node.kind() {
        "call_expression" => call_site_from_call(node, source),
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
        // Zig compiler builtins such as `@memcpy` and `@This` are not user
        // symbols. Other expression-shaped callees are intentionally left
        // unresolved rather than manufacturing a misleading symbol name.
        "builtin_function" => None,
        _ => None,
    }
}

fn extract_import_bindings(root: Node<'_>, source: &[u8]) -> Vec<ImportBinding> {
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "variable_declaration" {
            continue;
        }
        if let Some(import) = import_binding(child, source) {
            imports.push(import);
        }
    }
    imports
}

fn import_binding(node: Node<'_>, source: &[u8]) -> Option<ImportBinding> {
    if declaration_keyword(node, source) != Some("const") {
        return None;
    }
    let local_node = first_named_child_of_kind(node, "identifier")?;
    let initializer = initializer_after_equals(node)?;
    if initializer.kind() != "builtin_function" {
        return None;
    }

    let builtin = first_named_child_of_kind(initializer, "builtin_identifier")?;
    if node_text(builtin, source) != "@import" {
        return None;
    }
    let arguments = first_named_child_of_kind(initializer, "arguments")?;
    let string = first_named_child_of_kind(arguments, "string")?;
    let raw_string = node_text(string, source);
    let raw_spec = raw_string.strip_prefix('"')?.strip_suffix('"')?;
    if raw_spec.is_empty() {
        return None;
    }
    let module = if raw_spec.ends_with(".zig")
        && !raw_spec.starts_with("./")
        && !raw_spec.starts_with("../")
    {
        format!("./{raw_spec}")
    } else {
        raw_spec.to_string()
    };

    Some(ImportBinding {
        local: node_text(local_node, source).to_string(),
        module,
        line: node.start_position().row as u32 + 1,
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
        declarations.push(make_declaration(
            child,
            source,
            DeclarationKind::EnumMember,
            name.clone(),
            name,
            None,
            Vec::new(),
        ));
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
        attrs: Vec::new(),
        docs,
        docs_inside: false,
        visibility: visibility(node, source),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        doc_start_byte,
        native_kind,
        modifiers: Vec::new(),
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

fn container_signature(node: Node<'_>, container: Node<'_>, source: &[u8]) -> String {
    let mut cursor = container.walk();
    let brace_start = container
        .children(&mut cursor)
        .find(|child| child.kind() == "{")
        .map_or(container.end_byte(), |brace| brace.start_byte());
    collapse_ws(&String::from_utf8_lossy(
        &source[node.start_byte()..brace_start],
    ))
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
    if node_text(node, source).split_whitespace().next() == Some("pub") {
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
                ("helper", None, CallKind::Call),
            ]
        );
        assert!(outer.calls.iter().all(|call| call.name != "hidden"));

        let test = declaration_named(&parsed.declarations, "direct calls");
        assert_eq!(test.calls.len(), 1);
        assert_eq!(test.calls[0].name, "leaf");
    }

    #[test]
    fn extracts_only_top_level_import_bindings() {
        let source = br#"const std = @import("std");
const helper: type = @import("lib/helper.zig");
const sibling = @import("../sibling.zig");
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
            ]
        );
    }
}
