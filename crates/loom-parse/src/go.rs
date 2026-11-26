//! Go 的导入指向整个包，也就是一个目录，而不是文件；导入路径要靠 go.mod 声明的模块路径
//! 才能对上目录。这里把“哪个文件”落到两类名字上：经包名用到的导出名，和同包文件里的
//! 顶层名字。模块路径由调用方读 go.mod 后传入，这里不碰文件系统。

use std::collections::{BTreeMap, HashMap, HashSet};

use tree_sitter::Node;

use crate::{Parsed, child, line, member, paths, text, visit};

/// 预声明的类型、常量和零值，以及实际没有人在包里重新声明的内建函数；它们不按同包名字去找。
/// 其余内建函数照常去找：包里自带 `min`、`max`、`new`、`copy` 的并不少见。
const PREDECLARED: &[&str] = &[
    "any",
    "append",
    "bool",
    "byte",
    "cap",
    "comparable",
    "complex128",
    "complex64",
    "error",
    "false",
    "float32",
    "float64",
    "int",
    "int16",
    "int32",
    "int64",
    "int8",
    "iota",
    "len",
    "make",
    "nil",
    "panic",
    "recover",
    "rune",
    "string",
    "true",
    "uint",
    "uint16",
    "uint32",
    "uint64",
    "uint8",
    "uintptr",
];

/// 文件名里能充当构建约束的系统和架构后缀（`go/build` 的已知清单）。
const SYSTEMS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "hurd",
    "illumos",
    "ios",
    "js",
    "linux",
    "nacl",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
    "zos",
    "386",
    "amd64",
    "amd64p32",
    "arm",
    "arm64",
    "arm64be",
    "armbe",
    "loong64",
    "mips",
    "mips64",
    "mips64le",
    "mips64p32",
    "mips64p32le",
    "mipsle",
    "ppc",
    "ppc64",
    "ppc64le",
    "riscv",
    "riscv64",
    "s390",
    "s390x",
    "sparc",
    "sparc64",
    "wasm",
];

/// 项目里的 Go 模块：go.mod 所在目录和它声明的模块路径，目录深的在前。
#[derive(Default)]
pub(crate) struct Modules(Vec<(String, Option<String>)>);

/// `inner` 就是 `outer`，或在它之下。空的 `outer` 是项目根。
fn within(inner: &str, outer: &str) -> bool {
    outer.is_empty()
        || inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl Modules {
    pub(crate) fn new(list: impl IntoIterator<Item = (String, Option<String>)>) -> Self {
        let mut list: Vec<_> = list
            .into_iter()
            .map(|(dir, module)| (dir.replace('\\', "/").trim_matches('/').to_owned(), module))
            .collect();
        list.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Self(list)
    }

    /// 目录所属的模块（最近的带 go.mod 的祖先）和它对应的导入路径。
    /// 最近的 go.mod 读不出模块路径时没有结论，不退到外层模块。
    fn home(&self, dir: &str) -> Option<(&str, String)> {
        let (root, module) = self.0.iter().find(|(root, _)| within(dir, root))?;
        let module = module.as_deref()?;
        let rest = dir[root.len()..].trim_start_matches('/');
        let import = if rest.is_empty() {
            module.to_owned()
        } else {
            format!("{module}/{rest}")
        };
        Some((root, import))
    }

    /// 导入路径落在项目里某个模块之下。
    fn local(&self, import: &str) -> bool {
        self.0
            .iter()
            .filter_map(|(_, module)| module.as_deref())
            .any(|module| within(import, module))
    }

    /// 有读不出模块路径的 go.mod：对不上的导入也可能属于它，不能断定是外部的。
    fn opaque(&self) -> bool {
        self.0.iter().any(|(_, module)| module.is_none())
    }

    /// 包里的每个非测试文件都按所在目录的导入路径提供一个键，没能解析的文件也算。
    pub(crate) fn path_keys(&self, path: &str) -> Vec<String> {
        if path.ends_with("_test.go") {
            return Vec::new();
        }
        self.home(paths::parent(path))
            .map(|(_, import)| format!("go:{import}"))
            .into_iter()
            .collect()
    }
}

/// 可以原样写进键里的导入路径：没有空段，也没有 `.`、`..`。
fn import_path(p: &str) -> bool {
    paths::literal(p) && p.split('/').all(|s| !matches!(s, "" | "." | ".."))
}

/// go.mod 里 `module` 指令声明的模块路径。
pub fn module_path(text: &str) -> Option<String> {
    let mut block = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.split("//").next().unwrap_or_default().trim();
        let value = if block {
            if line.is_empty() {
                continue;
            }
            line
        } else {
            let Some(rest) = line.strip_prefix("module") else {
                continue;
            };
            if !rest.starts_with(|c: char| c.is_whitespace() || "(\"`".contains(c)) {
                continue;
            }
            let rest = rest.trim();
            if rest == "(" {
                block = true;
                continue;
            }
            rest
        };
        let value = ['"', '`']
            .iter()
            .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(value);
        return import_path(value).then(|| value.to_owned());
    }
    None
}

/// 没写别名时，包名取自被导入包自己的 package 子句，这里看不到；按通行约定从路径推测，
/// 只用来把“名字.成员”归到某条导入上。真实包名写在声明键里，推测错了只会对不上，不会连错。
fn assumed_name(path: &str) -> &str {
    let mut parts = path.rsplit('/');
    let mut base = parts.next().unwrap_or(path);
    if let Some(digits) = base.strip_prefix('v')
        && !digits.is_empty()
        && digits.bytes().all(|c| c.is_ascii_digit())
        && let Some(previous) = parts.next()
    {
        base = previous;
    }
    let base = base.strip_prefix("go-").unwrap_or(base);
    let end = base
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(base.len());
    &base[..end]
}

fn exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// 文件名或 package 子句之前的构建注释让文件只在某些构建里出现，甚至从不参与构建。
fn constrained(path: &str, root: Node<'_>, src: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    // 以下划线开头的文件，go 工具直接跳过。
    if file.starts_with('_') {
        return true;
    }
    let stem = file.strip_suffix(".go").unwrap_or(file);
    let stem = stem.strip_suffix("_test").unwrap_or(stem);
    // 第一个下划线之前的部分不算：`linux.go` 没有约束，`open_linux.go` 才有。
    if let Some((_, rest)) = stem.split_once('_')
        && rest
            .rsplit('_')
            .next()
            .is_some_and(|last| SYSTEMS.contains(&last))
    {
        return true;
    }
    let mut c = root.walk();
    for n in root.named_children(&mut c) {
        if n.kind() != "comment" {
            break;
        }
        let line = text(n, src);
        if line.starts_with("//go:build")
            || line
                .strip_prefix("//")
                .is_some_and(|rest| rest.trim_start().starts_with("+build"))
        {
            return true;
        }
    }
    false
}

struct Import<'s> {
    path: &'s str,
    line: usize,
    /// 别名把名字直接绑定到这个包；没有别名时名字只是推测。
    alias: bool,
    local: bool,
    /// 已经由成员或搜索键交代了去向，不必再指向整个包。
    placed: bool,
}

/// 声明里可以成组出现的各项：`var ( … )` 多包一层列表。
fn specs<'t>(decl: Node<'t>) -> Vec<Node<'t>> {
    let mut found = Vec::new();
    let mut c = decl.walk();
    for s in decl.named_children(&mut c) {
        if s.kind().ends_with("_list") {
            let mut inner = s.walk();
            found.extend(s.named_children(&mut inner));
        } else {
            found.push(s);
        }
    }
    found
}

/// `:=`、range 和类型分支左边的名字。
fn bind<'s>(list: Option<Node<'_>>, src: &'s str, declared: &mut HashSet<&'s str>) {
    let Some(list) = list else {
        return;
    };
    let mut c = list.walk();
    for s in list.named_children(&mut c) {
        if s.kind() == "identifier" {
            declared.insert(text(s, src));
        }
    }
}

/// 先序遍历具名节点，同时告知节点是否在某个函数体里。Go 里只有函数体（含函数字面量）
/// 和其中的语句用 `block`，包级的声明、签名和初始化表达式都在任何 `block` 之外。
fn walk<'t>(root: Node<'t>, mut f: impl FnMut(Node<'t>, bool)) {
    let mut c = root.walk();
    let mut blocks = 0usize;
    loop {
        let n = c.node();
        if n.kind() == "block" {
            blocks += 1;
        }
        if n.is_named() {
            f(n, blocks > 0);
        }
        if c.goto_first_child() {
            continue;
        }
        loop {
            if c.node().kind() == "block" {
                blocks -= 1;
            }
            if c.goto_next_sibling() {
                break;
            }
            if !c.goto_parent() {
                return;
            }
        }
    }
}

/// 文件里声明过的名字。函数体里的一处使用可能被任何一处局部声明遮住，这里不分析块作用域，
/// 文件里任何地方声明过就算遮住；包级位置上的使用只可能被包级声明和类型参数遮住。
#[derive(Default)]
struct Declared<'s> {
    anywhere: HashSet<&'s str>,
    package: HashSet<&'s str>,
    /// 处在声明位置上的标识符节点，它们不是对名字的使用。
    binders: HashSet<usize>,
}

impl<'s> Declared<'s> {
    fn bind(&mut self, name: Node<'_>, src: &'s str, package: bool) {
        let label = text(name, src);
        self.anywhere.insert(label);
        self.binders.insert(name.id());
        if package {
            self.package.insert(label);
        }
    }

    fn shadows(&self, name: &str, in_body: bool) -> bool {
        if in_body {
            self.anywhere.contains(name)
        } else {
            self.package.contains(name)
        }
    }
}

pub(super) fn extract(root: Node<'_>, src: &str, path: &str, modules: &Modules, out: &mut Parsed) {
    let Some(package) = child(root, "package_clause").and_then(|c| child(c, "package_identifier"))
    else {
        out.syntax_error = true;
        return;
    };
    let package = text(package, src);
    let dir = paths::parent(path);
    let test = path.ends_with("_test.go");
    let home = modules.home(dir);
    // 外部测试包（`store_test`）与被测包的包内测试文件一起编译，用得到后者导出的名字。
    let tested = package.strip_suffix("_test").filter(|_| test);
    let own_tests = |import: &str| {
        let tested = tested?;
        let (_, own) = home.as_ref()?;
        (own == import).then(|| format!("gotest:{import}!{tested}"))
    };
    let mut conditional = constrained(path, root, src);
    // 同一目录、同一个包名的文件共用包作用域，互相不经导入就能用到对方的顶层名字。
    let siblings = format!("gop:{dir}!{package}");
    out.provides.push(siblings.clone());
    if let Some((_, import)) = &home {
        if !test {
            out.provides.push(format!("go:{import}!{package}"));
        } else if tested.is_none() {
            // 包内测试文件导出的名字只有同目录的外部测试包用得到。
            out.provides.push(format!("gotest:{import}!{package}"));
        }
    }

    let mut imports: Vec<Import<'_>> = Vec::new();
    let mut names: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut c = root.walk();
    for decl in root.named_children(&mut c) {
        if decl.kind() != "import_declaration" {
            continue;
        }
        for spec in specs(decl) {
            if spec.kind() != "import_spec" {
                continue;
            }
            let literal = spec
                .child_by_field_name("path")
                .and_then(|p| p.named_child(0).filter(|_| p.named_child_count() == 1))
                .filter(|s| {
                    matches!(
                        s.kind(),
                        "interpreted_string_literal_content" | "raw_string_literal_content"
                    )
                })
                .map(|s| text(s, src))
                .filter(|p| import_path(p));
            let Some(import) = literal else {
                out.unresolved("import", spec);
                continue;
            };
            // `import "C"` 是 cgo 的伪包；用了它的文件只在启用 cgo 时参与构建。
            let cgo = import == "C";
            conditional |= cgo;
            let local = !cgo && home.is_some() && modules.local(import);
            let binding = spec.child_by_field_name("name");
            let dot = binding.is_some_and(|n| n.kind() == "dot");
            let bound = match binding {
                None if cgo => Some(("C", true)),
                None => Some((assumed_name(import), false)),
                Some(n) if n.kind() == "package_identifier" => Some((text(n, src), true)),
                // `.` 把导出名直接放进文件作用域，`_` 只为副作用导入：都没有可跟踪的包名。
                Some(_) => None,
            };
            if let Some((name, _)) = bound.filter(|(name, _)| !name.is_empty()) {
                names.entry(name).or_default().push(imports.len());
            }
            if local && dot {
                // 点导入的名字和同包的名字写法一样，按名字到那个包里去找。
                out.search.push(format!("go:{import}"));
                out.search.extend(own_tests(import));
            } else if cgo || (!local && home.is_some() && !modules.opaque()) {
                // 不在项目里任何模块的路径之下：标准库或第三方包。
                out.reference(import, "external", spec, Vec::new(), false);
            } else if !local {
                // 所在目录不属于项目里可辨认的模块，导入路径对不上目录。
                out.unresolved(import, spec);
            }
            imports.push(Import {
                path: import,
                line: line(spec),
                alias: bound.is_some_and(|(_, alias)| alias),
                local,
                placed: dot,
            });
        }
    }
    // 受构建约束的文件不一定参与构建：不给它所属的根，它声明的和它用到的名字就都只算可能。
    if !conditional && let Some((module_root, _)) = &home {
        out.domain = Some((*module_root).to_owned());
    }

    let mut declared = Declared::default();
    // 经包名写出的成员：包名、成员、行号、是否在类型位置、是否在函数体里。
    let mut uses: Vec<(&str, &str, usize, bool, bool)> = Vec::new();
    let mut bare: Vec<(&str, usize, bool)> = Vec::new();
    walk(root, |n, in_body| {
        let mut c = n.walk();
        match n.kind() {
            // 所有能引入名字的写法。
            "const_spec" | "var_spec" | "function_declaration" | "type_spec" | "type_alias" => {
                for name in n.children_by_field_name("name", &mut c) {
                    declared.bind(name, src, !in_body);
                }
            }
            "parameter_declaration" | "variadic_parameter_declaration" => {
                for name in n.children_by_field_name("name", &mut c) {
                    declared.bind(name, src, false);
                }
            }
            // 类型参数在整个签名和类型体里都看得见。
            "type_parameter_declaration" => {
                for name in n.children_by_field_name("name", &mut c) {
                    declared.bind(name, src, true);
                }
            }
            // 接收者类型里写的类型实参其实是在声明类型参数：`func (s *Set[T]) Add(v T)`。
            "method_declaration" => {
                if let Some(receiver) = n.child_by_field_name("receiver") {
                    visit(receiver, |arguments| {
                        if arguments.kind() == "type_arguments" {
                            visit(arguments, |name| {
                                if name.kind() == "type_identifier" {
                                    declared.bind(name, src, true);
                                }
                            });
                        }
                    });
                }
            }
            "short_var_declaration" | "range_clause" | "receive_statement" => {
                bind(n.child_by_field_name("left"), src, &mut declared.anywhere)
            }
            "type_switch_statement" => {
                bind(n.child_by_field_name("alias"), src, &mut declared.anywhere)
            }
            // 类型位置上的 `包.名字` 只可能是包限定：Go 没有嵌套类型。
            "qualified_type" => {
                if let (Some(p), Some(item)) = (
                    n.child_by_field_name("package"),
                    n.child_by_field_name("name"),
                ) {
                    uses.push((text(p, src), text(item, src), line(n), true, in_body));
                }
            }
            "selector_expression" => {
                if let (Some(operand), Some(item)) = (
                    n.child_by_field_name("operand"),
                    n.child_by_field_name("field"),
                ) && operand.kind() == "identifier"
                {
                    uses.push((text(operand, src), text(item, src), line(n), false, in_body));
                }
            }
            "identifier" | "type_identifier" if !declared.binders.contains(&n.id()) => {
                let skip = n.parent().is_some_and(|p| match p.kind() {
                    "qualified_type" => true,
                    // 复合字面量里的键多半是字段名，不是对名字的引用。
                    "literal_element" => p.parent().is_some_and(|k| {
                        k.kind() == "keyed_element" && k.child_by_field_name("key") == Some(p)
                    }),
                    _ => false,
                });
                if !skip {
                    bare.push((text(n, src), line(n), in_body));
                }
            }
            _ => (),
        }
    });

    // 经包名用到的导出名。名字另有声明时，表达式里的 `名字.成员` 可能是字段访问，
    // 只认类型位置上的写法。
    let mut members: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (name, item, at, typed, in_body) in uses {
        let Some(bound) = names.get(name) else {
            continue;
        };
        if !exported(item)
            || (!typed && declared.shadows(name, in_body))
            || bound.iter().any(|&i| !imports[i].local)
        {
            continue;
        }
        for &i in bound {
            imports[i].placed = true;
        }
        members
            .entry((name, item))
            .and_modify(|first| *first = (*first).min(at))
            .or_insert(at);
    }
    for ((name, item), at) in members {
        let mut candidates = Vec::new();
        for import in names[name].iter().map(|&i| &imports[i]) {
            // 没写别名时，被导入包自己声明的包名也要对上。
            candidates.push(if import.alias {
                member(item, &format!("go:{}", import.path))
            } else {
                member(item, &format!("go:{}!{name}", import.path))
            });
            candidates.extend(own_tests(import.path).map(|key| member(item, &key)));
        }
        out.reference_at(&format!("{name}.{item}"), "use", at, candidates, true);
    }
    // 没有可跟踪成员的导入只能指向整个包：包里只有一个文件时才落得到文件上。
    for import in imports.iter().filter(|i| i.local && !i.placed) {
        out.reference_at(
            import.path,
            "import",
            import.line,
            vec![format!("go:{}", import.path)],
            true,
        );
    }

    // 没有被遮住、也不是导入名的名字，只能来自包作用域：同包的另一个文件，或预声明。
    // 同包的文件一起编译，对上了唯一的声明就是确定的；经点导入找到的由索引另行降为可能。
    let mut seen = HashSet::new();
    for (name, at, in_body) in bare {
        if name != "_"
            && !declared.shadows(name, in_body)
            && !names.contains_key(name)
            && !PREDECLARED.contains(&name)
            && seen.insert(name)
        {
            out.reference_at(name, "mention", at, vec![member(name, &siblings)], true);
        }
    }

    let mut c = root.walk();
    for decl in root.named_children(&mut c) {
        let kind = match decl.kind() {
            "function_declaration" => "function",
            "type_declaration" => "type",
            "var_declaration" => "variable",
            "const_declaration" => "constant",
            _ => continue,
        };
        let holders = if kind == "function" {
            vec![decl]
        } else {
            specs(decl)
        };
        for holder in holders {
            let mut c = holder.walk();
            for name in holder.children_by_field_name("name", &mut c) {
                let label = text(name, src);
                // 方法不在包作用域里；`init` 可以有多个，也不能被引用。
                if label != "_" && !(kind == "function" && label == "init") {
                    out.declare(label, kind, name, exported(label));
                }
            }
        }
    }
    out.refs.sort_by_key(|r| r.line);
}
