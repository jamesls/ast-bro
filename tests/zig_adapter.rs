//! End-to-end coverage for the direct tree-sitter Zig adapter.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

const FIXTURE: &str = "tests/fixtures/zig_adapter/sample.zig";
const STALE_GRAMMAR_FIXTURE: &str = "tests/fixtures/zig_adapter/stale_grammar.zig";

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ast-bro"))
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("run ast-bro")
}

fn run_success(args: &[&str]) -> String {
    let output = run(args);
    assert!(
        output.status.success(),
        "exit non-zero:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 stdout")
}

fn map_json(path: &str) -> Value {
    let stdout = run_success(&["map", path, "--json", "--compact"]);
    serde_json::from_str(stdout.trim()).expect("valid map JSON")
}

fn file_declarations(value: &Value) -> &[Value] {
    value["files"][0]["declarations"]
        .as_array()
        .expect("declarations array")
}

fn find_decl<'a>(declarations: &'a [Value], name: &str) -> Option<&'a Value> {
    for declaration in declarations {
        if declaration["name"].as_str() == Some(name) {
            return Some(declaration);
        }
        if let Some(children) = declaration["children"].as_array() {
            if let Some(found) = find_decl(children, name) {
                return Some(found);
            }
        }
    }
    None
}

#[test]
fn container_kinds_and_native_kinds_are_preserved() {
    let value = map_json(FIXTURE);
    let declarations = file_declarations(&value);
    for (name, kind, native_kind, signature) in [
        ("Widget", "struct", "struct", "pub const Widget = struct"),
        ("Mode", "enum", "enum", "pub const Mode = enum"),
        ("Value", "struct", "union", "pub const Value = union(enum)"),
        ("Handle", "struct", "opaque", "pub const Handle = opaque"),
        ("Failure", "enum", "error set", "pub const Failure = error"),
    ] {
        let declaration = find_decl(declarations, name).expect("type declaration");
        assert_eq!(declaration["kind"], kind, "wrong kind for {name}");
        assert_eq!(
            declaration["native_kind"], native_kind,
            "wrong native kind for {name}"
        );
        assert_eq!(
            declaration["signature"], signature,
            "wrong signature for {name}"
        );
    }

    let digest = run_success(&["digest", FIXTURE]);
    for expected in [
        "struct Widget",
        "enum Mode",
        "union Value",
        "opaque Handle",
        "error set Failure",
    ] {
        assert!(digest.contains(expected), "{expected} missing:\n{digest}");
    }
}

#[test]
fn members_nest_under_their_container() {
    let value = map_json(FIXTURE);
    let declarations = file_declarations(&value);
    let widget = find_decl(declarations, "Widget").expect("Widget declaration");
    let children = widget["children"].as_array().expect("Widget children");

    assert_eq!(
        find_decl(children, "count").expect("count")["kind"],
        "field"
    );
    assert_eq!(find_decl(children, "init").expect("init")["kind"], "method");
    assert_eq!(
        find_decl(children, "reset").expect("reset")["kind"],
        "method"
    );
    assert_eq!(
        find_decl(children, "widget local").expect("local test")["native_kind"],
        "test"
    );
}

#[test]
fn pub_token_controls_visibility_and_modifiers() {
    let value = map_json(FIXTURE);
    let declarations = file_declarations(&value);

    for (name, visibility) in [
        ("Widget", "public"),
        ("Hidden", "private"),
        ("init", "public"),
        ("reset", "private"),
        ("active", "private"),
    ] {
        assert_eq!(
            find_decl(declarations, name).expect("declaration")["visibility"],
            visibility,
            "wrong visibility for {name}"
        );
    }

    assert_eq!(
        find_decl(declarations, "init").expect("init")["modifiers"],
        serde_json::json!(["inline"])
    );
    assert_eq!(
        find_decl(declarations, "launch").expect("launch")["modifiers"],
        serde_json::json!(["export"])
    );
    assert_eq!(
        find_decl(declarations, "active").expect("active")["modifiers"],
        serde_json::json!(["threadlocal"])
    );

    let public_only = run_success(&["map", FIXTURE, "--no-private"]);
    assert!(public_only.contains("pub const Widget"), "{public_only}");
    assert!(!public_only.contains("const Hidden"), "{public_only}");
    assert!(!public_only.contains("fn reset"), "{public_only}");
}

#[test]
fn doc_comments_and_tests_render() {
    let map = run_success(&["map", FIXTURE]);
    assert!(map.contains("/// A public widget."), "{map}");
    assert!(map.contains("/// Builds a widget."), "{map}");
    assert!(map.contains("test \"widget local\""), "{map}");
    assert!(map.contains("test \"launch works\""), "{map}");
    assert!(
        !map.contains("This ordinary comment is not documentation"),
        "{map}"
    );
    assert!(!map.contains("Detached documentation"), "{map}");
}

#[test]
fn json_identifies_zig_and_clean_fixture_has_no_errors() {
    let value = map_json(FIXTURE);
    assert_eq!(value["schema"], "ast-bro.map.v1");
    assert_eq!(value["files"][0]["language"], "zig");
    assert_eq!(value["files"][0]["error_count"], 0);
}

#[test]
fn show_and_implements_use_the_shared_zig_routing() {
    let shown = run_success(&["show", FIXTURE, "Widget.init"]);
    assert!(shown.contains("pub inline fn init"), "{shown}");
    assert!(shown.contains("return Widget"), "{shown}");

    let implements = run_success(&["implements", "Widget", FIXTURE]);
    assert!(implements.contains("0 match(es)"), "{implements}");
}

#[test]
fn stale_grammar_errors_do_not_hide_surrounding_declarations() {
    let value = map_json(STALE_GRAMMAR_FIXTURE);
    let file = &value["files"][0];
    assert!(
        file["error_count"].as_u64().is_some_and(|count| count > 0),
        "the known Zig 0.15 asm gap should be reported: {value}"
    );
    let declarations = file_declarations(&value);
    for name in ["before", "stale_asm", "after"] {
        assert!(
            find_decl(declarations, name).is_some(),
            "{name} missing after recovery: {value}"
        );
    }
}
