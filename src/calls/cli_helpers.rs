//! Shared helpers used by both `cli.rs` (one-shot CLI) and `mcp.rs`
//! (long-lived MCP server). Pulled out so the two callers can stay thin
//! while the symbol-resolution logic has one home.

use crate::calls::graph::{CallGraph, Qn};
use crate::symbol_path::{first_single_colon, split_dotted, split_qualified};
use std::collections::BTreeSet;

/// What a resolved target qn refers to. Lets the caller dispatch to either
/// the call-edge path (`Callable`) or the type-aware path (`Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// qn is a function/method/constructor — present in `forward`/`reverse`.
    Callable,
    /// qn is a class/struct/trait/interface/enum/record — present in
    /// `types` / `type_by_name` but not in the call edges.
    Type,
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub qn: Qn,
    pub kind: SymbolKind,
}

/// Suffix-match `target` against every qn in the graph, mirroring the
/// `show`/`implements` mental model. Accepts:
/// - bare name: `TakeDamage` → matches `*::TakeDamage`.
/// - dotted path: `Player.TakeDamage` → matches `*::Player::TakeDamage`.
/// - explicit qn: `src/Player.cs::Player::TakeDamage` → exact.
/// - file-scoped: `src/Player.cs:TakeDamage` (single `:`) → restricts the
///   match to qns whose file equals or ends with that path. Useful when the
///   same name lives in multiple files. The symbol part still supports the
///   dotted form (`src/Player.cs:Player.TakeDamage`).
///
/// Returns *only* callable qns. For the kind-aware variant that also
/// surfaces types, use [`resolve_target_full`].
pub fn resolve_target_qns(calls: &CallGraph, target: &str) -> Vec<Qn> {
    resolve_target_full(calls, target)
        .into_iter()
        .filter(|r| r.kind == SymbolKind::Callable)
        .map(|r| r.qn)
        .collect()
}

/// Kind-aware resolution: returns every matching qn paired with whether
/// it's a callable or a type. Both halves are searched and deduped together.
pub fn resolve_target_full(calls: &CallGraph, target: &str) -> Vec<ResolvedTarget> {
    let (file_filter, symbol) = split_file_filter(target);
    let parts = split_dotted(symbol);

    let mut out: Vec<ResolvedTarget> = Vec::new();
    for qn in collect_callable_qns(calls) {
        if let Some(filter) = file_filter {
            if !file_matches(&qn, filter) {
                continue;
            }
        }
        if qn_matches(&qn, symbol, &parts) {
            out.push(ResolvedTarget {
                qn,
                kind: SymbolKind::Callable,
            });
        }
    }
    for qn in calls.types.keys() {
        if let Some(filter) = file_filter {
            if !file_matches(qn, filter) {
                continue;
            }
        }
        if qn_matches(qn, symbol, &parts) {
            out.push(ResolvedTarget {
                qn: qn.clone(),
                kind: SymbolKind::Type,
            });
        }
    }
    out.sort_by(|a, b| a.qn.0.cmp(&b.qn.0));
    out.dedup_by(|a, b| a.qn == b.qn && a.kind == b.kind);
    out
}

/// `src/foo.rs:bar` → `(Some("src/foo.rs"), "bar")`. Plain symbol → `(None, target)`.
/// `::`-only inputs (already-qualified qns) stay intact.
fn split_file_filter(target: &str) -> (Option<&str>, &str) {
    if let Some(index) = first_single_colon(target) {
        let (file, rest) = (&target[..index], &target[index + 1..]);
        if !file.is_empty() && !rest.is_empty() {
            return (Some(file), rest);
        }
    }
    (None, target)
}

fn file_matches(qn: &Qn, filter: &str) -> bool {
    let f = qn.file();
    if f == filter {
        return true;
    }
    // Trailing-path match — `Player.cs` matches `src/game/Player.cs`.
    f.ends_with(filter)
        && f.as_bytes()
            .get(f.len().saturating_sub(filter.len()).saturating_sub(1))
            == Some(&b'/')
}

fn collect_callable_qns(calls: &CallGraph) -> Vec<Qn> {
    let mut s: BTreeSet<Qn> = BTreeSet::new();
    for k in calls.forward.keys() {
        s.insert(k.clone());
    }
    for k in calls.reverse.keys() {
        s.insert(k.clone());
    }
    s.into_iter().collect()
}

fn qn_matches(qn: &Qn, raw: &str, parts: &[&str]) -> bool {
    if qn.0 == raw {
        return true;
    }
    let segments = split_qualified(&qn.0);
    if parts.len() > segments.len() {
        return false;
    }
    let start = segments.len() - parts.len();
    parts.iter().enumerate().all(|(i, p)| {
        if qn.file().ends_with(".zig") {
            crate::zig_syntax::identifier(segments[start + i]) == crate::zig_syntax::identifier(p)
        } else {
            segments[start + i] == *p
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_file_filter_plain_symbol() {
        assert_eq!(split_file_filter("greet"), (None, "greet"));
    }

    #[test]
    fn split_file_filter_with_file() {
        assert_eq!(
            split_file_filter("src/main.rs:parse_file"),
            (Some("src/main.rs"), "parse_file")
        );
    }

    #[test]
    fn split_file_filter_double_colon_stays_intact() {
        assert_eq!(
            split_file_filter("src/main.rs::parse_file"),
            (None, "src/main.rs::parse_file")
        );
    }

    #[test]
    fn split_file_filter_with_dotted_symbol() {
        assert_eq!(
            split_file_filter("src/foo.rs:Foo.bar"),
            (Some("src/foo.rs"), "Foo.bar")
        );
    }

    #[test]
    fn split_file_filter_ignores_colons_in_escaped_identifier() {
        assert_eq!(
            split_file_filter(r#"@"work::later""#),
            (None, r#"@"work::later""#)
        );
        assert_eq!(
            split_file_filter(r#"src/helper.zig:@"work::later""#),
            (Some("src/helper.zig"), r#"@"work::later""#)
        );
    }

    #[test]
    fn qn_match_preserves_escaped_dots_and_colons() {
        let qn = Qn::new(r#"helper.zig::@"Type.With-Dash"::@"work::later""#);
        let dotted = r#"@"Type.With-Dash".@"work::later""#;
        let parts = split_dotted(dotted);
        assert!(qn_matches(&qn, dotted, &parts));
    }
}
