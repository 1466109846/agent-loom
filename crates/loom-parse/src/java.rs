//! Java 的引用按“包 + 顶层类型”找文件，不按路径：导入写的是限定名，同包的类型连导入都不用。
//! 文件用自己的 package 声明给出所在的包，引用方按名字去对。

use std::collections::HashSet;

use tree_sitter::Node;

use crate::{Parsed, child, line, member, paths, text, visit};

const TYPE_DECLARATIONS: &[(&str, &str)] = &[
    ("class_declaration", "class"),
    ("interface_declaration", "interface"),
    ("enum_declaration", "enum"),
    ("record_declaration", "record"),
    ("annotation_type_declaration", "annotation"),
];

/// `java.lang` 里随处可见、实际没有人在自己包里重新声明的类型；它们不按同包的名字去找。
const PLATFORM: &[&str] = &[
    "Boolean",
    "Byte",
    "CharSequence",
    "Character",
    "Class",
    "Comparable",
    "Deprecated",
    "Double",
    "Enum",
    "Exception",
    "Float",
    "FunctionalInterface",
    "IllegalArgumentException",
    "IllegalStateException",
    "Integer",
    "Iterable",
    "Long",
    "Math",
    "Number",
    "Object",
    "Override",
    "Runnable",
    "RuntimeException",
    "SafeVarargs",
    "Short",
    "String",
    "StringBuilder",
    "SuppressWarnings",
    "System",
    "Thread",
    "Throwable",
    "Void",
];

fn type_kind(node: &str) -> Option<&'static str> {
    TYPE_DECLARATIONS
        .iter()
        .find(|(kind, _)| *kind == node)
        .map(|(_, label)| *label)
}

/// `a.b.C` 的各段；不是纯标识符的限定名时返回 None。
fn dotted<'s>(mut n: Node<'_>, src: &'s str) -> Option<Vec<&'s str>> {
    let mut parts = Vec::new();
    while n.kind() == "scoped_identifier" {
        parts.push(text(n.child_by_field_name("name")?, src));
        n = n.child_by_field_name("scope")?;
        if parts.len() > 64 {
            return None;
        }
    }
    if n.kind() != "identifier" {
        return None;
    }
    parts.push(text(n, src));
    parts.reverse();
    Some(parts)
}

fn package_key(package: &str) -> String {
    format!("java:{package}")
}

fn capitalized(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// 限定名 `a.b.C.D` 里包在哪一段结束，要看别处的声明，这里看不到。按通行的命名约定，
/// 第一个大写开头的段是顶层类型，后面是嵌套类型或成员；不合约定的名字再按“最后一段是类型”
/// 试一次。候选要对上实际的声明才成立，猜错了只会对不上。
fn qualified(parts: &[&str]) -> Vec<String> {
    let last = parts.len().saturating_sub(1);
    let conventional = parts.iter().position(|p| capitalized(p));
    let mut found = Vec::new();
    for i in [conventional, Some(last)].into_iter().flatten() {
        // 无名包里的类型没有限定名。
        if i > 0 {
            let candidate = member(parts[i], &package_key(&parts[..i].join(".")));
            if !found.contains(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

fn import<'s>(n: Node<'_>, src: &'s str, imported: &mut HashSet<&'s str>, out: &mut Parsed) {
    let (mut fixed, mut star, mut name) = (false, false, None);
    let mut c = n.walk();
    for s in n.children(&mut c) {
        match s.kind() {
            "static" => fixed = true,
            "asterisk" => star = true,
            "identifier" | "scoped_identifier" => name = Some(s),
            _ => (),
        }
    }
    let Some(parts) = name.and_then(|s| dotted(s, src)) else {
        out.unresolved("import", n);
        return;
    };
    let spelling = if star {
        format!("{}.*", parts.join("."))
    } else {
        parts.join(".")
    };
    // 单个导入把最后一段绑定成简单名字；用到它时不再按同包去找。
    if !star {
        imported.insert(parts[parts.len() - 1]);
    }
    if parts[0] == "java" {
        // `java.` 开头的包只能由平台提供，类加载器不接受别处定义的同名类。
        out.reference(&spelling, "external", n, Vec::new(), false);
        return;
    }
    if star && !fixed {
        // 按需导入本身不指向哪个文件：经它用到的类型按名字各自去找。
        // 它也可能是在导入某个类型的嵌套类型，这种可能只算一处提及。
        out.search.push(package_key(&parts.join(".")));
        out.reference(&spelling, "mention", n, qualified(&parts), false);
        return;
    }
    // 静态导入的最后一段是成员，前面才是类型。
    let owner = if fixed && !star {
        &parts[..parts.len() - 1]
    } else {
        &parts[..]
    };
    let candidates = qualified(owner);
    if candidates.is_empty() {
        // 无名包里的类型不能被导入。
        out.unresolved(&spelling, n);
    } else {
        out.reference(&spelling, "import", n, candidates, true);
    }
}

pub(super) fn extract(root: Node<'_>, src: &str, path: &str, out: &mut Parsed) {
    let dir = paths::parent(path);
    let mut package: Vec<&str> = Vec::new();
    let mut imported: HashSet<&str> = HashSet::new();
    let mut c = root.walk();
    for n in root.named_children(&mut c) {
        match n.kind() {
            "package_declaration" => {
                let mut c = n.walk();
                let name = n
                    .named_children(&mut c)
                    .find(|s| matches!(s.kind(), "identifier" | "scoped_identifier"));
                let Some(parts) = name.and_then(|s| dotted(s, src)) else {
                    out.syntax_error = true;
                    return;
                };
                package = parts;
            }
            "import_declaration" => import(n, src, &mut imported, out),
            // 模块声明里的 `uses` 和 `provides … with …` 写的是类型的全限定名。
            "module_declaration" => {
                let Some(body) = n.child_by_field_name("body") else {
                    continue;
                };
                let mut c = body.walk();
                for directive in body.named_children(&mut c).filter(|d| {
                    matches!(
                        d.kind(),
                        "uses_module_directive" | "provides_module_directive"
                    )
                }) {
                    let mut c = directive.walk();
                    for name in directive.named_children(&mut c) {
                        let Some(parts) = dotted(name, src) else {
                            continue;
                        };
                        let spelling = parts.join(".");
                        let candidates = qualified(&parts);
                        if parts[0] == "java" {
                            out.reference(&spelling, "external", name, Vec::new(), false);
                        } else if candidates.is_empty() {
                            out.unresolved(&spelling, name);
                        } else {
                            out.reference(&spelling, "import", name, candidates, true);
                        }
                    }
                }
            }
            kind => {
                if let Some(label) = type_kind(kind)
                    && let Some(name) = n.child_by_field_name("name")
                {
                    let mut c = n.walk();
                    let public = child(n, "modifiers")
                        .is_some_and(|m| m.children(&mut c).any(|s| s.kind() == "public"));
                    // 没写 public 的顶层类型只有同包看得见。
                    out.declare(text(name, src), label, name, public);
                }
            }
        }
    }
    // 源根是目录去掉包路径后剩下的部分；目录与包对不上时没有源根。
    let tail = package.join("/");
    let home = if tail.is_empty() {
        // 无名包没有名字可对：只有同一个目录里的文件算同包。
        out.domain = Some(dir.to_owned());
        format!("javadir:{dir}")
    } else {
        out.domain = if dir == tail {
            Some(String::new())
        } else {
            dir.strip_suffix(&tail)
                .and_then(|d| d.strip_suffix('/'))
                .map(str::to_owned)
        };
        package_key(&package.join("."))
    };
    out.provides.push(home.clone());

    // 文件里声明的类型和值会遮住同名的包成员；任何一处声明过的名字都不再按同包去找。
    let mut types: HashSet<&str> = HashSet::new();
    let mut values: HashSet<&str> = HashSet::new();
    // 名字链、行号、是否出现在表达式里（表达式里的名字也可能是变量）。
    let mut found: Vec<(Vec<&str>, usize, bool)> = Vec::new();
    visit(root, |n| {
        let named = |n: Node<'_>| n.child_by_field_name("name").map(|s| text(s, src));
        match n.kind() {
            "type_parameter" => types.extend(child(n, "type_identifier").map(|s| text(s, src))),
            "variable_declarator"
            | "formal_parameter"
            | "catch_formal_parameter"
            | "enum_constant"
            | "resource"
            | "enhanced_for_statement"
            | "instanceof_expression"
            | "annotation_type_element_declaration" => values.extend(named(n)),
            "inferred_parameters" | "record_pattern_component" => {
                let mut c = n.walk();
                for s in n.named_children(&mut c) {
                    if s.kind() == "identifier" {
                        values.insert(text(s, src));
                    }
                }
            }
            "lambda_expression" => values.extend(
                n.child_by_field_name("parameters")
                    .filter(|s| s.kind() == "identifier")
                    .map(|s| text(s, src)),
            ),
            "type_identifier" => {
                let Some(p) = n.parent() else {
                    return;
                };
                let mut chain = vec![text(n, src)];
                match p.kind() {
                    "type_parameter" if p.named_child(0) == Some(n) => return,
                    "scoped_type_identifier" => {
                        // 只有最左边一段是当前作用域里的名字，其余是它的成员。
                        if p.named_child(0) != Some(n) {
                            return;
                        }
                        let mut scope = p;
                        loop {
                            let mut c = scope.walk();
                            let Some(last) = scope
                                .named_children(&mut c)
                                .filter(|s| s.kind() == "type_identifier")
                                .last()
                            else {
                                break;
                            };
                            chain.push(text(last, src));
                            match scope.parent() {
                                Some(up)
                                    if up.kind() == "scoped_type_identifier"
                                        && up.named_child(0) == Some(scope)
                                        && chain.len() < 64 =>
                                {
                                    scope = up
                                }
                                _ => break,
                            }
                        }
                    }
                    _ => (),
                }
                found.push((chain, line(n), false));
            }
            "identifier" => {
                let Some(p) = n.parent() else {
                    return;
                };
                // 只看限定别的名字的位置：`名字.成员`、`名字::方法`、注解和记录模式的类型。
                let head = match p.kind() {
                    "method_invocation" | "field_access" => {
                        p.child_by_field_name("object") == Some(n)
                    }
                    "method_reference" | "record_pattern" => p.named_child(0) == Some(n),
                    "marker_annotation" | "annotation" => p.child_by_field_name("name") == Some(n),
                    _ => false,
                };
                if !head {
                    return;
                }
                let mut chain = vec![text(n, src)];
                let mut access = p;
                while access.kind() == "field_access" && chain.len() < 64 {
                    let Some(field) = access
                        .child_by_field_name("field")
                        .filter(|s| s.kind() == "identifier")
                    else {
                        break;
                    };
                    chain.push(text(field, src));
                    match access.parent() {
                        Some(up) if up.child_by_field_name("object") == Some(access) => access = up,
                        _ => break,
                    }
                }
                let value = !matches!(
                    p.kind(),
                    "marker_annotation" | "annotation" | "record_pattern"
                );
                found.push((chain, line(n), value));
            }
            // 注解写了限定名。
            "scoped_identifier"
                if n.parent()
                    .is_some_and(|p| matches!(p.kind(), "marker_annotation" | "annotation")) =>
            {
                if let Some(chain) = dotted(n, src) {
                    found.push((chain, line(n), false));
                }
            }
            kind => {
                if type_kind(kind).is_some() {
                    types.extend(named(n));
                }
            }
        }
    });

    // 这些名字只是“提及”：能在同包、按需导入的包或限定名里对上一个文件才成立，
    // 对不上的多半是平台或第三方的类型，不算缺口。
    let mut heads = HashSet::new();
    let mut chains = HashSet::new();
    for (chain, at, value) in found {
        let head = chain[0];
        if head == "var"
            || head == "java"
            || types.contains(head)
            || imported.contains(head)
            || (value && values.contains(head))
        {
            continue;
        }
        // 类型位置上单独出现的名字，和任何位置上大写开头的名字，按简单类型名去找。
        if capitalized(head) || (!value && chain.len() == 1) {
            if !PLATFORM.contains(&head) && heads.insert(head) {
                // 同包的类型遮住按需导入的同名类型；后者由索引在这里落空时再去找。
                out.reference_at(head, "mention", at, vec![member(head, &home)], false);
            }
            continue;
        }
        // 小写开头的链：在类型位置上一定是限定名；在表达式里多半是“变量.字段”，
        // 只有至少两段小写之后接一段大写的写法才当作限定名，取到类型为止。
        let (name, candidates) = if !value {
            (chain.join("."), qualified(&chain))
        } else {
            match chain.iter().position(|part| capitalized(part)) {
                Some(i) if i >= 2 => (
                    chain[..=i].join("."),
                    vec![member(chain[i], &package_key(&chain[..i].join(".")))],
                ),
                _ => continue,
            }
        };
        if !candidates.is_empty() && chains.insert(name.clone()) {
            out.reference_at(&name, "mention", at, candidates, false);
        }
    }
    out.refs.sort_by_key(|r| r.line);
}
