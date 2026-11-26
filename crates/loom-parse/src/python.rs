use std::collections::HashSet;

use tree_sitter::Node;

use crate::{Parsed, child, paths, text, visit};

pub(super) fn extract(
    root: Node<'_>,
    src: &str,
    path: &str,
    packages: Option<&HashSet<String>>,
    out: &mut Parsed,
) {
    let dir = paths::parent(path);
    let package_root = packages.map(|packages| {
        let mut root = dir;
        while !root.is_empty() && packages.contains(root) {
            root = paths::parent(root);
        }
        root
    });
    let mut exports = Exports::Convention;
    visit(root, |n| {
        if out.syntax_error {
            return;
        }
        // Python grammar 为编辑中源码容许空 block、无处理分支的 try；它们不能产出事实。
        let incomplete = match n.kind() {
            "block" => !n.named_children(&mut n.walk()).any(|s| !s.is_extra()),
            "try_statement" => !n.named_children(&mut n.walk()).any(|s| {
                matches!(
                    s.kind(),
                    "except_clause" | "except_group_clause" | "finally_clause"
                )
            }),
            _ => false,
        };
        if incomplete {
            *out = Parsed {
                syntax_error: true,
                ..Parsed::default()
            };
            return;
        }
        if n.kind() == "identifier" && text(n, src) == "__all__" {
            exports.observe(n, root, src);
        }
        let top = n.parent() == Some(root);
        match n.kind() {
            "import_statement" | "import_from_statement" => {
                let first = out.refs.len();
                if n.kind() == "import_statement" {
                    let mut c = n.walk();
                    for name in n.children_by_field_name("name", &mut c) {
                        if let Some(parts) = dotted(name, src) {
                            absolute_ref(n, &parts, dir, package_root, out);
                        } else {
                            out.unresolved("import", n);
                        }
                    }
                } else {
                    from_import(n, src, dir, package_root, out);
                }
                if !top {
                    // 分支、函数体或类体里的导入不一定执行，不构成无条件的声明链：
                    // 候选照常给出，但即使是相对导入也只算“可能”。
                    for r in &mut out.refs[first..] {
                        r.exact = false;
                    }
                }
            }
            "future_import_statement" => {
                out.reference("__future__", "external", n, Vec::new(), false)
            }
            "call" => {
                if let Some(f) = n.child_by_field_name("function") {
                    let name = if f.kind() == "attribute" {
                        f.child_by_field_name("attribute")
                    } else {
                        Some(f)
                    };
                    if name.is_some_and(|s| {
                        s.kind() == "identifier"
                            && matches!(text(s, src), "__import__" | "import_module")
                    }) {
                        out.unresolved("import", n);
                    }
                }
            }
            "decorated_definition"
            | "function_definition"
            | "class_definition"
            | "expression_statement"
                if top =>
            {
                symbol(n, src, out)
            }
            _ => (),
        }
    });
    out.symbols.retain(|s| match &exports {
        Exports::Convention => !s.name.starts_with('_'),
        Exports::Listed(names) => names.contains(s.name.as_str()),
        Exports::Unknown => false,
    });
}

fn dotted<'s>(mut n: Node<'_>, src: &'s str) -> Option<Vec<&'s str>> {
    if n.kind() == "aliased_import" {
        n = n.child_by_field_name("name")?;
    }
    if n.kind() != "dotted_name" {
        return None;
    }
    let mut c = n.walk();
    let components: Vec<_> = n
        .named_children(&mut c)
        .filter(|s| s.kind() == "identifier")
        .map(|s| text(s, src))
        .collect();
    (!components.is_empty()).then_some(components)
}

fn from_import(n: Node<'_>, src: &str, dir: &str, package_root: Option<&str>, out: &mut Parsed) {
    let Some(module) = n.child_by_field_name("module_name") else {
        out.unresolved("import", n);
        return;
    };
    if let Some(parts) = dotted(module, src) {
        absolute_ref(n, &parts, dir, package_root, out);
        return;
    }
    let Some(prefix) = child(module, "import_prefix") else {
        out.unresolved("import", n);
        return;
    };
    let mut c = prefix.walk();
    let level: usize = prefix
        .children(&mut c)
        .map(|s| match s.kind() {
            "." => 1,
            "..." => 3,
            _ => 0,
        })
        .sum();
    if level == 0 {
        out.unresolved("import", n);
        return;
    }
    let mut package = dir;
    for _ in 1..level {
        if package.is_empty() {
            out.unresolved("import", n);
            return;
        }
        package = paths::parent(package);
    }
    if let Some(base) = child(module, "dotted_name").and_then(|s| dotted(s, src)) {
        // 导入成员不是文件名；目标为 from 后的模块。
        module_ref(n, &base, package, level, out);
    } else if child(n, "wildcard_import").is_some() {
        out.reference(
            "*",
            "import",
            n,
            vec![paths::join(package, "__init__.py")],
            !package.is_empty(),
        );
    } else {
        // `from . import x` 既可能是包属性，也可能加载子模块。不能择一。
        for name in n.children_by_field_name("name", &mut c) {
            if let Some(parts) = dotted(name, src) {
                let mut files = vec![paths::join(package, "__init__.py")];
                files.extend(paths::module_pair(
                    package,
                    &parts.join("/"),
                    "py",
                    "__init__.py",
                ));
                out.reference(&parts.join("."), "import", n, files, false);
            } else {
                out.unresolved("import", n);
            }
        }
    }
}

/// 绝对导入取决于 sys.path，永远只是“可能”。除项目根和已知包根外，按词法补两类常见根。
/// 一是 `src/` 根：使用方所在的各层 `src` 目录，以及不在任何 `src` 目录内的祖先之下的
/// `src/`。`src` 树内部不再向下猜更深的 `src/`，否则每处导入的候选随目录深度增长，
/// 而用得上它的只有“`src` 树里另有一个不是使用方祖先的 `src/`”这种少见布局。
/// 二是导入至少两段时，与首段同名的祖先目录的上一级。单段导入不走这一类：
/// 包装外部库的目录常与该库同名（`providers/openai/` 里的 `import openai`），
/// 仅凭目录名会把外部依赖连到本地包。不访问文件系统。
fn absolute_ref(
    n: Node<'_>,
    parts: &[&str],
    dir: &str,
    package_root: Option<&str>,
    out: &mut Parsed,
) {
    let name = parts.join("/");
    let mut bases = vec![String::new()];
    let mut add = |base: String| {
        if !bases.contains(&base) {
            bases.push(base);
        }
    };
    for d in std::iter::successors(Some(dir), |&d| (!d.is_empty()).then(|| paths::parent(d))) {
        let last = d.rsplit('/').next().unwrap_or_default();
        if last == "src" {
            add(d.to_owned());
        } else if d.split('/').all(|s| s != "src") {
            add(paths::join(d, "src"));
        }
        if parts.len() > 1 && !d.is_empty() && last == parts[0] {
            add(paths::parent(d).to_owned());
        }
    }
    if let Some(root) = package_root {
        add(root.to_owned());
    }
    let files = bases
        .iter()
        .flat_map(|base| paths::module_pair(base, &name, "py", "__init__.py"))
        .collect();
    out.reference(&parts.join("."), "import", n, files, false);
}

fn module_ref(n: Node<'_>, parts: &[&str], package: &str, level: usize, out: &mut Parsed) {
    let files = paths::module_pair(package, &parts.join("/"), "py", "__init__.py");
    let label = format!("{}{}", ".".repeat(level), parts.join("."));
    // 只用于相对导入；到达项目根时无法证明根目录自身是包。
    out.reference(&label, "import", n, files, !package.is_empty());
}

fn symbol(mut n: Node<'_>, src: &str, out: &mut Parsed) {
    if n.kind() == "decorated_definition" {
        let Some(def) = n.child_by_field_name("definition") else {
            return;
        };
        n = def;
    }
    let (name, kind) = match n.kind() {
        "function_definition" => (n.child_by_field_name("name"), "function"),
        "class_definition" => (n.child_by_field_name("name"), "class"),
        "expression_statement" => (
            child(n, "assignment").and_then(|a| a.child_by_field_name("left")),
            "variable",
        ),
        _ => return,
    };
    if let Some(name) = name.filter(|s| s.kind() == "identifier") {
        out.symbol(text(name, src), kind, name);
    }
}

enum Exports<'s> {
    Convention,
    Listed(HashSet<&'s str>),
    Unknown,
}

impl<'s> Exports<'s> {
    /// 只接受单次、顶层、字面量列表/元组。其他读写（含条件和 append）一律不推断导出表。
    fn observe(&mut self, name: Node<'_>, root: Node<'_>, src: &'s str) {
        let listed = || {
            let a = name.parent()?;
            if a.kind() != "assignment"
                || a.child_by_field_name("left")? != name
                || a.parent()?.parent()? != root
            {
                return None;
            }
            let rhs = a.child_by_field_name("right")?;
            if !matches!(rhs.kind(), "list" | "tuple") {
                return None;
            }
            let mut c = rhs.walk();
            let mut names = HashSet::new();
            for s in rhs.named_children(&mut c).filter(|s| !s.is_extra()) {
                if s.kind() != "string" {
                    return None;
                }
                let raw = text(s, src);
                let (first, rest) = raw.split_at_checked(1)?;
                if !matches!(first, "'" | "\"") {
                    return None;
                }
                let v = rest.strip_suffix(first)?;
                if v.is_empty() || !v.chars().all(|c| c == '_' || c.is_alphanumeric()) {
                    return None;
                }
                names.insert(v);
            }
            Some(names)
        };
        *self = match self {
            Self::Convention => listed().map_or(Self::Unknown, Self::Listed),
            _ => Self::Unknown,
        };
    }
}
