mod declarations;

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::{Parsed, child, paths, text, visit};

#[derive(Clone, Copy)]
enum Attr<'s> {
    Plain,
    Path(&'s str),
    Opaque,
}

impl<'s> Attr<'s> {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Plain, a) | (a, Self::Plain) => a,
            _ => Self::Opaque,
        }
    }
}

fn attribute<'s>(n: Node<'_>, src: &'s str) -> Attr<'s> {
    let Some(a) = child(n, "attribute") else {
        return Attr::Opaque;
    };
    match a.named_child(0).map(|key| text(key, src)) {
        Some("path") => {
            if let Some(value) = a.child_by_field_name("value")
                && matches!(value.kind(), "string_literal" | "raw_string_literal")
                && text(value, src).starts_with(['"', 'r'])
                && value.named_child_count() == 1
                && let Some(s) = child(value, "string_content")
            {
                Attr::Path(text(s, src))
            } else {
                Attr::Opaque
            }
        }
        Some(
            "allow" | "warn" | "deny" | "forbid" | "expect" | "doc" | "deprecated" | "inline"
            | "must_use" | "derive" | "repr" | "no_std" | "no_main",
        ) => Attr::Plain,
        // cfg/cfg_attr 和未知过程属性可能移除、替换或重定向整个 item。
        _ => Attr::Opaque,
    }
}

fn ident<'s>(n: Node<'_>, src: &'s str) -> &'s str {
    let s = text(n, src);
    s.strip_prefix("r#").unwrap_or(s)
}

fn public(n: Node<'_>) -> bool {
    child(n, "visibility_modifier")
        .is_some_and(|v| v.child_count() == 1 && v.child(0).is_some_and(|s| s.kind() == "pub"))
}

struct Scope {
    /// 无 path 属性的文件模块相对于所属模块目录，显式 path 相对于源码目录（内联作用域除外）。
    module_dir: String,
    attr_dir: String,
    crate_root: bool,
    enabled: bool,
}

impl Scope {
    fn file(path: &str) -> Self {
        let dir = paths::parent(path);
        let stem = path
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .strip_suffix(".rs")
            .unwrap_or("");
        let src_dir = dir == "src" || dir.ends_with("/src");
        let target_dir = |d: &str| {
            matches!(d.rsplit('/').next(), Some("examples" | "tests" | "benches"))
                || d == "src/bin"
                || d.ends_with("/src/bin")
        };
        let crate_root = ((dir.is_empty() || src_dir) && matches!(stem, "lib" | "main"))
            || path == "build.rs"
            || target_dir(dir)
            || (stem == "main" && target_dir(paths::parent(dir)));
        // 后缀本身安全不代表去后缀后的目录安全，例如 ...rs 的 stem 为 ..。
        let enabled = !stem.is_empty() && !stem.ends_with([' ', '.']);
        let module_dir = if !enabled || crate_root || stem == "mod" {
            dir.into()
        } else {
            paths::join(dir, stem)
        };
        Self {
            module_dir,
            attr_dir: dir.into(),
            crate_root,
            enabled,
        }
    }

    fn files(&self, attr: Attr<'_>, name: &str) -> Option<Vec<String>> {
        match attr {
            Attr::Plain => Some(paths::module_pair(&self.module_dir, name, "rs", "mod.rs")),
            Attr::Path(p) if !paths::directory_spec(p) => {
                paths::resolve(&self.attr_dir, p).map(|p| vec![p])
            }
            _ => None,
        }
    }
}

enum Binding {
    File(Vec<String>),
    Other,
    Conflict,
}
type Bindings<'s> = HashMap<&'s str, Binding>;

struct Item<'t, 's> {
    node: Node<'t>,
    attr: Attr<'s>,
    leaves: Vec<Leaf<'t>>,
}

fn scope_items<'t, 's>(scope_node: Node<'t>, scope: &mut Scope, src: &'s str) -> Vec<Item<'t, 's>> {
    let mut items = Vec::new();
    let mut attr = Attr::Plain;
    let mut c = scope_node.walk();
    for n in scope_node.named_children(&mut c) {
        match n.kind() {
            "attribute_item" => attr = attr.combine(attribute(n, src)),
            "inner_attribute_item" => scope.enabled &= matches!(attribute(n, src), Attr::Plain),
            "line_comment" | "block_comment" | "shebang" => (),
            _ => {
                let mut leaves = Vec::new();
                if let Some(arg) = n
                    .child_by_field_name("argument")
                    .filter(|_| n.kind() == "use_declaration")
                    && !use_tree(arg, &[], None, &mut leaves, 0)
                {
                    leaves.clear();
                }
                items.push(Item {
                    node: n,
                    attr,
                    leaves,
                });
                attr = Attr::Plain;
            }
        }
    }
    items
}

// 名字可先用后声明。显式导入也可能占用类型命名空间；函数、常量或宏不会遮蔽模块。
fn bindings<'s>(items: &[Item<'_, 's>], scope: &Scope, src: &'s str) -> Bindings<'s> {
    let mut names = Bindings::new();
    let mut bind = |name, binding| {
        names
            .entry(name)
            .and_modify(|old| *old = Binding::Conflict)
            .or_insert(binding);
    };
    for item in items {
        let n = item.node;
        match n.kind() {
            "use_declaration" => {
                for b in item.leaves.iter().filter_map(Leaf::binding) {
                    bind(ident(b, src), Binding::Other);
                }
            }
            "mod_item"
            | "extern_crate_declaration"
            | "struct_item"
            | "union_item"
            | "enum_item"
            | "type_item"
            | "trait_item" => {
                if let Some(name) = n
                    .child_by_field_name("alias")
                    .or_else(|| n.child_by_field_name("name"))
                {
                    let label = ident(name, src);
                    let files = if n.kind() == "mod_item"
                        && n.child_by_field_name("body").is_none()
                        && scope.enabled
                    {
                        scope.files(item.attr, label)
                    } else {
                        None
                    };
                    bind(label, files.map_or(Binding::Other, Binding::File));
                }
            }
            _ => (),
        }
    }
    names
}

pub(super) fn extract(root: Node<'_>, src: &str, path: &str, out: &mut Parsed) {
    let mut pending = vec![(root, Scope::file(path))];
    while let Some((scope_node, mut scope)) = pending.pop() {
        let items = scope_items(scope_node, &mut scope, src);
        let names = bindings(&items, &scope, src);
        let provable = declarations::collect(&items, &scope, scope_node == root, src, out);
        for item in items {
            let n = item.node;
            let top_public = scope_node == root
                && scope.enabled
                && !matches!(item.attr, Attr::Opaque)
                && public(n);
            match n.kind() {
                "mod_item" => {
                    let Some(name) = n.child_by_field_name("name") else {
                        continue;
                    };
                    let label = ident(name, src);
                    let binding = names.get(label);
                    if let Some(body) = n.child_by_field_name("body") {
                        let enabled = scope.enabled
                            && matches!(item.attr, Attr::Plain)
                            && !matches!(binding, Some(Binding::Conflict));
                        if top_public && enabled {
                            out.symbol(label, "module", name);
                        }
                        let dir = paths::join(&scope.module_dir, label);
                        pending.push((
                            body,
                            Scope {
                                module_dir: dir.clone(),
                                attr_dir: dir,
                                crate_root: false,
                                enabled,
                            },
                        ));
                    } else if let Some(Binding::File(files)) = binding {
                        if top_public {
                            out.symbol(label, "module", name);
                        }
                        out.reference(label, "module", n, files.clone(), true);
                    } else if scope.enabled
                        && let Some(files) = scope.files(item.attr, label)
                    {
                        out.reference(label, "module", n, files, false);
                    } else {
                        out.unresolved(label, n);
                    }
                }
                "use_declaration" => {
                    if scope.enabled && matches!(item.attr, Attr::Plain) && !item.leaves.is_empty()
                    {
                        imports(n, &item.leaves, src, &scope, &names, provable, out);
                    } else {
                        out.unresolved("use", n);
                    }
                }
                "extern_crate_declaration" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        let label = ident(name, src);
                        if label == "self" || !scope.enabled || !matches!(item.attr, Attr::Plain) {
                            out.unresolved("extern crate", n);
                        } else {
                            out.reference(label, "external", n, Vec::new(), false);
                        }
                        if top_public {
                            let alias = n.child_by_field_name("alias").unwrap_or(name);
                            out.symbol(ident(alias, src), "module", alias);
                        }
                    }
                }
                _ => {
                    if top_public
                        && let Some(kind) = declaration_kind(n.kind())
                        && let Some(name) = n.child_by_field_name("name")
                    {
                        out.symbol(ident(name, src), kind, name);
                    }
                    visit(n, |s| match s.kind() {
                        "mod_item" | "use_declaration" | "extern_crate_declaration" => {
                            out.unresolved("module", s)
                        }
                        "macro_invocation"
                            if s.child_by_field_name("macro")
                                .is_some_and(|m| text(m, src) == "include") =>
                        {
                            out.unresolved("include", s)
                        }
                        _ => (),
                    });
                }
            }
        }
    }
    out.refs.sort_by_key(|r| r.line);
}

fn declaration_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "function_item" => "function",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "union_item" => "union",
        "trait_item" => "trait",
        "type_item" => "type",
        "const_item" => "constant",
        "static_item" => "static",
        _ => return None,
    })
}

struct Leaf<'t> {
    path: Vec<Node<'t>>,
    alias: Option<Node<'t>>,
    glob: bool,
}

impl<'t> Leaf<'t> {
    fn binding(&self) -> Option<Node<'t>> {
        if self.glob {
            return None;
        }
        self.alias.or_else(|| {
            self.path
                .iter()
                .rev()
                .copied()
                .find(|s| s.kind() == "identifier")
        })
    }
}

// 仅消费 use 的语法字段。深度超限时整条 use 降级，不解析半棵树。
fn append_path<'t>(mut n: Node<'t>, path: &mut Vec<Node<'t>>) -> bool {
    let start = path.len();
    while n.kind() == "scoped_identifier" {
        let Some(name) = n.child_by_field_name("name") else {
            return false;
        };
        path.push(name);
        if path.len() > 64 {
            return false;
        }
        let Some(prefix) = n
            .child_by_field_name("path")
            .or_else(|| n.child(0).filter(|s| s.kind() == "::"))
        else {
            return false;
        };
        n = prefix;
    }
    if !matches!(n.kind(), "identifier" | "self" | "super" | "crate" | "::") {
        return false;
    }
    path.push(n);
    path[start..].reverse();
    true
}

fn use_tree<'t>(
    n: Node<'t>,
    prefix: &[Node<'t>],
    alias: Option<Node<'t>>,
    leaves: &mut Vec<Leaf<'t>>,
    depth: usize,
) -> bool {
    if depth > 64 {
        return false;
    }
    let mut path = prefix.to_vec();
    let mut c = n.walk();
    match n.kind() {
        "use_as_clause" => n
            .child_by_field_name("path")
            .is_some_and(|p| use_tree(p, &path, n.child_by_field_name("alias"), leaves, depth + 1)),
        "use_list" => n
            .named_children(&mut c)
            .filter(|s| !s.is_extra())
            .all(|s| use_tree(s, &path, None, leaves, depth + 1)),
        "scoped_use_list" => {
            if let Some(p) = n.child_by_field_name("path") {
                if !append_path(p, &mut path) {
                    return false;
                }
            } else if n.child(0).is_some_and(|s| s.kind() == "::") {
                return false;
            }
            n.child_by_field_name("list")
                .is_some_and(|s| use_tree(s, &path, None, leaves, depth + 1))
        }
        _ => {
            let glob = n.kind() == "use_wildcard";
            if glob {
                if let Some(p) = n.named_children(&mut c).find(|s| !s.is_extra())
                    && !append_path(p, &mut path)
                {
                    return false;
                }
            } else if !append_path(n, &mut path) {
                return false;
            }
            leaves.push(Leaf { path, alias, glob });
            true
        }
    }
}

fn imports(
    n: Node<'_>,
    leaves: &[Leaf<'_>],
    src: &str,
    scope: &Scope,
    names: &Bindings<'_>,
    provable: bool,
    out: &mut Parsed,
) {
    if n.parent().is_some_and(|p| p.kind() == "source_file") && public(n) {
        for b in leaves.iter().filter_map(Leaf::binding) {
            if ident(b, src) != "_" {
                out.symbol(ident(b, src), "export", b);
            }
        }
    }
    let mut seen_bindings = HashSet::new();
    let mut unique = true;
    for b in leaves.iter().filter_map(Leaf::binding) {
        let label = ident(b, src);
        unique &= label != "_"
            && seen_bindings.insert(label)
            && !matches!(names.get(label), Some(Binding::Conflict));
    }
    if provable
        && unique
        && let Some(paths) = leaves
            .iter()
            .map(|leaf| declarations::use_path(leaf, scope, names, src))
            .collect::<Option<Vec<_>>>()
    {
        let label = paths[0].split("::").nth(1).unwrap_or("use");
        let kind = if public(n) { "reexport" } else { "import" };
        out.reference(label, kind, n, Vec::new(), false);
        out.refs.last_mut().unwrap().rust_paths = paths;
        return;
    }
    // 保持原始引用口径：待定组按语句、首段计，而不是把每个叶子都算成新引用。
    let mut seen = HashSet::new();
    for leaf in leaves {
        let parts = match leaf.path.as_slice() {
            [first, rest @ ..]
                if first.kind() == "self" || (first.kind() == "crate" && scope.crate_root) =>
            {
                rest
            }
            other => other,
        };
        let label = parts
            .first()
            .filter(|s| s.kind() == "identifier")
            .map_or("use", |s| ident(*s, src));
        if seen.insert(label) {
            out.unresolved(label, n);
        }
    }
}
