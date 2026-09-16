//! End-to-end coverage for the direct tree-sitter Zig adapter.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

const FIXTURE: &str = "tests/fixtures/zig_adapter/sample.zig";
const ADVANCED_FIXTURE: &str = "tests/fixtures/zig_adapter/advanced.zig";
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
        children
            .iter()
            .find(|declaration| declaration["signature"] == "test \"widget local\"")
            .expect("local test")["native_kind"],
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
        ("count", ""),
        ("idle", ""),
        ("BadInput", ""),
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
    assert!(public_only.contains("count: usize"), "{public_only}");
    assert!(public_only.contains("idle"), "{public_only}");
    assert!(public_only.contains("BadInput"), "{public_only}");
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
fn digest_omits_tests_even_when_private_declarations_are_included() {
    let digest = run_success(&["digest", "--include-private", FIXTURE]);

    assert!(!digest.contains("widget local"), "{digest}");
    assert!(!digest.contains("launch works"), "{digest}");
    assert!(digest.contains("5 methods"), "{digest}");
    assert!(digest.contains("reset()"), "{digest}");
}

#[test]
fn test_only_digest_has_no_callable_legend_or_test_local_counts() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = directory.path().join("tests_only.zig");
    std::fs::write(
        &fixture,
        "test \"not a callable\" {\n    const Local = struct {\n        value: u8,\n        fn helper() void {}\n    };\n    _ = Local{ .value = 0 };\n}\n",
    )
    .expect("write Zig fixture");

    let digest = run_success(&[
        "digest",
        "--include-private",
        fixture.to_str().expect("UTF-8 fixture path"),
    ]);

    assert!(!digest.contains("# legend:"), "{digest}");
    assert!(!digest.contains("not a callable"), "{digest}");
    assert!(digest.contains("# no declarations"), "{digest}");
    assert!(!digest.contains(" types"), "{digest}");
    assert!(!digest.contains(" methods"), "{digest}");
    assert!(!digest.contains(" fields"), "{digest}");
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
fn current_grammar_accepts_assembly_and_preserves_surrounding_declarations() {
    let value = map_json(STALE_GRAMMAR_FIXTURE);
    let file = &value["files"][0];
    assert_eq!(file["error_count"], 0, "{value}");
    let declarations = file_declarations(&value);
    for name in ["before", "stale_asm", "after"] {
        assert!(
            find_decl(declarations, name).is_some(),
            "{name} missing after recovery: {value}"
        );
    }
}

#[test]
fn qualifiers_and_container_modifiers_are_preserved() {
    let value = map_json(ADVANCED_FIXTURE);
    let declarations = file_declarations(&value);

    for (name, attrs) in [
        (
            "shared_state",
            serde_json::json!([
                "align(16)",
                "addrspace(.generic)",
                "linksection(\".shared\")"
            ]),
        ),
        ("Callback", serde_json::json!(["callconv(.c)"])),
        (
            "qualified",
            serde_json::json!([
                "align(16)",
                "addrspace(.generic)",
                "linksection(\".text.qualified\")",
                "callconv(.c)"
            ]),
        ),
        ("bits", serde_json::json!(["align(1)"])),
        ("ptr", serde_json::json!(["align(4096)"])),
    ] {
        assert_eq!(
            find_decl(declarations, name).expect("qualified declaration")["attrs"],
            attrs,
            "wrong qualifiers for {name}"
        );
    }

    for (name, modifier) in [("PackedHeader", "packed"), ("ExternHeader", "extern")] {
        let declaration = find_decl(declarations, name).expect("container declaration");
        assert_eq!(declaration["native_kind"], "struct");
        assert_eq!(declaration["modifiers"], serde_json::json!([modifier]));
    }
    assert_eq!(
        find_decl(declarations, "qualified").expect("qualified function")["modifiers"],
        serde_json::json!(["extern"])
    );

    let abi_entry = find_decl(declarations, "abiEntry").expect("exported ABI entry point");
    assert_eq!(abi_entry["visibility"], "public");
    assert_eq!(abi_entry["modifiers"], serde_json::json!(["export"]));
    assert!(
        abi_entry.get("attrs").is_none(),
        "parameter alignment must not become a function attribute: {abi_entry}"
    );
    let probe_fn = find_decl(declarations, "probe_fn").expect("function pointer field");
    assert!(
        probe_fn.get("attrs").is_none(),
        "parameter alignment must not become a field attribute: {probe_fn}"
    );
}

#[test]
fn comptime_and_usingnamespace_declarations_are_surfaced() {
    let value = map_json(ADVANCED_FIXTURE);
    let declarations = file_declarations(&value);

    let imported = find_decl(declarations, "@import(\"mixin.zig\")")
        .expect("public usingnamespace declaration");
    assert_eq!(imported["kind"], "field");
    assert_eq!(imported["native_kind"], "usingnamespace");
    assert_eq!(imported["visibility"], "public");

    let named = find_decl(declarations, "Helpers").expect("named usingnamespace declaration");
    assert_eq!(named["kind"], "field");
    assert_eq!(named["native_kind"], "usingnamespace");

    let comptime = find_decl(declarations, "comptime@L17C1").expect("top-level comptime block");
    assert_eq!(comptime["kind"], "function");
    assert_eq!(comptime["native_kind"], "comptime");
    assert_eq!(comptime["modifiers"], serde_json::json!(["comptime"]));
    assert_eq!(comptime["calls"][0]["name"], "registerTypes");
    assert!(
        find_decl(
            comptime["children"].as_array().expect("comptime children"),
            "generatedHook"
        )
        .is_some(),
        "named containers inside comptime blocks should remain navigable"
    );
}

#[test]
fn local_callbacks_and_builtins_keep_their_call_owners() {
    let value = map_json(ADVANCED_FIXTURE);
    let declarations = file_declarations(&value);
    let owner = find_decl(declarations, "owner").expect("owner function");
    let owner_calls: Vec<_> = owner["calls"]
        .as_array()
        .expect("owner calls")
        .iter()
        .map(|call| call["name"].as_str().expect("call name"))
        .collect();
    assert_eq!(
        owner_calls,
        ["beforeLocal", "@memcpy", "consume", "afterLocal"]
    );
    assert!(
        !owner_calls.contains(&"@import"),
        "@import is dependency syntax, not a runtime call"
    );

    let callback = find_decl(
        owner["children"].as_array().expect("owner children"),
        "callback",
    )
    .expect("named local callback");
    let callback_calls: Vec<_> = callback["calls"]
        .as_array()
        .expect("callback calls")
        .iter()
        .map(|call| call["name"].as_str().expect("call name"))
        .collect();
    assert_eq!(callback_calls, ["callbackOnly", "@branchHint"]);

    let anonymous =
        find_decl(declarations, "anonymous_struct@L44C13").expect("anonymous callback container");
    assert_eq!(anonymous["kind"], "struct");
    assert_eq!(
        find_decl(
            anonymous["children"]
                .as_array()
                .expect("anonymous container children"),
            "lessThan"
        )
        .expect("anonymous callback method")["calls"][0]["name"],
        "comparatorOnly"
    );

    let nested_comptime = find_decl(declarations, "comptime@L51C5").expect("nested comptime block");
    assert_eq!(nested_comptime["calls"][0]["name"], "compileOnly");
}

#[test]
fn inner_container_docs_use_inside_placement_without_conflating_outer_docs() {
    let value = map_json(ADVANCED_FIXTURE);
    let declarations = file_declarations(&value);

    let documented = find_decl(declarations, "Documented").expect("documented container");
    assert_eq!(
        documented["docs"],
        serde_json::json!(["//! Documentation for the enclosing container."])
    );
    assert_eq!(documented["docs_inside"], true);

    let both = find_decl(declarations, "BothDocs").expect("container with both doc positions");
    assert_eq!(
        both["docs"],
        serde_json::json!([
            "/// Outer documentation wins when the IR cannot preserve both positions."
        ])
    );
    assert_eq!(both["docs_inside"], false);
}

#[test]
fn typed_anonymous_containers_preserve_all_nested_fields() {
    let value = map_json(ADVANCED_FIXTURE);
    let declarations = file_declarations(&value);

    let typed = find_decl(declarations, "typed_value").expect("typed anonymous struct value");
    assert_eq!(typed["kind"], "field");
    let typed_children = typed["children"].as_array().expect("typed struct fields");
    assert!(find_decl(typed_children, "typed_one").is_some());
    assert!(find_decl(typed_children, "typed_two").is_some());

    let fulfill = find_decl(declarations, "fulfill").expect("union payload variant");
    let payload_fields = fulfill["children"]
        .as_array()
        .expect("anonymous payload fields");
    assert!(find_decl(payload_fields, "pending").is_some());
    assert!(find_decl(payload_fields, "buffer").is_some());

    let shapes = find_decl(declarations, "anonymousShapes").expect("anonymous shape owner");
    let shape_children = shapes["children"]
        .as_array()
        .expect("anonymous shape declarations");
    for name in ["size", "expected", "value", "pad"] {
        assert!(
            find_decl(shape_children, name).is_some(),
            "anonymous field {name} missing: {shapes}"
        );
    }
    let returned_shape = shape_children
        .iter()
        .find(|declaration| {
            declaration["children"]
                .as_array()
                .is_some_and(|children| find_decl(children, "value").is_some())
        })
        .expect("returned anonymous struct");
    assert!(
        returned_shape.get("attrs").is_none(),
        "a child field qualifier must not become a container attribute: {returned_shape}"
    );
    assert_eq!(
        find_decl(shape_children, "value").expect("aligned returned field")["attrs"],
        serde_json::json!(["align(8)"])
    );

    let cases = find_decl(declarations, "top_level_cases").expect("top-level case table");
    let case_children = cases["children"].as_array().expect("case table shape");
    assert!(find_decl(case_children, "name").is_some());
    assert!(find_decl(case_children, "expected").is_some());

    let nested = find_decl(declarations, "top_level_nested").expect("nested inferred shape");
    assert!(find_decl(
        nested["children"]
            .as_array()
            .expect("nested shape children"),
        "value"
    )
    .is_some());

    let combined = find_decl(declarations, "CombinedError").expect("inferred error union");
    let error_children = combined["children"].as_array().expect("error members");
    for name in ["Missing", "Invalid", "Other"] {
        assert!(
            error_children
                .iter()
                .any(|declaration| declaration["name"] == name),
            "inferred error member {name} must be direct: {combined}"
        );
    }
    assert!(
        error_children.iter().all(|declaration| {
            !declaration["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("anonymous_error_set@"))
        }),
        "inferred error unions must not gain a synthetic namespace: {combined}"
    );
}
