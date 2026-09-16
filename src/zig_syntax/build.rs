//! Read literal module wiring from build.zig without executing a build script.

use super::{parse, string_value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tree_sitter::Node;

#[derive(Clone, Default)]
struct BuildInfo {
    modules: BTreeMap<String, Option<Vec<PathBuf>>>,
    roots: Vec<PathBuf>,
}

pub fn resolve_named_import(importer: &Path, name: &str) -> Option<PathBuf> {
    if matches!(name, "std" | "builtin" | "root") {
        return None;
    }
    for directory in importer.parent()?.ancestors() {
        let build = directory.join("build.zig");
        if build.is_file() {
            let info = read_build(&build)?;
            let candidates = info.modules.get(name)?.as_ref()?;
            let [target] = candidates.as_slice() else {
                return None;
            };
            return target.is_file().then(|| target.clone());
        }
        if directory.join(".git").exists() {
            break;
        }
    }
    None
}

pub fn compilation_root(build: &Path) -> Option<PathBuf> {
    let info = read_build(build)?;
    let [root] = info.roots.as_slice() else {
        return None;
    };
    root.is_file().then(|| root.clone())
}

fn read_build(path: &Path) -> Option<BuildInfo> {
    // Hash the contents so edits made within one filesystem clock tick also
    // invalidate this process-local cache (including MCP graph refreshes).
    type Cache = HashMap<PathBuf, (u64, BuildInfo)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let source = std::fs::read(path).ok()?;
    let hash = xxhash_rust::xxh3::xxh3_64(&source);
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    if let Some((previous, info)) = cache.get(path) {
        if *previous == hash {
            return Some(info.clone());
        }
    }
    let info = scan_build(&source, path.parent()?);
    cache.insert(path.to_path_buf(), (hash, info.clone()));
    Some(info)
}

fn scan_build(source: &[u8], directory: &Path) -> BuildInfo {
    let tree = parse(source);
    let mut nodes = Vec::new();
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        nodes.push(node);
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    let mut bindings: HashMap<&str, Vec<Node<'_>>> = HashMap::new();
    for node in &nodes {
        if node.kind() == "variable_declaration" {
            if let Some(name) = node
                .named_child(0)
                .and_then(|name| name.utf8_text(source).ok())
            {
                let mut cursor = node.walk();
                if let Some(value) = node.named_children(&mut cursor).last() {
                    bindings.entry(name).or_default().push(value);
                }
            }
        }
    }
    let mut info = BuildInfo::default();
    for node in nodes {
        if let Some((name, value)) = super::field_assignment(node) {
            if name.utf8_text(source).ok() == Some("root_source_file") {
                if let Some(path) = literal_path(value, source) {
                    info.roots.push(directory.join(path));
                }
            }
        }
        if node.kind() != "call_expression" {
            continue;
        }
        let Some(function) = node.child_by_field_name("function") else {
            continue;
        };
        let method = function
            .child_by_field_name("member")
            .and_then(|name| name.utf8_text(source).ok());
        if !matches!(method, Some("addModule" | "addImport")) {
            continue;
        }
        let Some(arguments) = node.child_by_field_name("arguments") else {
            continue;
        };
        let mut cursor = arguments.walk();
        let args: Vec<_> = arguments
            .named_children(&mut cursor)
            .filter(|node| node.kind() != "comment")
            .collect();
        let [name, options] = args.as_slice() else {
            continue;
        };
        let Some(name) = name.utf8_text(source).ok().and_then(string_value) else {
            continue;
        };
        let paths = module_roots(*options, source, &bindings, 0, &mut 256);
        let entry = info.modules.entry(name).or_insert_with(|| Some(Vec::new()));
        match (entry.as_mut(), paths) {
            (Some(existing), Some(paths)) => {
                existing.extend(paths.into_iter().map(|path| directory.join(path)))
            }
            _ => *entry = None,
        }
    }
    info.roots.sort();
    info.roots.dedup();
    for paths in info.modules.values_mut().flatten() {
        paths.sort();
        paths.dedup();
    }
    info
}

fn literal_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "call_expression" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    if function
        .child_by_field_name("member")?
        .utf8_text(source)
        .ok()?
        != "path"
    {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    string_value(args.named_child(0)?.utf8_text(source).ok()?)
}

fn module_roots(
    node: Node<'_>,
    source: &[u8],
    bindings: &HashMap<&str, Vec<Node<'_>>>,
    depth: usize,
    remaining: &mut usize,
) -> Option<Vec<String>> {
    // Typical wiring follows a few aliases. Bound both depth and total work
    // so cycles and repeated aliases cannot expand exponentially.
    if depth >= 32 || *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    if node.kind() == "identifier" {
        let declarations = bindings.get(node.utf8_text(source).ok()?)?;
        let mut roots = Vec::new();
        for declaration in declarations {
            roots.extend(module_roots(
                *declaration,
                source,
                bindings,
                depth + 1,
                remaining,
            )?);
        }
        return Some(roots);
    }
    if node.kind() == "call_expression" {
        let method = node
            .child_by_field_name("function")?
            .child_by_field_name("member")?
            .utf8_text(source)
            .ok()?;
        if !matches!(method, "createModule" | "addModule") {
            return None;
        }
        let arguments = node.child_by_field_name("arguments")?;
        let mut cursor = arguments.walk();
        let options = arguments
            .named_children(&mut cursor)
            .filter(|node| node.kind() != "comment")
            .last()?;
        return module_roots(options, source, bindings, depth + 1, remaining);
    }
    let list = if node.kind() == "initializer_list" {
        node
    } else {
        let mut cursor = node.walk();
        let list = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "initializer_list")?;
        list
    };
    let mut cursor = list.walk();
    for field in list.named_children(&mut cursor) {
        if let Some((name, value)) = super::field_assignment(field) {
            if name.utf8_text(source).ok() == Some("root_source_file") {
                return Some(vec![literal_path(value, source)?]);
            }
        }
    }
    None
}
