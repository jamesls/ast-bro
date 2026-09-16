//! Shared Zig grammar, literal decoding, and symbol spelling.

pub mod build;

use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_zig() -> *const ();
}

// SAFETY: The statically linked parser exports tree-sitter's language factory.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_zig) };

pub fn parse(source: &[u8]) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&LANGUAGE.into())
        .expect("bundled Zig grammar must load");
    parser
        .parse(source, None)
        .expect("uncancelled parse must produce a tree")
}

pub fn first_expression(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = node.walk();
    let expression = node
        .named_children(&mut cursor)
        .find(|child| child.kind() != "comment");
    expression
}

/// Decode a Zig string literal, including byte and Unicode escapes.
pub fn string_value(literal: &str) -> Option<String> {
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            bytes.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            continue;
        }
        match chars.next()? {
            'n' => bytes.push(b'\n'),
            'r' => bytes.push(b'\r'),
            't' => bytes.push(b'\t'),
            '\\' => bytes.push(b'\\'),
            '"' => bytes.push(b'"'),
            '\'' => bytes.push(b'\''),
            'x' => {
                let high = chars.next()?.to_digit(16)?;
                let low = chars.next()?.to_digit(16)?;
                bytes.push((high * 16 + low) as u8);
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut value = 0u32;
                let mut digits = 0;
                loop {
                    let digit = chars.next()?;
                    if digit == '}' {
                        break;
                    }
                    value = value.checked_mul(16)?.checked_add(digit.to_digit(16)?)?;
                    digits += 1;
                }
                if digits == 0 {
                    return None;
                }
                bytes.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut [0; 4]).as_bytes());
            }
            _ => return None,
        }
    }
    String::from_utf8(bytes).ok()
}

/// A stable identifier key keeps punctuation quoted so qualified paths remain unambiguous.
pub fn identifier(name: &str) -> String {
    let value = name
        .strip_prefix('@')
        .and_then(string_value)
        .unwrap_or_else(|| name.to_string());
    let plain = value
        .bytes()
        .enumerate()
        .all(|(i, ch)| ch == b'_' || ch.is_ascii_alphabetic() || i > 0 && ch.is_ascii_digit());
    if !value.is_empty() && plain {
        value
    } else {
        format!("@{}", quote(&value))
    }
}

pub fn quote(value: &str) -> String {
    // JSON's escapes for control characters differ from Zig's. Emit Zig byte
    // escapes for controls, and leave Unicode scalar values as UTF-8.
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            ch if ch.is_control() => {
                for byte in ch.encode_utf8(&mut [0; 4]).as_bytes() {
                    use std::fmt::Write;
                    write!(quoted, "\\x{byte:02x}").expect("writing to String cannot fail");
                }
            }
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

pub fn import_spec(value: &str) -> String {
    if (value.ends_with(".zig") || value.ends_with(".zon"))
        && !std::path::Path::new(value).is_absolute()
        && !value.starts_with("./")
        && !value.starts_with("../")
    {
        format!("./{value}")
    } else {
        value.to_string()
    }
}

/// Both grammar alternatives for `.member = value` in an initializer list.
pub fn field_assignment(
    node: tree_sitter::Node<'_>,
) -> Option<(tree_sitter::Node<'_>, tree_sitter::Node<'_>)> {
    match node.kind() {
        "field_initializer" => Some((node.named_child(0)?, node.named_child(1)?)),
        "assignment_expression" if node.parent()?.kind() == "initializer_list" => {
            let left = node.child_by_field_name("left")?;
            if left.kind() != "field_expression" || left.child_by_field_name("object").is_some() {
                return None;
            }
            Some((
                left.child_by_field_name("member")?,
                node.child_by_field_name("right")?,
            ))
        }
        _ => None,
    }
}

pub fn returned_containers(body: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut result = Vec::new();
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "struct_declaration"
                | "enum_declaration"
                | "union_declaration"
                | "opaque_declaration"
                | "error_set_declaration"
        ) {
            if node
                .parent()
                .is_some_and(|parent| parent.kind() == "return_expression")
            {
                result.push(node);
            }
            continue;
        }
        if node.kind() == "function_declaration" {
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zig_literals_use_zig_escape_rules() {
        assert_eq!(
            string_value(r#""a\x2eb\u{1f600}\n""#),
            Some("a.b😀\n".into())
        );
        for invalid in [r#""\u{}""#, r#""\xgg""#, r#""\u{110000}""#, r#""\q""#] {
            assert_eq!(string_value(invalid), None);
        }
        assert_eq!(identifier(r#"@"nor\x6dal""#), "normal");
        assert_eq!(identifier(r#"@"a\x2eb::c""#), r#"@"a.b::c""#);
        let value = "\0\n\r\t'\\\"λ";
        assert_eq!(string_value(&quote(value)).as_deref(), Some(value));
    }
}
