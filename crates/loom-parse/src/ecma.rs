use tree_sitter::Node;

use crate::{Parsed, child, line, paths, text, tsconfig::Aliases, visit};

const EXTENSIONS: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts"];

/// 导入语句所在的位置：使用方的目录、它是不是 TypeScript、对它生效的别名。
#[derive(Clone, Copy)]
pub(super) struct Site<'a> {
    pub dir: &'a str,
    pub typescript: bool,
    pub aliases: Aliases<'a>,
}

#[derive(Clone, Copy)]
pub(super) enum Load {
    Import,
    Reexport,
    DynamicImport,
    Require,
}

pub(super) fn extract(root: Node<'_>, src: &str, site: Site<'_>, out: &mut Parsed) {
    visit(root, |n| match n.kind() {
        "import_statement" | "export_statement" => {
            let export = n.kind() == "export_statement";
            if !export && let Some(clause) = child(n, "import_require_clause") {
                literal_ref(
                    n,
                    clause.child_by_field_name("source"),
                    Load::Require,
                    site,
                    src,
                    out,
                );
                return;
            }
            if n.parent() != Some(root) {
                if !export || n.child_by_field_name("source").is_some() {
                    out.unresolved("import", n);
                }
                return;
            }
            if let Some(s) = n.child_by_field_name("source") {
                literal_ref(
                    n,
                    Some(s),
                    if export { Load::Reexport } else { Load::Import },
                    site,
                    src,
                    out,
                );
            } else if !export {
                out.unresolved("import", n);
            }
            if export {
                exported(n, src, out);
            }
        }
        "import_alias" => out.unresolved("import", n),
        "call_expression" => {
            let Some(callee) = n.child_by_field_name("function") else {
                return;
            };
            let load = match callee.kind() {
                "import" => Load::DynamicImport,
                "identifier" if text(callee, src) == "require" => Load::Require,
                _ => return,
            };
            let first = child(n, "arguments").and_then(|args| {
                args.named_children(&mut args.walk())
                    .find(|a| !a.is_extra())
            });
            literal_ref(n, first, load, site, src, out);
        }
        _ => (),
    });
}

fn literal_ref(
    n: Node<'_>,
    literal: Option<Node<'_>>,
    load: Load,
    site: Site<'_>,
    src: &str,
    out: &mut Parsed,
) {
    let spec = literal.and_then(|s| match s.kind() {
        "string" if s.named_child_count() == 1 => s
            .named_child(0)
            .filter(|f| f.kind() == "string_fragment")
            .map(|f| text(f, src)),
        "template_string"
            if s.named_children(&mut s.walk())
                .all(|f| f.kind() == "string_fragment") =>
        {
            let raw = text(s, src);
            Some(&raw[1..raw.len() - 1])
        }
        _ => None,
    });
    if let Some(spec) = spec {
        spec_ref(spec, load, line(n), site, out);
    } else {
        out.unresolved(
            if matches!(load, Load::Require) {
                "require"
            } else {
                "import"
            },
            n,
        );
    }
}

/// 一个已经落到项目里的目标能对上哪些文件，分别记进三组候选。返回它是不是写明了文件。
fn files(
    base: &str,
    directory: bool,
    typescript: bool,
    candidates: &mut Vec<String>,
    substitutes: &mut Vec<String>,
    later: &mut Vec<String>,
) -> bool {
    let explicit_file = !directory
        && base
            .rsplit('/')
            .next()
            .is_some_and(|name| name.trim_start_matches('.').contains('.'));
    if explicit_file {
        // TypeScript 允许以产物扩展名指向源码：`./x.js` 可能是 x.ts 或 x.tsx。只对 TS 使用方
        // 补充替代候选。书写的文件本身存在时仍是确定目标，两者并存则由索引报告多个候选。
        if typescript {
            match base.rsplit_once('.') {
                Some((stem, "js" | "jsx")) => {
                    substitutes.extend([format!("{stem}.ts"), format!("{stem}.tsx")])
                }
                Some((stem, "mjs")) => substitutes.push(format!("{stem}.mts")),
                Some((stem, "cjs")) => substitutes.push(format!("{stem}.cts")),
                _ => (),
            }
        }
        candidates.push(base.to_owned());
        return true;
    }
    let index = paths::join(base, "index");
    for ext in EXTENSIONS {
        if !directory {
            candidates.push(format!("{base}.{ext}"));
        }
        candidates.push(format!("{index}.{ext}"));
    }
    // 上面都落空时再看两种约定：打包器配置后才成立的省略 `.vue`，以及 TypeScript
    // 找不到实现文件时改用的声明文件。两者在各自工具里都排在脚本扩展名之后。
    let conventions: &[&str] = if typescript {
        &["vue", "d.ts"]
    } else {
        &["vue"]
    };
    for ext in conventions {
        if !directory {
            later.push(format!("{base}.{ext}"));
        }
        later.push(format!("{index}.{ext}"));
    }
    false
}

/// 一个字面量模块说明符指向哪些文件。Vue 脚本块上的 `src` 属性走同一套规则。
pub(super) fn spec_ref(spec: &str, load: Load, at: usize, site: Site<'_>, out: &mut Parsed) {
    let unresolved = |label: &str, out: &mut Parsed| {
        out.reference_at(label, "unsupported", at, Vec::new(), false)
    };
    // 即使相对路径写全了文件名，动态加载也不一定执行。
    let (kind, unconditional) = match load {
        Load::Import => ("import", true),
        Load::Reexport => ("reexport", true),
        Load::DynamicImport => ("import", false),
        Load::Require => ("require", false),
    };
    // 说明符不是模块式拼写时不回显，只说它是哪一种加载。
    let opaque = if matches!(load, Load::Require) {
        "require"
    } else {
        "import"
    };
    if let Some((scheme, rest)) = spec.split_once(':')
        && matches!(scheme, "node" | "npm" | "https" | "http" | "data")
    {
        let label = if scheme == "node" && paths::literal(rest) {
            spec
        } else {
            scheme
        };
        out.reference_at(label, "external", at, Vec::new(), false);
        return;
    }
    // 书写的目标：相对路径只有一个；别名的一个模式可以写几个替换目标。
    let relative = spec.starts_with("./") || spec.starts_with("../") || matches!(spec, "." | "..");
    let (label, bases) = if relative {
        let Some(base) = paths::resolve(site.dir, spec) else {
            unresolved(opaque, out);
            return;
        };
        (spec, vec![(base, paths::directory_spec(spec))])
    } else {
        let bases = site.aliases.targets(spec);
        let bare = spec.strip_prefix('#').unwrap_or(spec);
        if bases.is_empty() {
            // 裸名既可能是包，也可能是工作区或打包器的别名。仅保留模块式拼写。
            let label = if paths::literal(bare)
                && !bare.starts_with('.')
                && bare.split('/').all(|s| !matches!(s, "" | "." | ".."))
            {
                spec
            } else {
                opaque
            };
            unresolved(label, out);
            return;
        }
        (if paths::literal(bare) { spec } else { opaque }, bases)
    };
    let (mut candidates, mut substitutes, mut later) = (Vec::new(), Vec::new(), Vec::new());
    let mut explicit_file = true;
    for (base, directory) in &bases {
        explicit_file &= files(
            base,
            *directory,
            site.typescript,
            &mut candidates,
            &mut substitutes,
            &mut later,
        );
    }
    out.reference_at(
        label,
        kind,
        at,
        candidates,
        unconditional && relative && explicit_file,
    );
    let added = out.refs.last_mut().unwrap();
    added.substitutes = substitutes;
    added.fallbacks = later;
}

fn ident(n: Node<'_>) -> bool {
    matches!(
        n.kind(),
        "identifier" | "type_identifier" | "property_identifier"
    )
}

fn exported(n: Node<'_>, src: &str, out: &mut Parsed) {
    let mut c = n.walk();
    if let Some(default) = n.children(&mut c).find(|s| s.kind() == "default") {
        out.symbol("default", "export", default);
        return;
    }
    if let Some(mut decl) = n.child_by_field_name("declaration") {
        if decl.kind() == "ambient_declaration" {
            let mut c = decl.walk();
            let Some(inner) = decl.named_children(&mut c).find(|s| !s.is_extra()) else {
                return;
            };
            decl = inner;
        }
        if matches!(decl.kind(), "lexical_declaration" | "variable_declaration") {
            let kind = if decl
                .child_by_field_name("kind")
                .is_some_and(|k| k.kind() == "const")
            {
                "constant"
            } else {
                "variable"
            };
            for d in decl
                .named_children(&mut c)
                .filter(|v| v.kind() == "variable_declarator")
            {
                if let Some(name) = d.child_by_field_name("name").filter(|s| ident(*s)) {
                    out.symbol(text(name, src), kind, name);
                }
            }
            return;
        }
        let kind = match decl.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                "function"
            }
            "class_declaration" | "abstract_class_declaration" => "class",
            "type_alias_declaration" => "type",
            "interface_declaration" => "interface",
            "enum_declaration" => "enum",
            "internal_module" => "namespace",
            _ => return,
        };
        if let Some(name) = decl.child_by_field_name("name").filter(|s| ident(*s)) {
            out.symbol(text(name, src), kind, name);
        }
    } else if let Some(clause) = child(n, "export_clause") {
        for spec in clause
            .named_children(&mut c)
            .filter(|s| s.kind() == "export_specifier")
        {
            if let Some(name) = spec
                .child_by_field_name("alias")
                .or_else(|| spec.child_by_field_name("name"))
                .filter(|s| ident(*s))
            {
                out.symbol(text(name, src), "export", name);
            }
        }
    } else if let Some(ns) = child(n, "namespace_export")
        && let Some(name) = ns.named_children(&mut c).find(|s| ident(*s))
    {
        out.symbol(text(name, src), "namespace", name);
    }
}
