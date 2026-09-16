//! Regressions for current Zig syntax and statically resolvable language forms.

use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn project() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();
    for entry in fs::read_dir("tests/fixtures/zig_016").unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, temp.path().join(path.file_name().unwrap())).unwrap();
    }
    temp
}

fn run(root: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_ast-bro"))
        .current_dir(root)
        .args(args)
        .args(["--json", "--compact"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn targets(root: &Path, symbol: &str) -> Vec<String> {
    let doc = run(root, &["callees", symbol, "."]);
    doc["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            assert_eq!(edge["confidence"], "Exact", "{symbol}: {doc}");
            edge["target"].as_str().unwrap().to_owned()
        })
        .collect()
}

#[test]
fn zig_016_grammar_preserves_declaration_boundaries() {
    let temp = project();
    let doc = run(temp.path(), &["map", "main.zig"]);
    assert_eq!(doc["files"][0]["error_count"], 0, "{doc}");
    let declarations = doc["files"][0]["declarations"].as_array().unwrap();
    let rule = declarations.iter().find(|d| d["name"] == "Rule").unwrap();
    assert_eq!(rule["start_line"], rule["end_line"]);
    assert!(declarations.iter().any(|d| d["name"] == "fence"));
    assert!(declarations.iter().any(|d| d["name"] == "errorParameter"));
}

#[test]
fn zig_016_calls_follow_types_aliases_and_generic_facades() {
    let temp = project();
    for (symbol, expected) in [
        ("main.zig:inferredInit", vec!["main.zig::Box::init"]),
        (
            "main.zig:typedInit",
            vec!["main.zig::Box::init", "main.zig::Box::ping"],
        ),
        (
            "main.zig:fromInit",
            vec!["main.zig::Box::init", "main.zig::Box::ping"],
        ),
        ("Widget.zig:init", vec!["Widget.zig::reset"]),
        ("main.zig:Nested.init", vec!["main.zig::Nested::ping"]),
        ("main.zig:Nested.ping", vec!["alternate.zig::work"]),
        (
            "main.zig:ordinaryNamespaces",
            vec!["main.zig::crate::work", "main.zig::super::work"],
        ),
        (
            "main.zig:escapedCalls",
            vec![
                "main.zig::normal",
                "main.zig::quoted",
                "helper.zig::quoted",
                "helper.zig::quoted",
            ],
        ),
    ] {
        assert_eq!(targets(temp.path(), symbol), expected, "{symbol}");
    }
    let generic = targets(temp.path(), "generic.zig:useGeneric");
    assert!(generic[0].ends_with("::init"));
    assert!(generic[1].ends_with("::read"));
    assert_eq!(targets(temp.path(), "main.zig:throughFacade"), generic);
    assert_eq!(
        targets(temp.path(), "main.zig:aliases"),
        [
            "helper.zig::work",
            "main.zig::normal",
            "main.zig::normal",
            "helper.zig::work"
        ]
    );
    let comptime = run(temp.path(), &["callers", "helper.zig:work", "."]);
    assert!(
        comptime["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"]
                .as_str()
                .unwrap()
                .contains("::aliases::comptime@")),
        "{comptime}"
    );
}

#[test]
fn zig_016_sibling_scopes_do_not_depend_on_line_breaks() {
    let temp = project();
    fs::write(temp.path().join("scopes.zig"), "pub fn go() void { { const lib = @import(\"helper.zig\"); lib.work(); } { const lib = @import(\"alternate.zig\"); lib.work(); } }\n").unwrap();
    assert_eq!(
        targets(temp.path(), "scopes.zig:go"),
        ["helper.zig::work", "alternate.zig::work"]
    );
    fs::write(temp.path().join("scopes.zig"), "pub fn go() void { { const lib = @import(\"alternate.zig\"); lib.work(); } { const lib = @import(\"helper.zig\"); lib.work(); } }\n").unwrap();
    let warm = run(temp.path(), &["callees", "scopes.zig:go", "."]);
    assert_eq!(
        warm,
        run(temp.path(), &["callees", "scopes.zig:go", ".", "--rebuild"])
    );
    assert_eq!(
        targets(temp.path(), "scopes.zig:go"),
        ["alternate.zig::work", "helper.zig::work"]
    );
    fs::write(temp.path().join("types.zig"), "const A = struct { pub fn ping(_: A) void {} }; const B = struct { pub fn ping(_: B) void {} }; pub fn go() void { { const v: A = .{}; v.ping(); } { const v: B = .{}; v.ping(); } }\n").unwrap();
    assert_eq!(
        targets(temp.path(), "types.zig:go"),
        ["types.zig::A::ping", "types.zig::B::ping"]
    );
}

#[test]
fn zig_016_inline_tests_have_separate_identity_and_survive_cache_reload() {
    let temp = project();
    assert_eq!(
        targets(temp.path(), "main.zig:doctested"),
        ["main.zig::normal"]
    );
    let tests = run(temp.path(), &["callers", "main.zig:normal", ".", "--tests"]);
    let hits = tests["matches"].as_array().unwrap();
    assert!(!hits.is_empty(), "{tests}");
    assert!(
        hits.iter()
            .all(|e| e["source"].as_str().unwrap().contains("::test@")),
        "{tests}"
    );
    assert_eq!(
        tests,
        run(temp.path(), &["callers", "main.zig:normal", ".", "--tests"])
    );
    let production = run(
        temp.path(),
        &["callers", "main.zig:normal", ".", "--exclude-tests"],
    );
    assert!(production["matches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| !e["source"].as_str().unwrap().contains("::test@")));
    let impact = run(
        temp.path(),
        &["impact", "main.zig:normal", ".", "--mode", "tests"],
    );
    assert!(impact.to_string().contains("::test@"), "{impact}");
}

#[test]
fn zig_016_deps_are_syntax_based_and_include_zon_and_build_sources() {
    let temp = project();
    fs::write(temp.path().join("settings.zon"), ".{ .enabled = true }\n").unwrap();
    fs::create_dir_all(temp.path().join("src/build")).unwrap();
    fs::write(
        temp.path().join("src/build/options.zig"),
        "const settings = @import(\"../../settings.zon\"); pub fn options() void {}\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("build.zig"),
        "const data = @import(\"settings.zon\");\n",
    )
    .unwrap();
    let map = run(temp.path(), &["map", "."]);
    assert!(map.to_string().contains("src/build/options.zig"), "{map}");
    let source_map = run(temp.path(), &["map", "src"]);
    assert!(
        source_map.to_string().contains("src/build/options.zig"),
        "{source_map}"
    );
    let deps = run(temp.path(), &["deps", "main.zig"]);
    assert!(deps.to_string().contains("helper.zig"), "{deps}");
    assert!(!deps.to_string().contains("missing.zig"), "{deps}");
    let zon = run(temp.path(), &["deps", "build.zig"]);
    assert!(zon.to_string().contains("settings.zon"), "{zon}");
    assert!(!zon.to_string().contains("\"external\":true"), "{zon}");
}

#[test]
fn zig_016_surface_expands_parentheses_and_generic_types() {
    let temp = project();
    let doc = run(temp.path(), &["surface", "facade.zig"]);
    let text = doc.to_string();
    assert!(text.contains("Box.init"), "{doc}");
    assert!(text.contains("Box.read"), "{doc}");
}

#[test]
fn zig_016_conditional_facades_keep_candidates_ambiguous() {
    let temp = project();
    fs::write(temp.path().join("choice.zig"), r#"
const builtin = @import("builtin");
pub const Backend = if (builtin.os.tag == .linux) @import("helper.zig") else @import("alternate.zig");
pub const Selected = switch (builtin.os.tag) { .linux => @import("helper.zig"), else => @import("alternate.zig") };
pub fn go() void { Backend.work(); Selected.work(); }
"#).unwrap();
    let doc = run(temp.path(), &["callees", "choice.zig:go", "."]);
    for edge in doc["matches"].as_array().unwrap() {
        assert_eq!(edge["confidence"], "Ambiguous", "{doc}");
        assert_eq!(edge["candidates"].as_array().unwrap().len(), 2, "{doc}");
    }
    let surface = run(temp.path(), &["surface", "choice.zig", "--include-chain"]);
    let text = surface.to_string();
    assert!(text.contains("Backend.work"), "{surface}");
    assert!(text.contains("Selected.work"), "{surface}");
    assert!(text.contains("helper.zig"), "{surface}");
    assert!(text.contains("alternate.zig"), "{surface}");
    assert!(text.contains("\"conditional\":true"), "{surface}");
    let shallow = run(temp.path(), &["surface", "choice.zig", "--max-depth", "0"]);
    assert!(!shallow.to_string().contains("Backend.work"), "{shallow}");
}

#[test]
fn zig_016_literal_build_wiring_and_custom_roots_are_discovered() {
    let temp = project();
    fs::write(
        temp.path().join("build.zig"),
        r#"
const std = @import("std");
pub fn build(b: *std.Build) void {
    const helpers = b.createModule(.{ .root_source_file = b.path("helper.zig") });
    const app = b.addModule("app", .{ .root_source_file = b.path("custom.zig") });
    app.addImport("helpers", helpers);
}
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("custom.zig"),
        "const helpers = @import(\"helpers\"); pub fn go() void { helpers.work(); }\n",
    )
    .unwrap();
    assert_eq!(targets(temp.path(), "custom.zig:go"), ["helper.zig::work"]);
    let deps = run(temp.path(), &["deps", "custom.zig"]);
    assert!(deps.to_string().contains("helper.zig"), "{deps}");
    let build = fs::read_to_string(temp.path().join("build.zig"))
        .unwrap()
        .replace("helper.zig", "alternate.zig");
    fs::write(temp.path().join("build.zig"), build).unwrap();
    let warm = run(temp.path(), &["deps", "custom.zig"]);
    assert!(warm.to_string().contains("alternate.zig"), "{warm}");
    assert_eq!(warm, run(temp.path(), &["deps", "custom.zig", "--rebuild"]));
    assert_eq!(
        targets(temp.path(), "custom.zig:go"),
        ["alternate.zig::work"]
    );
    fs::write(temp.path().join("build.zig"), "pub fn build(b: anytype) void { _ = b.addModule(\"app\", .{ .root_source_file = b.path(\"custom.zig\") }); }\n").unwrap();
    let surface = run(temp.path(), &["surface", "."]);
    assert!(surface.to_string().contains("custom.zig"), "{surface}");
}

#[test]
fn zig_016_structural_search_and_rewrite_preserve_literals() {
    let temp = project();
    let file = temp.path().join("rewrite.zig");
    let source = "pub fn go() void { before(1, 2); before(3); }\nconst literal = \"$VALUE\";\n";
    fs::write(&file, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ast-bro"))
        .current_dir(temp.path())
        .args([
            "run",
            "rewrite.zig",
            "-p",
            "before($$$ARGS)",
            "-r",
            "after($$$ARGS)",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("after(1, 2)"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        source,
        "dry runs must preserve source"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ast-bro"))
        .current_dir(temp.path())
        .args([
            "run",
            "rewrite.zig",
            "-p",
            "before($$$ARGS)",
            "-r",
            "after($$$ARGS)",
            "--write",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rewritten = fs::read_to_string(file).unwrap();
    assert!(rewritten.contains("after(1, 2)"), "{rewritten}");
    let literal = run(
        temp.path(),
        &[
            "run",
            "rewrite.zig",
            "-p",
            "const literal = \"$VALUE\";",
            "--lang",
            "zig",
        ],
    );
    assert!(literal.to_string().contains("$VALUE"), "{literal}");
}

#[test]
fn zig_016_result_locations_and_unknown_callbacks_do_not_bind_decoys() {
    let temp = project();
    fs::write(temp.path().join("result.zig"), r#"
const Box = struct { pub fn init() Box { return .{}; } };
const Container = struct { box: Box };
fn consume(_: Box) void {}
pub fn go() void { const result: Container = .{ .box = .init() }; _ = result; consume(.init()); }
fn callback() void {}
pub fn indirect(callback_arg: *const fn() void) void { callback_arg(); }
const Inner = struct { pub fn ping(_: Inner) void {} };
const Outer = struct { inner: Inner };
pub fn members(outer: Outer, maybe: ?Inner) void { outer.inner.ping(); if (maybe) |inner| inner.ping(); }
const Callback = struct { callback: *const fn() void };
pub fn knownCallback() void { const holder: Callback = .{ .callback = callback }; holder.callback(); }
pub fn selected(index: usize) void { const functions = [_]*const fn() void{callback}; functions[index](); }
"#).unwrap();
    assert_eq!(
        targets(temp.path(), "result.zig:go"),
        [
            "result.zig::Box::init",
            "result.zig::consume",
            "result.zig::Box::init"
        ]
    );
    let indirect = run(temp.path(), &["callees", "result.zig:indirect", "."]);
    assert_eq!(
        indirect["matches"][0]["confidence"], "Ambiguous",
        "{indirect}"
    );
    assert_eq!(
        targets(temp.path(), "result.zig:members"),
        ["result.zig::Inner::ping", "result.zig::Inner::ping"]
    );
    assert_eq!(
        targets(temp.path(), "result.zig:knownCallback"),
        ["result.zig::callback"]
    );
    let selected = run(temp.path(), &["callees", "result.zig:selected", "."]);
    assert!(
        selected["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["target"]
                .as_str()
                .unwrap()
                .contains("functions[index]")),
        "{selected}"
    );
}

#[test]
fn zig_016_escaped_names_and_import_strings_use_decoded_values() {
    let temp = project();
    fs::write(
        temp.path().join("escapes.zig"),
        r#"
const h = // An initializer may start after a comment.
    @import("h\x65lper.zig");
pub fn @"na\x6de"() void { h.@"quo\u{74}ed"(); }
pub fn caller() void { name(); }
"#,
    )
    .unwrap();
    assert_eq!(
        targets(temp.path(), "escapes.zig:caller"),
        ["escapes.zig::name"]
    );
    assert_eq!(
        targets(temp.path(), "escapes.zig:@\"name\""),
        ["helper.zig::quoted"]
    );
    let shown = run(temp.path(), &["show", "escapes.zig", "name"]);
    assert!(
        shown["files"][0]["matches"][0]["source"]
            .as_str()
            .unwrap()
            .contains(r#"@"na\x6de""#),
        "{shown}"
    );
    let mapped = run(temp.path(), &["map", "escapes.zig"]);
    assert!(
        mapped["files"][0]["declarations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|declaration| declaration["name"] == r#"@"na\x6de""#),
        "{mapped}"
    );
    let deps = run(temp.path(), &["deps", "escapes.zig"]);
    assert!(deps.to_string().contains("helper.zig"), "{deps}");
}
