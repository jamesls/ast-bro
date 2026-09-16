fn main() {
    let source = "vendor/tree-sitter-zig/src";
    cc::Build::new()
        .std("c11")
        .include(source)
        .file(format!("{source}/parser.c"))
        .warnings(false)
        .compile("tree-sitter-zig");
    println!("cargo:rerun-if-changed={source}");
}
