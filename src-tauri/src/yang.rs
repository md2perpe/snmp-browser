//! YANG directory scanning and parsing, built on the `tree-sitter-yang` grammar - the schema-side
//! counterpart to `mib.rs`, giving gNMI the same "browse before you fetch" tree that MIB files
//! give SNMP. Reuses `mib.rs`'s directory walking (`collect_files`) and small `Node` helpers,
//! since both are "parse a pile of local schema files with tree-sitter" in the same shape.
//!
//! Phase 1 only, with real gaps against full YANG semantics (RFC 7950):
//! - `deviation`, `when`, `if-feature` and `feature` are not evaluated - everything they'd
//!   conditionally add or remove is shown unconditionally.
//! - `augment` targets are *not* spliced into the target location's spot in the tree; an augment
//!   is shown inline where it's declared, labeled with its target path. Clicking it (or a node
//!   under it) still sets the Path field to the real target path, so it's still usable for
//!   browsing and fetching - it just isn't visually merged into the target container.
//! - `uses` is resolved (grouping body inlined at the use site, including across files via
//!   `import`) for groupings found anywhere in a parsed module/submodule. A `uses` that can't be
//!   resolved (missing file, unknown prefix, or a reference cycle) is shown as a single greyed-out
//!   placeholder rather than silently dropped.
//! - A `submodule` is shown as its own top-level root rather than merged into the module it
//!   belongs to (`belongs-to`), since that would need matching submodules to modules by name
//!   across files.
//!
//! Every path segment is qualified with its owning YANG module's name (`module-name:node-name`),
//! not just the top-level one - this matches real-world examples (e.g. OpenROADM's
//! `/org-openroadm-device:org-openroadm-device/org-openroadm-device:circuit-packs[...]`) and,
//! since content pulled in via a cross-module `uses` genuinely lives in that other module's
//! namespace, is more correct than qualifying only the first segment.

use crate::gnmi::unquote;
use crate::mib::{collect_files, find_child_by_kind, node_text, push_error, DirFiles, FileErrors, NodeKind};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser};

#[derive(Serialize, Clone, Debug)]
pub struct YangTreeNode {
    pub id: String,
    pub label: String,
    /// Absolute gNMI xpath-style path, or "" for a node that isn't itself a valid fetch target
    /// (an unresolved `uses` placeholder).
    pub path: String,
    pub resolved: bool,
    #[serde(rename = "type")]
    pub kind: NodeKind,
    pub children: Vec<YangTreeNode>,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct YangParseResult {
    pub tree: Vec<YangTreeNode>,
    pub errors: Vec<FileErrors>,
    pub dir_files: Vec<DirFiles>,
}

/// A parsed module or submodule, and the little bit of its own header this pass needs again
/// later: its name (used both as its tree root label and as the qualifier on every path segment
/// under it) and its `import`s (local prefix -> imported module name, needed to resolve a
/// prefixed `uses`/`augment` reference back to the module that actually defines it).
struct ModuleInfo<'a> {
    name: String,
    root: Node<'a>,
    imports: HashMap<String, String>,
    src: &'a [u8],
}

fn arg_text(node: Node, src: &[u8]) -> String {
    unquote(node_text(node, src).trim())
}

/// Recursively collects every `grouping_stmt` anywhere under `node` (not just direct children -
/// groupings are commonly nested inside containers/lists too), keyed by the module it was found
/// in plus its own name.
fn collect_groupings<'a>(node: Node<'a>, src: &'a [u8], module_name: &str, out: &mut HashMap<(String, String), (Node<'a>, &'a [u8])>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "grouping_stmt" {
            if let Some(arg) = child.child_by_field_name("arg") {
                out.insert((module_name.to_string(), arg_text(arg, src)), (child, src));
            }
        }
        collect_groupings(child, src, module_name, out);
    }
}

/// Resolves a possibly-`prefix:name`-qualified reference (a `uses` target, or one segment of an
/// `augment` target path) to the full module name that owns it, using `origin_module`'s own
/// `import` table when a prefix is present.
fn resolve_qualified<'a>(raw: &str, origin_module: &'a str, imports: &'a HashMap<String, String>) -> (&'a str, String) {
    match raw.split_once(':') {
        Some((prefix, name)) => (imports.get(prefix).map(String::as_str).unwrap_or(origin_module), name.to_string()),
        None => (origin_module, raw.to_string()),
    }
}

/// Rewrites an `augment` target path's segments to `module-name:node-name` form, resolving each
/// segment's own prefix (if any) against `origin_module`'s imports, same as a data node's path.
fn resolve_target_path(raw: &str, origin_module: &str, imports: &HashMap<String, String>) -> String {
    let mut out = String::new();
    for seg in raw.split('/').filter(|s| !s.is_empty()) {
        let (module, name) = resolve_qualified(seg, origin_module, imports);
        out.push('/');
        out.push_str(module);
        out.push(':');
        out.push_str(&name);
    }
    out
}

fn imports_of<'a>(modules: &'a [ModuleInfo], module_name: &str) -> HashMap<String, String> {
    modules.iter().find(|m| m.name == module_name).map(|m| m.imports.clone()).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn build_children<'a>(
    stmt_node: Node<'a>,
    src: &'a [u8],
    origin_module: &str,
    path_prefix: &str,
    modules: &[ModuleInfo<'a>],
    groupings: &HashMap<(String, String), (Node<'a>, &'a [u8])>,
    visiting: &mut HashSet<(String, String)>,
    depth: u32,
) -> Vec<YangTreeNode> {
    let mut out = Vec::new();
    if depth > 200 {
        return out; // defensive backstop against a pathological/malformed file, not a normal limit
    }
    let imports = imports_of(modules, origin_module);

    let mut cursor = stmt_node.walk();
    for child in stmt_node.named_children(&mut cursor) {
        match child.kind() {
            "container_stmt" | "list_stmt" | "leaf_stmt" | "leaf_list_stmt" | "anydata_stmt" | "anyxml_stmt" => {
                let Some(name) = child.child_by_field_name("arg").map(|n| arg_text(n, src)) else { continue };
                let path = format!("{path_prefix}/{origin_module}:{name}");
                let kind = match child.kind() {
                    "container_stmt" => NodeKind::Group,
                    "list_stmt" => NodeKind::Table,
                    _ => NodeKind::Scalar,
                };
                let label = if child.kind() == "list_stmt" {
                    match find_child_by_kind(child, "key_stmt").and_then(|k| k.child_by_field_name("arg")) {
                        Some(key_arg) => format!("{name} [{}]", arg_text(key_arg, src)),
                        None => name,
                    }
                } else {
                    name
                };
                let children = if kind == NodeKind::Scalar {
                    Vec::new()
                } else {
                    build_children(child, src, origin_module, &path, modules, groupings, visiting, depth + 1)
                };
                out.push(YangTreeNode { id: path.clone(), label, path, resolved: true, kind, children });
            }
            "choice_stmt" | "case_stmt" => {
                // Neither contributes a path segment - their content lands directly under the
                // enclosing container/list once one branch is chosen, so splice it in flat.
                out.extend(build_children(child, src, origin_module, path_prefix, modules, groupings, visiting, depth + 1));
            }
            "uses_stmt" => {
                let Some(raw) = child.child_by_field_name("arg").map(|n| arg_text(n, src)) else { continue };
                let (target_module, grouping_name) = resolve_qualified(&raw, origin_module, &imports);
                let key = (target_module.to_string(), grouping_name.clone());
                if !visiting.contains(&key) {
                    if let Some(&(grouping_node, grouping_src)) = groupings.get(&key) {
                        visiting.insert(key.clone());
                        out.extend(build_children(grouping_node, grouping_src, target_module, path_prefix, modules, groupings, visiting, depth + 1));
                        visiting.remove(&key);
                        continue;
                    }
                }
                out.push(YangTreeNode {
                    id: format!("{path_prefix}#uses:{raw}"),
                    label: format!("uses {raw}"),
                    path: String::new(),
                    resolved: false,
                    kind: NodeKind::Scalar,
                    children: Vec::new(),
                });
            }
            "augment_stmt" => {
                let Some(raw) = child.child_by_field_name("arg").map(|n| arg_text(n, src)) else { continue };
                let target = resolve_target_path(&raw, origin_module, &imports);
                let children = build_children(child, src, origin_module, &target, modules, groupings, visiting, depth + 1);
                out.push(YangTreeNode {
                    id: format!("{path_prefix}#augment:{target}"),
                    label: format!("augment {target}"),
                    path: target,
                    resolved: true,
                    kind: NodeKind::Group,
                    children,
                });
            }
            _ => {} // description/config/status/type/etc. - not part of the browsable data tree
        }
    }
    out
}

pub fn parse_directories(dirs: &[String]) -> YangParseResult {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_yang::LANGUAGE.into()).is_err() {
        return YangParseResult {
            errors: vec![FileErrors { file: "<internal>".into(), errors: vec!["failed to load YANG grammar".into()] }],
            ..Default::default()
        };
    }

    let mut errors: Vec<FileErrors> = Vec::new();
    let mut dir_files: Vec<DirFiles> = Vec::new();
    let mut parsed: Vec<(String, String, tree_sitter::Tree)> = Vec::new();

    for dir in dirs {
        let mut files = Vec::new();
        collect_files(std::path::Path::new(dir), &mut files, &mut errors);
        dir_files.push(DirFiles { dir: dir.clone(), files: files.iter().map(|p| p.display().to_string()).collect() });
        for path in files {
            if path.extension().and_then(|e| e.to_str()) != Some("yang") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue; // not a readable text file - skip silently
            };
            let file = path.display().to_string();
            let Some(tree) = parser.parse(&src, None) else {
                push_error(&mut errors, &file, "failed to parse".into());
                continue;
            };
            if tree.root_node().has_error() {
                push_error(&mut errors, &file, "contains syntax errors (parsed on a best-effort basis)".into());
            }
            parsed.push((file, src, tree));
        }
    }

    let mut modules: Vec<ModuleInfo> = Vec::new();
    let mut groupings: HashMap<(String, String), (Node, &[u8])> = HashMap::new();

    for (_file, src, tree) in &parsed {
        let src = src.as_bytes();
        let Some(module_node) = tree.root_node().named_child(0) else { continue };
        if module_node.kind() != "module_stmt" && module_node.kind() != "submodule_stmt" {
            continue;
        }
        let Some(name) = module_node.child_by_field_name("arg").map(|n| arg_text(n, src)) else { continue };

        let mut imports = HashMap::new();
        let mut cursor = module_node.walk();
        for child in module_node.named_children(&mut cursor) {
            if child.kind() == "import_stmt" {
                let Some(imported) = child.child_by_field_name("arg").map(|n| arg_text(n, src)) else { continue };
                if let Some(prefix_arg) = find_child_by_kind(child, "prefix_stmt").and_then(|p| p.child_by_field_name("arg")) {
                    imports.insert(arg_text(prefix_arg, src), imported);
                }
            }
        }

        collect_groupings(module_node, src, &name, &mut groupings);
        modules.push(ModuleInfo { name, root: module_node, imports, src });
    }

    let mut tree = Vec::new();
    for m in &modules {
        let mut visiting = HashSet::new();
        let children = build_children(m.root, m.src, &m.name, "", &modules, &groupings, &mut visiting, 0);
        tree.push(YangTreeNode { id: format!("yang:{}", m.name), label: m.name.clone(), path: "/".to_string(), resolved: true, kind: NodeKind::Group, children });
    }

    YangParseResult { tree, errors, dir_files }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn builds_a_tree_with_containers_lists_choice_and_cross_module_uses() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "example-common.yang",
            r#"
module example-common {
    namespace "urn:example:common";
    prefix common;

    grouping port-attrs {
        leaf speed {
            type string;
        }
        leaf enabled {
            type boolean;
        }
    }
}
"#,
        );
        write(
            dir.path(),
            "example-device.yang",
            r#"
module example-device {
    namespace "urn:example:device";
    prefix dev;

    import example-common {
        prefix common;
    }

    container device {
        list circuit-packs {
            key "name";
            leaf name {
                type string;
            }
            container ports {
                list port {
                    key "port-name";
                    leaf port-name {
                        type string;
                    }
                    uses common:port-attrs;
                    choice mode {
                        case fixed {
                            leaf fixed-rate {
                                type uint32;
                            }
                        }
                        case auto {
                            leaf auto-negotiate {
                                type boolean;
                            }
                        }
                    }
                }
            }
        }
    }

    augment "/dev:device" {
        leaf status {
            type string;
        }
    }
}
"#,
        );

        let result = parse_directories(&[dir.path().display().to_string()]);
        assert!(result.errors.is_empty(), "unexpected parse errors: {:?}", result.errors);
        assert_eq!(result.tree.len(), 2, "expected one root per module");

        let device = result.tree.iter().find(|n| n.label == "example-device").expect("example-device root");
        let device_container = device.children.iter().find(|n| n.label == "device").expect("device container");
        assert_eq!(device_container.path, "/example-device:device");
        assert_eq!(device_container.kind, NodeKind::Group);

        let packs = device_container.children.iter().find(|n| n.label.starts_with("circuit-packs")).expect("circuit-packs list");
        assert_eq!(packs.kind, NodeKind::Table);
        assert!(packs.label.contains("[name]"), "label should show the key: {}", packs.label);
        assert_eq!(packs.path, "/example-device:device/example-device:circuit-packs");

        let ports_container = packs.children.iter().find(|n| n.label == "ports").expect("ports container");
        let port_list = ports_container.children.iter().find(|n| n.label.starts_with("port")).expect("port list");

        // `uses common:port-attrs` should be resolved and its leafs inlined directly under `port`,
        // qualified with the *defining* module (example-common), not the using module.
        let speed = port_list.children.iter().find(|n| n.label == "speed").expect("speed leaf from the resolved uses");
        assert_eq!(speed.path, "/example-device:device/example-device:circuit-packs/example-device:ports/example-device:port/example-common:speed");
        assert!(speed.resolved);

        // choice/case should be flattened away - both branches' leafs appear as direct children.
        assert!(port_list.children.iter().any(|n| n.label == "fixed-rate"));
        assert!(port_list.children.iter().any(|n| n.label == "auto-negotiate"));

        // The augment shows up inline, labeled with its resolved target path, and is itself usable
        // as a path (even though it isn't spliced into `device`'s own child list).
        let augment = device.children.iter().find(|n| n.label.starts_with("augment")).expect("augment node");
        assert_eq!(augment.path, "/example-device:device");
        assert!(augment.children.iter().any(|n| n.label == "status"));
    }

    #[test]
    fn an_unresolvable_uses_is_shown_as_a_greyed_out_placeholder_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "example-lonely.yang",
            r#"
module example-lonely {
    namespace "urn:example:lonely";
    prefix lonely;

    container top {
        uses missing-grouping;
    }
}
"#,
        );

        let result = parse_directories(&[dir.path().display().to_string()]);
        let top = result.tree[0].children.iter().find(|n| n.label == "top").unwrap();
        let placeholder = &top.children[0];
        assert_eq!(placeholder.label, "uses missing-grouping");
        assert!(!placeholder.resolved);
        assert_eq!(placeholder.path, "");
    }
}
