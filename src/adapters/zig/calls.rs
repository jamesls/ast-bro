//! Resolve statically named calls in their original lexical scope.

use super::*;
use crate::zig_syntax::{identifier, quote, string_value};

// Cyclic aliases and recursive factories are legal inputs to the parser. This
// bounds syntax-only expansion; exceeding it leaves the call unresolved.
const MAX_ALIAS_DEPTH: usize = 64;

struct Context<'tree, 'source> {
    source: &'source [u8],
    declarations: HashMap<String, Node<'tree>>,
    bodies: HashMap<usize, Node<'tree>>,
    values: std::cell::RefCell<HashMap<usize, Option<String>>>,
    active: std::cell::RefCell<std::collections::HashSet<usize>>,
}

impl<'tree, 'source> Context<'tree, 'source> {
    fn new(root: Node<'tree>, source: &'source [u8]) -> Self {
        let mut declarations = HashMap::new();
        let mut bodies = HashMap::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if matches!(
                node.kind(),
                "variable_declaration" | "function_declaration" | "container_field"
            ) {
                let path = declaration_path(node, source);
                declarations.insert(
                    path.strip_prefix("@This().").unwrap_or(&path).to_string(),
                    node,
                );
            }
            if matches!(
                node.kind(),
                "function_declaration"
                    | "test_declaration"
                    | "comptime_statement"
                    | "comptime_declaration"
            ) {
                let body = node
                    .child_by_field_name("body")
                    .or_else(|| first_named_child_of_kind(node, "block"))
                    .or_else(|| first_named_child_of_kind(node, "expression_statement"));
                if let Some(body) = body {
                    bodies.insert(node.start_byte(), body);
                }
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        Self {
            source,
            declarations,
            bodies,
            values: Default::default(),
            active: Default::default(),
        }
    }

    fn declaration(&self, path: &str) -> Option<Node<'tree>> {
        self.declarations
            .get(path.strip_prefix("@This().").unwrap_or(path))
            .copied()
    }
}

pub(super) fn populate(declarations: &mut [Declaration], root: Node<'_>, source: &[u8]) {
    let context = Context::new(root, source);
    fn visit(declarations: &mut [Declaration], context: &Context<'_, '_>) {
        for declaration in declarations {
            if let Some(body) = context.bodies.get(&declaration.start_byte) {
                walk(*body, context, &mut declaration.calls);
            }
            visit(&mut declaration.children, context);
        }
    }
    visit(declarations, &context);
}

fn walk(node: Node<'_>, context: &Context<'_, '_>, calls: &mut Vec<CallSite>) {
    let source = context.source;
    if matches!(
        node.kind(),
        "function_declaration" | "test_declaration" | "comptime_declaration" | "comptime_statement"
    ) || is_container_kind(node.kind())
    {
        return;
    }
    let target = match node.kind() {
        "call_expression" => node
            .child_by_field_name("function")
            .and_then(|function| callee(function, node, context)),
        "builtin_function" => {
            let name = first_named_child_of_kind(node, "builtin_identifier")
                .map(|name| node_text(name, source));
            match name {
                Some("@import" | "@This" | "@field") => None,
                Some("@call") => arguments(node)
                    .get(1)
                    .and_then(|target| callee(*target, node, context)),
                Some(name) => Some((name.to_string(), None)),
                None => None,
            }
        }
        "struct_initializer" => node.named_child(0).and_then(|ty| callee(ty, node, context)),
        _ => None,
    };
    if let Some((name, receiver)) = target {
        calls.push(CallSite {
            name,
            receiver,
            line: node.start_position().row as u32 + 1,
            kind: if node.kind() == "struct_initializer" {
                CallKind::Construct
            } else {
                CallKind::Call
            },
        });
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, context, calls);
    }
}

fn callee(
    node: Node<'_>,
    call: Node<'_>,
    context: &Context<'_, '_>,
) -> Option<(String, Option<String>)> {
    let source = context.source;
    if node.kind() == "parenthesized_expression" {
        return callee(crate::zig_syntax::first_expression(node)?, call, context);
    }
    if node.kind() == "field_expression" && node.child_by_field_name("object").is_none() {
        let name = identifier(node_text(node.child_by_field_name("member")?, source));
        // Keep a receiver even without type context, so `.init()` cannot bind
        // an unrelated globally unique function named `init`.
        return Some((
            name,
            Some(expected_type(call, context).unwrap_or_else(|| ".".into())),
        ));
    }
    if let Some(path) = value(node, context, 0) {
        return split_path(&path);
    }
    if node.kind() == "field_expression" {
        let name = identifier(node_text(node.child_by_field_name("member")?, source));
        return Some((
            name,
            Some(node_text(node.child_by_field_name("object")?, source).to_string()),
        ));
    }
    // A runtime-selected callee still represents a call. Retain its syntax
    // and an unknown receiver instead of dropping it or binding a homonym.
    Some((node_text(node, source).trim().to_string(), Some("?".into())))
}

fn split_path(path: &str) -> Option<(String, Option<String>)> {
    if let Some(dot) = crate::symbol_path::last_unquoted_byte(path, b'.') {
        // Dots inside @import strings are quoted and therefore skipped.
        Some((path[dot + 1..].to_string(), Some(path[..dot].to_string())))
    } else if !path.is_empty() {
        Some((path.to_string(), None))
    } else {
        None
    }
}

fn value(node: Node<'_>, context: &Context<'_, '_>, depth: usize) -> Option<String> {
    if let Some(value) = context.values.borrow().get(&node.id()) {
        return value.clone();
    }
    if !context.active.borrow_mut().insert(node.id()) {
        return None;
    }
    let result = compute_value(node, context, depth);
    context.active.borrow_mut().remove(&node.id());
    context
        .values
        .borrow_mut()
        .insert(node.id(), result.clone());
    result
}

fn compute_value(node: Node<'_>, context: &Context<'_, '_>, depth: usize) -> Option<String> {
    let source = context.source;

    if depth >= MAX_ALIAS_DEPTH {
        return None;
    }
    match node.kind() {
        "identifier" => {
            let name = identifier(node_text(node, source));
            let Some(binding) = binding(node, &name, source) else {
                return Some(name);
            };
            match binding.kind() {
                "variable_declaration" | "parameter" => {
                    if let Some(ty) = binding.child_by_field_name("type") {
                        // Function-pointer annotations describe the signature,
                        // while their initializer may identify a concrete target.
                        if ty.kind() != "function_signature"
                            && !node_text(ty, source).contains("fn (")
                            && !node_text(ty, source).contains("fn(")
                        {
                            if let Some(ty) = value(ty, context, depth + 1) {
                                return Some(ty);
                            }
                        }
                    }
                    if let Some(initializer) = initializer_after_equals(binding) {
                        if is_container_kind(initializer.kind()) {
                            return Some(declaration_path(binding, source));
                        }
                        if let Some(value) = value(initializer, context, depth + 1) {
                            return Some(value);
                        }
                        if matches!(
                            initializer.kind(),
                            "if_expression" | "switch_expression" | "call_expression"
                        ) {
                            return Some(declaration_path(binding, source));
                        }
                    }
                    // A parameter or local value with unknown type is not a
                    // bare function in another file, even if names coincide.
                    Some(format!("?.{name}"))
                }
                "function_declaration" => Some(declaration_path(binding, source)),
                "payload" => {
                    let condition = binding
                        .parent()
                        .and_then(|parent| parent.child_by_field_name("condition"));
                    condition
                        .and_then(|condition| value(condition, context, depth + 1))
                        .or_else(|| Some(format!("?.{name}")))
                }
                _ => Some(name),
            }
        }
        "field_expression" => {
            let object_node = node.child_by_field_name("object")?;
            let member = identifier(node_text(node.child_by_field_name("member")?, source));
            if object_node.kind() == "identifier" {
                if let Some(binding) = binding(
                    object_node,
                    &identifier(node_text(object_node, source)),
                    source,
                ) {
                    if declaration_keyword(binding, source) == Some("const") {
                        if let Some(initializer) = initializer_after_equals(binding) {
                            if let Some(list) =
                                first_named_child_of_kind(initializer, "initializer_list")
                            {
                                let mut cursor = list.walk();
                                for field in list.named_children(&mut cursor) {
                                    if let Some((name, initializer)) =
                                        crate::zig_syntax::field_assignment(field)
                                    {
                                        if identifier(node_text(name, source)) == member {
                                            if let Some(path) =
                                                value(initializer, context, depth + 1)
                                            {
                                                return Some(path);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let object = value(object_node, context, depth + 1)?;
            // Follow a same-file member alias, including aliases inside types.
            let path = format!("{object}.{member}");
            if let Some(declaration) = context.declaration(&path) {
                if declaration.kind() == "container_field" {
                    if let Some(ty) = declaration.child_by_field_name("type") {
                        if let Some(path) = value(ty, context, depth + 1) {
                            return Some(path);
                        }
                    }
                }
                if declaration.kind() == "variable_declaration" {
                    if let Some(initializer) = initializer_after_equals(declaration) {
                        if !is_container_kind(initializer.kind()) {
                            if let Some(value) = value(initializer, context, depth + 1) {
                                return Some(value);
                            }
                        }
                    }
                }
            }
            Some(path)
        }
        "builtin_function" => {
            let name = node_text(
                first_named_child_of_kind(node, "builtin_identifier")?,
                source,
            );
            match name {
                "@import" => Some(format!("@import({})", quote(&import_module(node, source)?))),
                "@This" => Some(namespace(node, source)),
                "@field" => {
                    let args = arguments(node);
                    let object = value(*args.first()?, context, depth + 1)?;
                    let member = string_value(node_text(*args.get(1)?, source))?;
                    Some(format!(
                        "{object}.{}",
                        identifier(&format!("@{}", quote(&member)))
                    ))
                }
                _ => None,
            }
        }
        "call_expression" => {
            let function = node.child_by_field_name("function")?;
            let path = value(function, context, depth + 1)?;
            let declaration = context.declaration(&path)?;
            let ty = declaration.child_by_field_name("type")?;
            if node_text(ty, source) == "type" {
                let body = declaration.child_by_field_name("body")?;
                let containers = crate::zig_syntax::returned_containers(body);
                if containers.len() == 1 {
                    return Some(format!("{path}.{}", anonymous_name(containers[0])));
                }
                return None;
            }
            value(ty, context, depth + 1)
        }
        "struct_initializer" => value(node.named_child(0)?, context, depth + 1),
        "pointer_type" | "nullable_type" | "slice_type" | "array_type" | "error_union_type" => {
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            children
                .into_iter()
                .rev()
                .find_map(|child| value(child, context, depth + 1))
        }
        "parenthesized_expression"
        | "try_expression"
        | "comptime_expression"
        | "dereference_expression"
        | "null_coercion_expression" => value(
            crate::zig_syntax::first_expression(node)?,
            context,
            depth + 1,
        ),
        _ => None,
    }
}

fn arguments(node: Node<'_>) -> Vec<Node<'_>> {
    let Some(arguments) = first_named_child_of_kind(node, "arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

/// Search one lexical scope at a time. Byte ranges and ancestry distinguish
/// sibling blocks on the same line; unknown bindings still shadow outer ones.
fn binding<'tree>(node: Node<'tree>, name: &str, source: &[u8]) -> Option<Node<'tree>> {
    let mut scope = node.parent();
    while let Some(parent) = scope {
        let mut cursor = parent.walk();
        let children: Vec<_> = parent.children(&mut cursor).collect();
        for (index, payload) in children
            .iter()
            .enumerate()
            .filter(|(_, child)| child.kind() == "payload")
        {
            let end = children[index + 1..]
                .iter()
                .find(|child| matches!(child.kind(), "else" | "else_clause" | "payload"))
                .map_or(parent.end_byte(), |child| child.start_byte());
            if payload.end_byte() > node.start_byte() || node.end_byte() > end {
                continue;
            }
            let mut cursor = payload.walk();
            if payload
                .named_children(&mut cursor)
                .any(|capture| identifier(node_text(capture, source)) == name)
            {
                return Some(*payload);
            }
        }
        if parent.kind() == "function_declaration" {
            if let Some(parameters) = first_named_child_of_kind(parent, "parameters") {
                let mut cursor = parameters.walk();
                for parameter in parameters.named_children(&mut cursor) {
                    if parameter
                        .child_by_field_name("name")
                        .is_some_and(|n| identifier(node_text(n, source)) == name)
                    {
                        return Some(parameter);
                    }
                }
            }
        }
        if matches!(parent.kind(), "block" | "source_file") || is_container_kind(parent.kind()) {
            let mut cursor = parent.walk();
            let mut found = None;
            for child in parent.named_children(&mut cursor) {
                if parent.kind() == "block" && child.end_byte() > node.start_byte() {
                    continue;
                }
                let declared = match child.kind() {
                    "variable_declaration" => first_named_child_of_kind(child, "identifier"),
                    "function_declaration" => child.child_by_field_name("name"),
                    _ => None,
                };
                if declared.is_some_and(|n| identifier(node_text(n, source)) == name) {
                    found = Some(child);
                }
            }
            if found.is_some() {
                return found;
            }
        }
        scope = parent.parent();
    }
    None
}

fn expected_type(node: Node<'_>, context: &Context<'_, '_>) -> Option<String> {
    let mut parent = node.parent();
    while let Some(owner) = parent {
        match owner.kind() {
            "variable_declaration" => {
                return owner
                    .child_by_field_name("type")
                    .and_then(|ty| value(ty, context, 0))
            }
            "arguments" => {
                let call = owner.parent()?;
                let args = arguments(call);
                let index = args.iter().position(|argument| {
                    argument.start_byte() <= node.start_byte()
                        && node.end_byte() <= argument.end_byte()
                })?;
                if call.kind() == "builtin_function" {
                    if node_text(call.named_child(0)?, context.source) == "@as" && index == 1 {
                        return value(*args.first()?, context, 0);
                    }
                    return None;
                }
                let function = value(call.child_by_field_name("function")?, context, 0)?;
                let declaration = context.declaration(&function)?;
                let parameters = first_named_child_of_kind(declaration, "parameters")?;
                let mut cursor = parameters.walk();
                let params: Vec<_> = parameters
                    .named_children(&mut cursor)
                    .filter(|node| node.kind() == "parameter")
                    .collect();
                let offset = params.len().checked_sub(args.len())?;
                if offset > 1 {
                    return None;
                }
                let ty = params.get(index + offset)?.child_by_field_name("type")?;
                return value(ty, context, 0);
            }
            "field_initializer" | "assignment_expression" => {
                let (field, _) = crate::zig_syntax::field_assignment(owner)?;
                let field = identifier(node_text(field, context.source));
                let initializer = owner.parent()?.parent()?;
                let ty = if initializer.kind() == "struct_initializer" {
                    value(initializer.named_child(0)?, context, 0)?
                } else {
                    expected_type(initializer, context)?
                };
                let declaration = context.declaration(&ty)?;
                let container = initializer_container(declaration)?;
                let mut cursor = container.walk();
                for member in container.named_children(&mut cursor) {
                    if member.kind() == "container_field"
                        && member.child_by_field_name("name").is_some_and(|name| {
                            identifier(node_text(name, context.source)) == field
                        })
                    {
                        return member
                            .child_by_field_name("type")
                            .and_then(|ty| value(ty, context, 0));
                    }
                }
                return None;
            }
            "return_expression" => {
                let mut scope = owner.parent();
                while let Some(function) = scope {
                    if function.kind() == "function_declaration" {
                        return function
                            .child_by_field_name("type")
                            .and_then(|ty| value(ty, context, 0));
                    }
                    scope = function.parent();
                }
                return None;
            }
            "block" | "initializer_list" => return None,
            _ => parent = owner.parent(),
        }
    }
    None
}

fn anonymous_name(node: Node<'_>) -> String {
    let native = container_shape(node.kind()).map_or("struct", |(_, native, _)| native);
    format!(
        "anonymous_{native}@L{}C{}",
        node.start_position().row + 1,
        node.start_position().column + 1
    )
    .replace(' ', "_")
}

fn namespace(node: Node<'_>, source: &[u8]) -> String {
    let mut current = node.parent();
    while let Some(parent) = current {
        if is_container_kind(parent.kind()) {
            return declaration_path(parent, source);
        }
        current = parent.parent();
    }
    "@This()".into()
}

fn declaration_path(node: Node<'_>, source: &[u8]) -> String {
    let mut segments = Vec::new();
    let mut current = Some(node);
    while let Some(parent) = current {
        match parent.kind() {
            "variable_declaration" => {
                if let Some(name) = first_named_child_of_kind(parent, "identifier") {
                    segments.push(identifier(node_text(name, source)));
                }
            }
            "function_declaration" => {
                if let Some(name) = parent.child_by_field_name("name") {
                    segments.push(identifier(node_text(name, source)));
                }
            }
            "container_field" => {
                if let Some(name) = parent.child_by_field_name("name") {
                    segments.push(identifier(node_text(name, source)));
                }
            }
            "test_declaration" => segments.push(format!(
                "test@L{}C{}",
                parent.start_position().row + 1,
                parent.start_position().column + 1
            )),
            "comptime_statement" | "comptime_declaration" => segments.push(format!(
                "comptime@L{}C{}",
                parent.start_position().row + 1,
                parent.start_position().column + 1
            )),
            kind if is_container_kind(kind)
                && !parent.parent().is_some_and(|owner| {
                    owner.kind() == "variable_declaration"
                        && initializer_container(owner) == Some(parent)
                }) =>
            {
                segments.push(anonymous_name(parent))
            }
            _ => {}
        }
        current = parent.parent();
    }
    segments.reverse();
    if segments.len() == 1 {
        format!("@This().{}", segments[0])
    } else {
        segments.join(".")
    }
}
