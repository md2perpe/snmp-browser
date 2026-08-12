//! MIB directory scanning and parsing, built on the `tree-sitter-asn1` grammar.
//!
//! OID values in MIB source are relative (`::= { interfaces 2 }`): resolving
//! them to absolute dotted OIDs requires knowing the full path of whatever
//! symbol they reference, which may live in a different file, and that
//! symbol's own ancestors, and so on. We do this in two passes: first parse
//! every file into a flat list of `RawSymbol`s (name, kind, raw OID
//! components), then run a fixed-point resolution pass over that whole set
//! until no more progress can be made. Anything left over — most commonly
//! because the standard root MIBs (RFC1155-SMI etc.) weren't among the
//! configured directories — is reported as unresolved rather than guessed.
//!
//! The tree shown in the UI is built from declared parent/child name
//! references (the first component of each OID value), not from the
//! resolved numeric paths, so an unresolved node still lands in the right
//! place in the tree (as a leaf of its nominal parent, or as a top-level
//! entry if even that isn't known) and can be shown greyed out.

use serde::Serialize;
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Group,
    Scalar,
    Table,
}

#[derive(Serialize, Clone, Debug)]
pub struct MibTreeNode {
    pub id: String,
    pub label: String,
    pub oid: String,
    pub resolved: bool,
    #[serde(rename = "type")]
    pub kind: NodeKind,
    pub children: Vec<MibTreeNode>,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub name: String,
    pub arc: Option<u32>,
    pub enum_values: Vec<(i64, String)>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct TableInfo {
    pub oid: String,
    pub resolved: bool,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInfo {
    pub oid: String,
    pub resolved: bool,
    #[serde(rename = "type")]
    pub kind: NodeKind,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct FileErrors {
    pub file: String,
    pub errors: Vec<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ParseResult {
    pub tree: Vec<MibTreeNode>,
    pub tables: HashMap<String, TableInfo>,
    pub symbols: HashMap<String, SymbolInfo>,
    pub errors: Vec<FileErrors>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawKind {
    Group,
    Scalar,
    Table,
    Other,
}

#[derive(Clone, Debug)]
enum OidComponent {
    Number(u32),
    Named(String, u32),
    Ident(String),
}

#[derive(Clone, Debug)]
struct RawSymbol {
    name: String,
    oid: Vec<OidComponent>,
    kind: RawKind,
    /// Set when SYNTAX is `SEQUENCE OF X` - X is the entry type name.
    sequence_of_target: Option<String>,
    enum_values: Vec<(i64, String)>,
    /// Path of the file this symbol was declared in, for grouping error messages.
    file: String,
}

fn push_error(errors: &mut Vec<FileErrors>, file: &str, msg: String) {
    if let Some(entry) = errors.iter_mut().find(|e| e.file == file) {
        entry.errors.push(msg);
    } else {
        errors.push(FileErrors { file: file.to_string(), errors: vec![msg] });
    }
}

pub fn parse_directories(dirs: &[String]) -> ParseResult {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_asn1::LANGUAGE.into()).is_err() {
        return ParseResult {
            errors: vec![FileErrors { file: "<internal>".into(), errors: vec!["failed to load MIB grammar".into()] }],
            ..Default::default()
        };
    }

    let mut raw: Vec<RawSymbol> = Vec::new();
    let mut sequence_types: HashMap<String, Vec<String>> = HashMap::new();
    let mut errors: Vec<FileErrors> = Vec::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                push_error(&mut errors, dir, format!("{e}"));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
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
            walk_module(tree.root_node(), src.as_bytes(), &file, &mut raw, &mut sequence_types);
        }
    }

    let (resolved, unresolved_names) = resolve_oids(&raw);
    let file_of: HashMap<&str, &str> = raw.iter().map(|s| (s.name.as_str(), s.file.as_str())).collect();
    for name in unresolved_names {
        let file = file_of.get(name.as_str()).copied().unwrap_or("<unknown>");
        push_error(&mut errors, file, format!("{name}: could not resolve to an absolute OID (missing ancestor definition)"));
    }

    let tree = build_tree(&raw, &resolved);
    let tables = build_tables(&raw, &sequence_types, &resolved);
    let symbols = raw
        .iter()
        .filter(|s| s.kind != RawKind::Other)
        .map(|s| {
            let path = resolved.get(&s.name);
            let kind = match s.kind {
                RawKind::Table => NodeKind::Table,
                RawKind::Group => NodeKind::Group,
                RawKind::Scalar | RawKind::Other => NodeKind::Scalar,
            };
            (s.name.clone(), SymbolInfo { oid: path.map(|p| dotted(p)).unwrap_or_default(), resolved: path.is_some(), kind })
        })
        .collect();

    ParseResult { tree, tables, symbols, errors }
}

fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn walk_module(root: Node, src: &[u8], file: &str, raw: &mut Vec<RawSymbol>, sequence_types: &mut HashMap<String, Vec<String>>) {
    let Some(module_def) = find_child_by_kind(root, "module_definition") else { return };
    let mut cursor = module_def.walk();
    for assignment in module_def.named_children(&mut cursor) {
        if assignment.kind() != "assignment" {
            continue;
        }
        let Some(inner) = assignment.named_child(0) else { continue };
        match inner.kind() {
            "object_identifier_assignment" | "object_identity_assignment" | "module_identity_assignment" => {
                let (Some(name_node), Some(oid_node)) = (inner.named_child(0), find_child_by_kind(inner, "object_identifier_value")) else { continue };
                raw.push(RawSymbol {
                    name: node_text(name_node, src).to_string(),
                    oid: parse_oid_value(oid_node, src),
                    kind: RawKind::Group,
                    sequence_of_target: None,
                    enum_values: vec![],
                    file: file.to_string(),
                });
            }
            "object_type_assignment" => {
                let (Some(name_node), Some(clauses), Some(oid_node)) = (
                    inner.named_child(0),
                    find_child_by_kind(inner, "object_type_clauses"),
                    find_child_by_kind(inner, "object_identifier_value"),
                ) else {
                    continue;
                };

                let mut kind = RawKind::Scalar;
                let mut sequence_of_target = None;
                let mut enum_values = vec![];

                if let Some(syntax_clause) = clauses.child_by_field_name("syntax") {
                    if let Some(type_node) = find_child_by_kind(syntax_clause, "type") {
                        if let Some(seq_of) = find_child_by_kind(type_node, "sequence_of_type") {
                            kind = RawKind::Table;
                            if let Some(ident) = find_child_by_kind(seq_of, "identifier") {
                                sequence_of_target = Some(node_text(ident, src).to_string());
                            }
                        } else if let Some(enum_node) = find_child_by_kind(type_node, "enum_constraint") {
                            enum_values = parse_enum_constraint(enum_node, src);
                        }
                    }
                }

                raw.push(RawSymbol {
                    name: node_text(name_node, src).to_string(),
                    oid: parse_oid_value(oid_node, src),
                    kind,
                    sequence_of_target,
                    enum_values,
                    file: file.to_string(),
                });
            }
            "notification_type_assignment" | "trap_type_assignment" => {
                // Not part of the browsable OID tree (no value to GET), but keep them
                // in the symbol table in case something legitimately parents off one.
                if let Some(name_node) = inner.named_child(0) {
                    let oid = find_child_by_kind(inner, "object_identifier_value")
                        .map(|n| parse_oid_value(n, src))
                        .unwrap_or_default();
                    raw.push(RawSymbol {
                        name: node_text(name_node, src).to_string(),
                        oid,
                        kind: RawKind::Other,
                        sequence_of_target: None,
                        enum_values: vec![],
                        file: file.to_string(),
                    });
                }
            }
            "type_assignment" => {
                let (Some(name_node), Some(type_node)) = (inner.named_child(0), inner.named_child(1)) else { continue };
                if let Some(seq_type) = find_child_by_kind(type_node, "sequence_type") {
                    let mut c2 = seq_type.walk();
                    let fields: Vec<String> = seq_type
                        .named_children(&mut c2)
                        .filter(|f| f.kind() == "sequence_field")
                        .filter_map(|f| f.child_by_field_name("name"))
                        .map(|n| node_text(n, src).to_string())
                        .collect();
                    sequence_types.insert(node_text(name_node, src).to_string(), fields);
                }
            }
            _ => {}
        }
    }
}

fn parse_enum_constraint(enum_node: Node, src: &[u8]) -> Vec<(i64, String)> {
    let mut cursor = enum_node.walk();
    enum_node
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "named_number")
        .filter_map(|n| {
            let name = n.child_by_field_name("name")?;
            let value = n.child_by_field_name("value")?;
            let v: i64 = node_text(value, src).parse().ok()?;
            Some((v, node_text(name, src).to_string()))
        })
        .collect()
}

fn parse_oid_value(node: Node, src: &[u8]) -> Vec<OidComponent> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter_map(|c| match c.kind() {
            "number" => node_text(c, src).parse::<u32>().ok().map(OidComponent::Number),
            "named_oid_component" => {
                let name = c.child_by_field_name("name")?;
                let value = c.child_by_field_name("value")?;
                let v: u32 = node_text(value, src).parse().ok()?;
                Some(OidComponent::Named(node_text(name, src).to_string(), v))
            }
            "identifier" => Some(OidComponent::Ident(node_text(c, src).to_string())),
            _ => None,
        })
        .collect()
}

/// The name of the parent this symbol was declared under, if any (a bare
/// leading number, e.g. a genuine root's `::= { 1 }`, has no named parent).
fn parent_name(components: &[OidComponent]) -> Option<String> {
    match components.first()? {
        OidComponent::Ident(name) | OidComponent::Named(name, _) => Some(name.clone()),
        OidComponent::Number(_) => None,
    }
}

fn last_arc(components: &[OidComponent]) -> Option<u32> {
    match components.last()? {
        OidComponent::Number(n) => Some(*n),
        OidComponent::Named(_, v) => Some(*v),
        OidComponent::Ident(_) => None,
    }
}

fn try_resolve(components: &[OidComponent], resolved: &HashMap<String, Vec<u32>>) -> Option<Vec<u32>> {
    let mut iter = components.iter();
    let mut path: Vec<u32> = match iter.next()? {
        OidComponent::Number(n) => vec![*n],
        OidComponent::Named(_, v) => vec![*v],
        OidComponent::Ident(name) => resolved.get(name)?.clone(),
    };
    for c in iter {
        match c {
            OidComponent::Number(n) => path.push(*n),
            OidComponent::Named(_, v) => path.push(*v),
            OidComponent::Ident(name) => path = resolved.get(name)?.clone(),
        }
    }
    Some(path)
}

/// The top-level arcs of the ASN.1 OID tree (X.680 §12.2) are primitives of
/// the notation itself - `iso OBJECT IDENTIFIER ::= { 1 }` never actually
/// appears in MIB source, yet nearly every MIB's whole ancestor chain roots
/// through `iso`. Seed them so resolution doesn't treat every module as
/// unresolvable just because none of them declares its own root.
const WELL_KNOWN_ROOTS: &[(&str, u32)] =
    &[("ccitt", 0), ("itu-t", 0), ("iso", 1), ("joint-iso-itu-t", 2), ("joint-iso-ccitt", 2)];

fn resolve_oids(raw: &[RawSymbol]) -> (HashMap<String, Vec<u32>>, Vec<String>) {
    let mut resolved: HashMap<String, Vec<u32>> = WELL_KNOWN_ROOTS.iter().map(|&(name, arc)| (name.to_string(), vec![arc])).collect();
    let mut remaining: Vec<&RawSymbol> = raw.iter().collect();
    loop {
        let mut progressed = false;
        remaining.retain(|s| {
            if resolved.contains_key(&s.name) {
                return false;
            }
            match try_resolve(&s.oid, &resolved) {
                Some(path) => {
                    resolved.insert(s.name.clone(), path);
                    progressed = true;
                    false
                }
                None => true,
            }
        });
        if !progressed {
            break;
        }
    }
    let unresolved = remaining.iter().map(|s| s.name.clone()).collect();
    (resolved, unresolved)
}

fn dotted(path: &[u32]) -> String {
    path.iter().map(u32::to_string).collect::<Vec<_>>().join(".")
}

fn build_tree(raw: &[RawSymbol], resolved: &HashMap<String, Vec<u32>>) -> Vec<MibTreeNode> {
    let by_name: HashMap<&str, &RawSymbol> = raw.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut children_of: HashMap<String, Vec<&RawSymbol>> = HashMap::new();
    let mut roots: Vec<&RawSymbol> = Vec::new();

    for s in raw {
        if s.kind == RawKind::Other {
            continue;
        }
        match parent_name(&s.oid) {
            Some(p) if by_name.contains_key(p.as_str()) => children_of.entry(p).or_default().push(s),
            _ => roots.push(s),
        }
    }

    fn build_node(s: &RawSymbol, children_of: &HashMap<String, Vec<&RawSymbol>>, resolved: &HashMap<String, Vec<u32>>) -> MibTreeNode {
        let path = resolved.get(&s.name);
        let kind = match s.kind {
            RawKind::Table => NodeKind::Table,
            RawKind::Group => NodeKind::Group,
            RawKind::Scalar | RawKind::Other => NodeKind::Scalar,
        };
        // Table columns live under a hidden "entry" object; browsing stops at the table itself.
        let children = if s.kind == RawKind::Table {
            Vec::new()
        } else {
            let mut kids: Vec<&RawSymbol> = children_of.get(&s.name).cloned().unwrap_or_default();
            kids.sort_by(|a, b| a.name.cmp(&b.name));
            kids.iter().map(|k| build_node(k, children_of, resolved)).collect()
        };
        MibTreeNode {
            id: s.name.clone(),
            label: s.name.clone(),
            oid: path.map(|p| dotted(p)).unwrap_or_default(),
            resolved: path.is_some(),
            kind,
            children,
        }
    }

    // Keep only roots that actually resolve to somewhere under `iso` (arc 1) -
    // the other ASN.1 primitives (ccitt/itu-t, joint-iso-*) never appear in
    // real MIB data, and anything left unresolved can't be confirmed to
    // belong under iso at all. Nest them under a single synthetic `iso` node
    // so it's the tree's sole root, matching how OID trees are conventionally
    // browsed - unresolved *descendants* further down are unaffected by this
    // and still show up greyed out, as before.
    let mut iso_roots: Vec<&RawSymbol> =
        roots.into_iter().filter(|s| matches!(resolved.get(&s.name), Some(path) if path.first() == Some(&1))).collect();
    iso_roots.sort_by(|a, b| a.name.cmp(&b.name));
    let children = iso_roots.iter().map(|s| build_node(s, &children_of, resolved)).collect();

    vec![MibTreeNode { id: "iso".to_string(), label: "iso".to_string(), oid: "1".to_string(), resolved: true, kind: NodeKind::Group, children }]
}

fn build_tables(raw: &[RawSymbol], sequence_types: &HashMap<String, Vec<String>>, resolved: &HashMap<String, Vec<u32>>) -> HashMap<String, TableInfo> {
    let by_name: HashMap<&str, &RawSymbol> = raw.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut tables = HashMap::new();

    for s in raw {
        if s.kind != RawKind::Table {
            continue;
        }
        let Some(entry_type_name) = &s.sequence_of_target else { continue };
        let Some(field_names) = sequence_types.get(entry_type_name) else { continue };

        let columns = field_names
            .iter()
            .map(|field_name| {
                let col = by_name.get(field_name.as_str());
                ColumnInfo {
                    name: field_name.clone(),
                    arc: col.and_then(|c| last_arc(&c.oid)),
                    enum_values: col.map(|c| c.enum_values.clone()).unwrap_or_default(),
                }
            })
            .collect();

        let path = resolved.get(&s.name);
        tables.insert(
            s.name.clone(),
            TableInfo { oid: path.map(|p| dotted(p)).unwrap_or_default(), resolved: path.is_some(), columns },
        );
    }

    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    const IF_TABLE_MIB: &str = r#"
TEST-MIB DEFINITIONS ::= BEGIN

mib-2 OBJECT IDENTIFIER ::= { 1 3 6 1 2 1 }
interfaces OBJECT IDENTIFIER ::= { mib-2 2 }

ifNumber OBJECT-TYPE
    SYNTAX INTEGER
    MAX-ACCESS read-only
    STATUS current
    DESCRIPTION "n"
    ::= { interfaces 1 }

ifTable OBJECT-TYPE
    SYNTAX SEQUENCE OF IfEntry
    MAX-ACCESS not-accessible
    STATUS current
    DESCRIPTION "t"
    ::= { interfaces 2 }

ifEntry OBJECT-TYPE
    SYNTAX IfEntry
    MAX-ACCESS not-accessible
    STATUS current
    DESCRIPTION "e"
    INDEX { ifIndex }
    ::= { ifTable 1 }

IfEntry ::= SEQUENCE {
    ifIndex INTEGER,
    ifDescr OCTET STRING,
    ifOperStatus INTEGER
}

ifIndex OBJECT-TYPE
    SYNTAX INTEGER
    MAX-ACCESS read-only
    STATUS current
    DESCRIPTION "i"
    ::= { ifEntry 1 }

ifDescr OBJECT-TYPE
    SYNTAX OCTET STRING
    MAX-ACCESS read-only
    STATUS current
    DESCRIPTION "d"
    ::= { ifEntry 2 }

ifOperStatus OBJECT-TYPE
    SYNTAX INTEGER { up(1), down(2), testing(3) }
    MAX-ACCESS read-only
    STATUS current
    DESCRIPTION "o"
    ::= { ifEntry 8 }

enterprises OBJECT IDENTIFIER ::= { private 1 }

END
"#;

    fn write_fixture(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("TEST.mib"), contents).unwrap();
        dir
    }

    #[test]
    fn resolves_absolute_oids_across_the_ancestor_chain() {
        let dir = write_fixture(IF_TABLE_MIB);
        let result = parse_directories(&[dir.path().to_string_lossy().to_string()]);

        let if_table = result.symbols.get("ifTable").expect("ifTable symbol");
        assert!(if_table.resolved, "ifTable should resolve via its full ancestor chain");
        assert_eq!(if_table.oid, "1.3.6.1.2.1.2.2");
        assert_eq!(if_table.kind, NodeKind::Table);
    }

    #[test]
    fn unresolvable_ancestor_is_reported_not_guessed() {
        let dir = write_fixture(IF_TABLE_MIB);
        let result = parse_directories(&[dir.path().to_string_lossy().to_string()]);

        // "private" is never defined in this fixture, so enterprises can't resolve.
        let enterprises = result.symbols.get("enterprises").expect("enterprises symbol");
        assert!(!enterprises.resolved);
        assert!(result.errors.iter().any(|fe| fe.errors.iter().any(|e| e.contains("enterprises"))));
    }

    #[test]
    fn well_known_asn1_roots_resolve_without_being_declared() {
        // Real MIBs never declare `iso OBJECT IDENTIFIER ::= { 1 }` themselves -
        // it's a primitive of the notation - so the whole ancestor chain must
        // still resolve through it.
        let mib = r#"
TEST-MIB DEFINITIONS ::= BEGIN
org OBJECT IDENTIFIER ::= { iso 3 }
dod OBJECT IDENTIFIER ::= { org 6 }
END
"#;
        let dir = write_fixture(mib);
        let result = parse_directories(&[dir.path().to_string_lossy().to_string()]);

        let dod = result.symbols.get("dod").expect("dod symbol");
        assert!(dod.resolved, "dod should resolve through the well-known 'iso' root");
        assert_eq!(dod.oid, "1.3.6");
    }

    #[test]
    fn table_appears_as_a_leaf_with_no_column_children_in_the_tree() {
        let dir = write_fixture(IF_TABLE_MIB);
        let result = parse_directories(&[dir.path().to_string_lossy().to_string()]);

        let iso = &result.tree[0];
        let mib2 = iso.children.iter().find(|n| n.id == "mib-2").expect("mib-2 under iso");
        let interfaces = mib2.children.iter().find(|n| n.id == "interfaces").expect("interfaces group");
        let if_table = interfaces.children.iter().find(|n| n.id == "ifTable").expect("ifTable node");
        assert_eq!(if_table.kind, NodeKind::Table);
        assert!(if_table.children.is_empty(), "table columns/entry should not appear as tree nodes");
    }

    #[test]
    fn iso_is_the_sole_tree_root_and_unrelated_roots_are_filtered_out() {
        let dir = write_fixture(IF_TABLE_MIB);
        let result = parse_directories(&[dir.path().to_string_lossy().to_string()]);

        assert_eq!(result.tree.len(), 1, "tree should have exactly one root");
        let iso = &result.tree[0];
        assert_eq!(iso.id, "iso");
        assert_eq!(iso.oid, "1");
        assert!(iso.resolved);

        // `enterprises ::= { private 1 }` never resolves (`private` isn't declared
        // anywhere in the fixture), so it can't be confirmed to live under iso and
        // must not appear anywhere in the tree - neither as a root nor nested.
        fn contains(nodes: &[MibTreeNode], id: &str) -> bool {
            nodes.iter().any(|n| n.id == id || contains(&n.children, id))
        }
        assert!(!contains(&result.tree, "enterprises"), "unresolved non-iso root should be filtered out");
    }

    #[test]
    fn table_columns_come_from_the_sequence_definition_in_order_with_arcs() {
        let dir = write_fixture(IF_TABLE_MIB);
        let result = parse_directories(&[dir.path().to_string_lossy().to_string()]);

        let table = result.tables.get("ifTable").expect("ifTable definition");
        assert!(table.resolved);
        assert_eq!(table.oid, "1.3.6.1.2.1.2.2");
        let names: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["ifIndex", "ifDescr", "ifOperStatus"]);
        let if_descr = table.columns.iter().find(|c| c.name == "ifDescr").unwrap();
        assert_eq!(if_descr.arc, Some(2));
        let if_oper = table.columns.iter().find(|c| c.name == "ifOperStatus").unwrap();
        assert_eq!(if_oper.enum_values, vec![(1, "up".to_string()), (2, "down".to_string()), (3, "testing".to_string())]);
    }

    /// Locks down the wire shape the TypeScript frontend actually deserializes:
    /// `type` (not `kind`), camelCase field names, lowercase enum variants.
    #[test]
    fn json_shape_matches_what_the_frontend_expects() {
        let dir = write_fixture(IF_TABLE_MIB);
        let result = parse_directories(&[dir.path().to_string_lossy().to_string()]);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains(r#""type":"table""#), "{json}");
        assert!(json.contains(r#""resolved":true"#), "{json}");
        assert!(!json.contains("sequence_of_target"), "internal snake_case field leaked: {json}");
        assert!(json.contains(r#""enumValues""#), "{json}");
    }
}
