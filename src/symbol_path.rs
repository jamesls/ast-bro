//! Separator helpers for qualified symbol paths.
//!
//! Zig escaped identifiers use the source spelling `@"..."` and may contain
//! dots, colons, and other characters that are separators in ast-bro's
//! user-facing symbol paths. These helpers recognize separators only outside
//! such identifiers so the serialized qualified-name format can stay stable.

/// Split a dotted symbol path, ignoring dots inside Zig escaped identifiers.
pub(crate) fn split_dotted(value: &str) -> Vec<&str> {
    split_unquoted(value, ".")
}

/// Split an ast-bro qualified name, ignoring `::` inside Zig escaped
/// identifiers.
pub(crate) fn split_qualified(value: &str) -> Vec<&str> {
    split_unquoted(value, "::")
}

pub(crate) fn first_qualified_separator(value: &str) -> Option<usize> {
    separator_indices(value, "::").next()
}

pub(crate) fn last_qualified_separator(value: &str) -> Option<usize> {
    separator_indices(value, "::").last()
}

pub(crate) fn first_unquoted_byte(value: &str, needle: u8) -> Option<usize> {
    unquoted_byte_indices(value, needle).next()
}

pub(crate) fn last_unquoted_byte(value: &str, needle: u8) -> Option<usize> {
    unquoted_byte_indices(value, needle).last()
}

/// Find the last unquoted namespace separator from a mixed-language receiver.
pub(crate) fn last_unquoted_any(value: &str, needles: &[u8]) -> Option<usize> {
    scanner(value)
        .filter_map(|(index, byte)| needles.contains(&byte).then_some(index))
        .last()
}

/// Find the final multi-byte separator outside an escaped Zig identifier.
/// When separators overlap at one position, the longest one wins.
pub(crate) fn last_unquoted_separator(value: &str, separators: &[&str]) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    scanner(value)
        .filter_map(|(index, _)| {
            separators
                .iter()
                .map(|separator| separator.as_bytes())
                .filter(|separator| bytes[index..].starts_with(separator))
                .max_by_key(|separator| separator.len())
                .map(|separator| (index, separator.len()))
        })
        .last()
}

/// Split on any listed separator outside an escaped Zig identifier.
pub(crate) fn split_unquoted_separators<'a>(value: &'a str, separators: &[&str]) -> Vec<&'a str> {
    let bytes = value.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    for (index, _) in scanner(value) {
        if index < start {
            continue;
        }
        let Some(width) = separators
            .iter()
            .map(|separator| separator.as_bytes())
            .filter(|separator| bytes[index..].starts_with(separator))
            .map(|separator| separator.len())
            .max()
        else {
            continue;
        };
        parts.push(&value[start..index]);
        start = index + width;
    }
    parts.push(&value[start..]);
    parts
}

/// Zig identifiers are either ordinary ASCII identifiers or escaped string
/// spellings. The escaped form may contain symbol-path punctuation.
pub(crate) fn is_zig_identifier(value: &str) -> bool {
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

/// Find a file/symbol separator while ignoring qn `::` separators and colons
/// embedded in escaped Zig identifiers.
pub(crate) fn first_single_colon(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    scanner(value).find_map(|(index, byte)| {
        (byte == b':'
            && bytes.get(index.wrapping_sub(1)) != Some(&b':')
            && bytes.get(index + 1) != Some(&b':'))
        .then_some(index)
    })
}

pub(crate) fn terminal_qualified(value: &str) -> &str {
    last_qualified_separator(value).map_or(value, |index| &value[index + 2..])
}

/// Convert the scope portion of a qn to the dotted spelling accepted by
/// `show`, preserving separator characters inside escaped identifiers.
pub(crate) fn qualified_to_dotted(value: &str) -> String {
    split_qualified(value).join(".")
}

/// Normalize dotted/PHP namespace receivers to ast-bro's `::` spelling
/// without rewriting punctuation that belongs to an escaped Zig identifier.
pub(crate) fn namespace_to_qualified(value: &str) -> String {
    let separators: Vec<usize> = scanner(value)
        .filter_map(|(index, byte)| matches!(byte, b'.' | b'\\').then_some(index))
        .collect();
    if separators.is_empty() {
        return value.to_string();
    }

    let mut normalized = String::with_capacity(value.len() + separators.len());
    let mut start = 0;
    for index in separators {
        normalized.push_str(&value[start..index]);
        normalized.push_str("::");
        start = index + 1;
    }
    normalized.push_str(&value[start..]);
    normalized
}

fn split_unquoted<'a>(value: &'a str, separator: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for index in separator_indices(value, separator) {
        parts.push(&value[start..index]);
        start = index + separator.len();
    }
    parts.push(&value[start..]);
    parts
}

fn separator_indices<'a>(value: &'a str, separator: &'a str) -> impl Iterator<Item = usize> + 'a {
    let bytes = value.as_bytes();
    let separator = separator.as_bytes();
    scanner(value)
        .filter_map(move |(index, _)| bytes[index..].starts_with(separator).then_some(index))
}

fn unquoted_byte_indices(value: &str, needle: u8) -> impl Iterator<Item = usize> + '_ {
    scanner(value).filter_map(move |(index, byte)| (byte == needle).then_some(index))
}

/// Yield byte positions outside `@"..."`. Backslash escapes inside an
/// escaped identifier keep the following quote from ending it.
fn scanner(value: &str) -> impl Iterator<Item = (usize, u8)> + '_ {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut in_identifier = false;
    let mut escaped = false;
    std::iter::from_fn(move || loop {
        let byte = *bytes.get(index)?;
        let current = index;
        index += 1;

        if in_identifier {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_identifier = false;
            }
            continue;
        }

        if byte == b'@' && bytes.get(index) == Some(&b'"') {
            in_identifier = true;
            index += 1;
            continue;
        }
        return Some((current, byte));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_split_preserves_escaped_identifier_separators() {
        assert_eq!(
            split_dotted(r#"helper.@"Type.With-Dash".run"#),
            ["helper", r#"@"Type.With-Dash""#, "run"]
        );
        assert_eq!(split_dotted(r#"@"work.now""#), [r#"@"work.now""#]);
    }

    #[test]
    fn qualified_split_preserves_escaped_identifier_separators() {
        let qn = r#"helper.zig::@"Type::With-Dash"::run"#;
        assert_eq!(
            split_qualified(qn),
            ["helper.zig", r#"@"Type::With-Dash""#, "run"]
        );
        assert_eq!(terminal_qualified(qn), "run");
        assert_eq!(
            terminal_qualified(r#"helper.zig::@"work::later""#),
            r#"@"work::later""#
        );
    }

    #[test]
    fn escaped_quotes_do_not_expose_inner_separators() {
        let qn = r#"helper.zig::@"say\"hi::now""#;
        assert_eq!(split_qualified(qn), ["helper.zig", r#"@"say\"hi::now""#]);
    }

    #[test]
    fn single_colon_ignores_qualified_and_escaped_colons() {
        assert_eq!(
            first_single_colon(r#"src/helper.zig:@"work::later""#),
            Some("src/helper.zig".len())
        );
        assert_eq!(
            first_single_colon(r#"src/helper.zig::@"work::later""#),
            None
        );
        assert_eq!(first_single_colon(r#"@"work:later""#), None);
    }

    #[test]
    fn qualified_to_dotted_changes_only_real_separators() {
        assert_eq!(
            qualified_to_dotted(r#"@"Type::With-Dash"::run"#),
            r#"@"Type::With-Dash".run"#
        );
    }

    #[test]
    fn receiver_normalization_preserves_escaped_punctuation() {
        assert_eq!(
            namespace_to_qualified(r#"pkg.@"Type.With-Dash".Nested"#),
            r#"pkg::@"Type.With-Dash"::Nested"#
        );
        assert_eq!(
            namespace_to_qualified(r#"Pkg\@"Type\\With-Dash""#),
            r#"Pkg::@"Type\\With-Dash""#
        );
    }

    #[test]
    fn final_mixed_separator_ignores_escaped_punctuation() {
        assert_eq!(
            last_unquoted_separator(
                r#"pkg.@"Type.With-Dash".@"work::later""#,
                &["::", "\\", "->", "."]
            ),
            Some((r#"pkg.@"Type.With-Dash""#.len(), 1))
        );
        assert_eq!(
            last_unquoted_separator(r#"@"work::later""#, &["::", "\\", "->", "."]),
            None
        );
    }

    #[test]
    fn mixed_split_and_identifier_validation_preserve_zig_names() {
        assert_eq!(
            split_unquoted_separators(
                r#"pkg.@"Type.With-Dash"::@"work::later""#,
                &["::", "\\", "->", "."]
            ),
            ["pkg", r#"@"Type.With-Dash""#, r#"@"work::later""#]
        );
        assert!(is_zig_identifier(r#"@"work::later""#));
        assert!(!is_zig_identifier(r#"@"unterminated"#));
    }
}
