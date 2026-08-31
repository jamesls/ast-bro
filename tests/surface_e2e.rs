//! End-to-end smoke tests for `ast-bro surface`. These shell out
//! to the built binary so they exercise the same code path users hit.
//!
//! Fixtures live in `tests/fixtures/surface/<name>/`. Each test asserts
//! a small set of invariants on the output (presence/absence of names,
//! re-export chains, etc.) rather than full snapshots — snapshots are
//! brittle to colour/whitespace changes and these are quick to read.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    // CARGO_BIN_EXE_<bin name> is set by cargo for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_ast-bro"))
}

fn surface(args: &[&str]) -> String {
    let out = Command::new(bin())
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("run ast-bro");
    assert!(
        out.status.success(),
        "ast-bro surface failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

#[test]
fn rust_chained_glob_reexport() {
    let s = surface(&["surface", "tests/fixtures/surface/rust_chained"]);
    // `pub use net::client::*` should publish Client at the crate root.
    assert!(
        s.contains("rust_chained::Client"),
        "missing glob-reexported Client:\n{s}"
    );
    // Direct decls in lib.rs come through too.
    assert!(s.contains("rust_chained::Error"), "missing Error:\n{s}");
    // Canonical path through `pub mod net` chain.
    assert!(
        s.contains("rust_chained::net::client::Client"),
        "missing canonical Client path:\n{s}"
    );
    // Impl methods get lifted under the type.
    assert!(
        s.contains("rust_chained::net::client::Client::connect"),
        "missing impl method:\n{s}"
    );
    // Private helpers must not leak.
    assert!(!s.contains("private_helper"), "private leaked:\n{s}");
    assert!(!s.contains("_internal"), "_internal leaked:\n{s}");
}

#[test]
fn rust_rename_keeps_alias() {
    let s = surface(&["surface", "tests/fixtures/surface/rust_rename"]);
    assert!(s.contains("rust_rename::Quux"), "alias missing:\n{s}");
    // The original name `Bar` should NOT appear at the crate root —
    // it's not separately exported.
    assert!(
        !s.contains("rust_rename::Bar"),
        "unrenamed Bar leaked:\n{s}"
    );
}

#[test]
fn python_dunder_all_filters_imports() {
    let s = surface(&["surface", "tests/fixtures/surface/python_dunder"]);
    assert!(s.contains("python_dunder.Thing"));
    assert!(s.contains("python_dunder.help_me"));
    // `internal_too` is imported but NOT in __all__ — must be dropped.
    assert!(
        !s.contains("internal_too"),
        "internal_too leaked past __all__:\n{s}"
    );
    // `also_public` is defined in __init__.py but NOT in __all__ — drop.
    assert!(
        !s.contains("also_public"),
        "also_public leaked past __all__:\n{s}"
    );
}

#[test]
fn python_no_dunder_uses_underscore_convention() {
    let s = surface(&["surface", "tests/fixtures/surface/python_no_dunder"]);
    assert!(s.contains("python_no_dunder.public_fn"));
    assert!(s.contains("python_no_dunder.PublicClass"));
    assert!(
        !s.contains("_hidden"),
        "leading-underscore name leaked:\n{s}"
    );
    assert!(!s.contains("_HiddenClass"), "private class leaked:\n{s}");
}

#[test]
fn java_fallback_filters_visibility() {
    let s = surface(&[
        "surface",
        "tests/fixtures/surface/java_fallback",
        "--lang",
        "fallback",
    ]);
    assert!(s.contains("Greeter"), "public class missing:\n{s}");
    assert!(s.contains("greet"), "public method missing:\n{s}");
    assert!(!s.contains("internal"), "private method leaked:\n{s}");
}

#[test]
fn build_zig_root_wins_over_nested_cargo_manifest() {
    let tmp = tempfile::tempdir().expect("create temp directory");
    write(&tmp.path().join("build.zig"), "pub fn zigRoot() void {}\n");
    write(
        &tmp.path().join("nested/Cargo.toml"),
        "[package]\nname = \"nested\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &tmp.path().join("nested/src/lib.rs"),
        "pub fn rust_hijack() {}\n",
    );

    let root = tmp.path().to_str().expect("UTF-8 temp path");
    let output = surface(&["surface", root]);

    assert!(
        output.contains("zigRoot"),
        "root build.zig was hijacked by nested Cargo.toml:\n{output}"
    );
}

#[test]
fn zig_surface_follows_namespace_facade_and_usingnamespace_aliases() {
    let tmp = tempfile::tempdir().expect("create temp directory");
    write(
        &tmp.path().join("build.zig"),
        "pub fn buildOnly() void {}\n",
    );
    write(
        &tmp.path().join("src/main.zig"),
        r#"pub const direct = @import("direct.zig");
const facade = @import("facade.zig");
const facade_again = facade;
pub const Renamed = facade_again.layer.Source;
pub const FacadeInjected = facade_again.Injected;
pub const InlineRenamed = @import("deep.zig").Source;
pub const ParentNormalized = @import("nested/bridge.zig").Source;
pub const ThisSource = struct {
    pub fn fromThis() void {}
};
pub const ThisAlias = @This().ThisSource;
pub const cycle = @import("cycle_peer.zig");
pub usingnamespace @import("mixin.zig");
pub const Collision = struct {
    pub fn local() void {}
};
const PrivateCollision = struct {
    pub fn privateLocal() void {}
};

pub usingnamespace Later;
const Later = struct {
    pub fn laterFn() void {}
};
usingnamespace PrivateLater;
const PrivateLater = struct {
    pub fn privateMixed() void {}
};

const Local = struct {
    pub fn localFn() void {}
    fn hiddenLocal() void {}
};
pub usingnamespace Local;

pub const Plain = struct {
    value: u8,
    pub fn own() void {}
    fn hiddenMethod() void {}
};
pub fn stable() void {}
fn hiddenRoot() void {}
"#,
    );
    write(
        &tmp.path().join("src/direct.zig"),
        r#"pub fn run() void {}
fn hiddenDirect() void {}
pub const Nested = struct {
    pub fn ping() void {}
};
"#,
    );
    write(
        &tmp.path().join("src/facade.zig"),
        "pub const layer = @import(\"layer.zig\");\npub usingnamespace @import(\"facade_mixin.zig\");\n",
    );
    write(
        &tmp.path().join("src/facade_mixin.zig"),
        r#"pub const Injected = struct {
    pub fn fromFacadeMixin() void {}
};
"#,
    );
    write(
        &tmp.path().join("src/cycle_peer.zig"),
        r#"pub const back = @import("main.zig");
pub fn leaf() void {}
"#,
    );
    write(
        &tmp.path().join("src/layer.zig"),
        "const deep = @import(\"deep.zig\");\npub const Source = deep.Source;\n",
    );
    write(
        &tmp.path().join("src/deep.zig"),
        r#"pub const Source = struct {
    pub fn create() Source { return .{}; }
    fn secret() void {}
};
"#,
    );
    write(
        &tmp.path().join("src/nested/bridge.zig"),
        "pub const Source = @import(\"../deep.zig\").Source;\n",
    );
    write(
        &tmp.path().join("src/mixin.zig"),
        r#"pub fn mixed() void {}
pub const MixedType = struct {
    pub fn fromMixin() void {}
};
pub const Collision = struct {
    pub fn imported() void {}
};
pub const PrivateCollision = struct {
    pub fn importedPublic() void {}
};
fn hiddenMixed() void {}
"#,
    );

    let root = tmp.path().to_str().expect("UTF-8 temp path");
    let output = surface(&["surface", root, "--lang", "zig"]);

    for expected in [
        "direct.run",
        "direct.Nested.ping",
        "Renamed.create",
        "FacadeInjected.fromFacadeMixin",
        "InlineRenamed.create",
        "ParentNormalized.create",
        "ThisAlias.fromThis",
        "cycle.back.stable",
        "cycle.leaf",
        "mixed",
        "PrivateCollision.importedPublic",
        "laterFn",
        "localFn",
        "Plain.value",
        "Plain.own",
    ] {
        assert!(
            output.contains(expected),
            "missing Zig surface entry {expected}:\n{output}"
        );
    }
    for hidden in [
        "buildOnly",
        "facade_again",
        "hiddenDirect",
        "hiddenLocal",
        "hiddenMethod",
        "hiddenRoot",
        "hiddenMixed",
        "PrivateCollision.privateLocal",
        "privateMixed",
        "cycle.back.cycle.leaf",
        "Renamed.Source",
    ] {
        assert!(
            !output.contains(hidden),
            "private or unrenamed Zig entry leaked ({hidden}):\n{output}"
        );
    }
    assert!(
        output.contains("[via *]"),
        "usingnamespace entries need a glob marker:\n{output}"
    );
    let with_private = surface(&[
        "surface",
        &tmp.path().join("src/main.zig").to_string_lossy(),
        "--include-private",
    ]);
    assert!(
        with_private.contains("privateMixed"),
        "--include-private should retain private usingnamespace composition:\n{with_private}"
    );

    let json = surface(&[
        "surface",
        &tmp.path().join("src/main.zig").to_string_lossy(),
        "--json",
        "--compact",
    ]);
    let document: serde_json::Value = serde_json::from_str(&json).expect("valid surface JSON");
    let entries = document["entries"].as_array().expect("surface entries");
    assert!(
        !entries.iter().any(|entry| {
            entry["qualified_path"] == "Collision"
                || entry["qualified_path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with("Collision."))
        }),
        "ambiguous direct/usingnamespace names must be omitted: {document}"
    );
    let renamed = entries
        .iter()
        .find(|entry| entry["qualified_path"] == "Renamed")
        .expect("renamed facade entry");
    assert_eq!(renamed["source_name"], "Source");
    assert!(
        renamed["source_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("src/deep.zig")),
        "facade alias should point to its defining declaration: {renamed}"
    );
    assert!(
        renamed["re_export_chain"]
            .as_array()
            .is_some_and(|chain| chain.len() >= 4),
        "facade provenance chain is incomplete: {renamed}"
    );
    let normalized = entries
        .iter()
        .find(|entry| entry["qualified_path"] == "ParentNormalized")
        .expect("parent-relative facade entry");
    let normalized_source = normalized["source_path"]
        .as_str()
        .expect("source path string");
    assert!(normalized_source.ends_with("src/deep.zig"));
    assert!(
        !normalized_source.contains("/../"),
        "resolved Zig source paths should be normalized: {normalized_source}"
    );
    let mixed = entries
        .iter()
        .find(|entry| entry["qualified_path"] == "mixed")
        .expect("usingnamespace entry");
    assert_eq!(mixed["via_glob"], true);
}

#[test]
fn json_schema_present() {
    let s = surface(&[
        "surface",
        "tests/fixtures/surface/rust_chained",
        "--json",
        "--compact",
    ]);
    assert!(
        s.contains("\"schema\":\"ast-bro.surface.v1\""),
        "schema id missing:\n{s}"
    );
    assert!(s.contains("\"qualified_path\":\"rust_chained::Client\""));
}

#[test]
fn ts_barrel_resolves_named_glob_and_rename() {
    let s = surface(&["surface", "tests/fixtures/surface/ts_barrel"]);
    // Inline export class — picked up directly.
    assert!(s.contains("ts_barrel.Direct"), "Direct missing:\n{s}");
    // Lifted method.
    assert!(
        s.contains("ts_barrel.Direct.greet"),
        "Direct.greet missing:\n{s}"
    );
    // `export { Client } from './client'` — barrel resolution.
    assert!(s.contains("ts_barrel.Client"), "Client barrel missing:\n{s}");
    assert!(
        s.contains("ts_barrel.Client.connect"),
        "Client.connect missing:\n{s}"
    );
    // Rename via `export { Util as Helper }`.
    assert!(s.contains("ts_barrel.Helper"), "Helper rename missing:\n{s}");
    assert!(
        !s.contains("ts_barrel.Util"),
        "unrenamed Util leaked:\n{s}"
    );
    // `export *` glob — type and interface.
    assert!(s.contains("ts_barrel.Id"), "Id glob missing:\n{s}");
    assert!(s.contains("ts_barrel.Spec"), "Spec glob missing:\n{s}");
    assert!(
        s.contains("[via *]"),
        "glob marker missing:\n{s}"
    );
}

#[test]
fn ts_exports_field_picks_types_condition() {
    let s = surface(&["surface", "tests/fixtures/surface/ts_exports_field"]);
    // The `exports` field's `types` condition points at index.d.ts —
    // both `FromTypes` (with method) and `topLevel` should surface.
    assert!(
        s.contains("ts_exports_field.FromTypes"),
        "FromTypes missing:\n{s}"
    );
    assert!(
        s.contains("ts_exports_field.FromTypes.hello"),
        "lifted hello() missing:\n{s}"
    );
    assert!(
        s.contains("ts_exports_field.topLevel"),
        "topLevel missing:\n{s}"
    );
}

#[test]
fn scala_export_clauses_republish() {
    let s = surface(&["surface", "tests/fixtures/surface/scala_exports"]);
    // Direct top-level decls (visibility fallback path).
    assert!(s.contains("mypkg.Api"), "Api missing:\n{s}");
    assert!(s.contains("mypkg.PublicClass"), "PublicClass missing:\n{s}");
    assert!(
        s.contains("mypkg.PublicClass.publicMethod"),
        "lifted publicMethod missing:\n{s}"
    );
    // `export internal.Helper` — relative path, should land at mypkg.Helper.
    assert!(s.contains("mypkg.Helper"), "Helper re-export missing:\n{s}");
    // `export internal.utils.*` — glob expands util1 and util2.
    assert!(s.contains("mypkg.util1"), "util1 glob missing:\n{s}");
    assert!(s.contains("mypkg.util2"), "util2 glob missing:\n{s}");
    // private class is filtered.
    assert!(
        !s.contains("HiddenClass"),
        "private HiddenClass leaked:\n{s}"
    );
}

#[test]
fn unknown_lang_errors_cleanly() {
    // Unknown --lang is a rejected call under the error contract (#36):
    // exit 2, message on stderr, nothing on stdout.
    let out = Command::new(bin())
        .args(["surface", ".", "--lang", "cobol"])
        .env("NO_COLOR", "1")
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "rejected call must exit 2");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.is_empty(), "stdout must be empty:\n{stdout}");
    assert!(
        stderr.contains("unknown --lang")
            && stderr.contains("rust|python|typescript|scala|zig|fallback"),
        "expected error + expected-values hint on stderr:\n{stderr}"
    );
}
