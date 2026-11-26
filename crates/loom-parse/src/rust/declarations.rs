//! 只从当前语法树取证明或反证。普通公开符号表含重导出，不能用作最终提供方表。
use std::collections::HashMap;

use tree_sitter::Node;

use super::{
    Attr, Item, Leaf, Scope, attribute, child, declaration_kind, ident, paths, public, text, visit,
};
use crate::{Parsed, RustItem, RustModule};

fn crate_visible(n: Node<'_>) -> bool {
    public(n)
        || child(n, "visibility_modifier").is_some_and(|v| {
            v.named_child_count() == 1 && v.named_child(0).is_some_and(|c| c.kind() == "crate")
        })
}

/// 条件/组合属性不能用于证明。保留能指向别的受管文件的字面量，只用于否决唯一所有者。
fn possible_files(
    n: Node<'_>,
    scope: &Scope,
    attr: Attr<'_>,
    label: &str,
    src: &str,
) -> Vec<String> {
    if !matches!(attr, Attr::Opaque) {
        return scope.files(attr, label).unwrap_or_default();
    }
    let mut files = scope.files(Attr::Plain, label).unwrap_or_default();
    let mut prev = n.prev_named_sibling();
    while let Some(a) = prev {
        match a.kind() {
            "attribute_item" => {
                if let Attr::Path(p) = attribute(a, src) {
                    files.extend(scope.files(Attr::Path(p), label).into_iter().flatten());
                } else if child(a, "attribute")
                    .and_then(|attr| attr.named_child(0))
                    .is_some_and(|k| text(k, src) == "cfg_attr")
                {
                    visit(a, |v| {
                        if matches!(v.kind(), "string_literal" | "raw_string_literal")
                            && v.named_child_count() == 1
                            && let Some(s) = child(v, "string_content")
                            && let Some(p) = paths::resolve(&scope.attr_dir, text(s, src))
                        {
                            files.push(p);
                        }
                    });
                }
            }
            "line_comment" | "block_comment" => (),
            _ => break,
        }
        prev = a.prev_named_sibling();
    }
    files.sort();
    files.dedup();
    files
}

/// 重名统计包含不可见、带属性和导入的绑定；否则只数“看起来可用”的声明会制造唯一性。
pub(super) fn collect(
    items: &[Item<'_, '_>],
    scope: &Scope,
    top: bool,
    src: &str,
    out: &mut Parsed,
) -> bool {
    let mut counts = HashMap::<&str, usize>::new();
    let mut usable = top && scope.enabled;
    for item in items {
        let n = item.node;
        if n.kind() == "use_declaration" {
            usable &= !item.leaves.is_empty() && !item.leaves.iter().any(|l| l.glob);
            for binding in item.leaves.iter().filter_map(Leaf::binding) {
                *counts.entry(ident(binding, src)).or_default() += 1;
            }
        } else if matches!(
            n.kind(),
            "macro_invocation" | "foreign_mod_item" | "expression_statement"
        ) {
            // 模块级宏或 extern 块可能引入未提取的同名项。
            usable = false;
        } else if let Some(name) = n
            .child_by_field_name("alias")
            .or_else(|| n.child_by_field_name("name"))
        {
            *counts.entry(ident(name, src)).or_default() += 1;
        }
    }
    if usable {
        out.rust_scope = Some(scope.crate_root);
    }
    for item in items {
        let n = item.node;
        let Some(name) = n.child_by_field_name("name") else {
            continue;
        };
        let label = ident(name, src);
        let direct = usable && matches!(item.attr, Attr::Plain) && counts.get(label) == Some(&1);
        if n.kind() == "mod_item" && n.child_by_field_name("body").is_none() {
            let candidates = possible_files(n, scope, item.attr, label, src);
            if !candidates.is_empty() {
                out.rust_modules.push(RustModule {
                    name: label.into(),
                    candidates,
                    usable: direct,
                    crate_visible: crate_visible(n),
                });
            }
        } else if direct && declaration_kind(n.kind()).is_some() && crate_visible(n) {
            out.rust_items.push(RustItem {
                name: label.into(),
                public: public(n),
            });
        }
    }
    usable
}

/// 只接收有模块段的命名导入。裸前缀仅接受根文件自己声明的文件模块。
/// self 作为分组叶时表示模块本身，不是通配符，也不是某个叫 self 的项。
pub(super) fn use_path(
    leaf: &Leaf<'_>,
    scope: &Scope,
    names: &super::Bindings<'_>,
    src: &str,
) -> Option<String> {
    if leaf.alias.is_some() || leaf.glob {
        return None;
    }
    let (head, tail) = leaf.path.split_first()?;
    let (anchor, rest) = match head.kind() {
        "crate" => ("crate", tail),
        "self" => ("self", tail),
        "identifier"
            if scope.crate_root
                && matches!(names.get(ident(*head, src)), Some(super::Binding::File(_))) =>
        {
            ("crate", leaf.path.as_slice())
        }
        _ => return None,
    };
    let (item, modules) = rest.split_last()?;
    if modules.is_empty() || !modules.iter().all(|m| m.kind() == "identifier") {
        return None;
    }
    if item.kind() != "identifier"
        && !(item.kind() == "self" && item.parent().is_some_and(|p| p.kind() == "use_list"))
    {
        return None;
    }
    let mut path = String::from(anchor);
    for segment in rest {
        let s = ident(*segment, src);
        if segment.kind() == "identifier" && matches!(s, "self" | "super" | "crate" | "Self" | "_")
        {
            return None;
        }
        if s.len() > 120 || path.len() + s.len() + 2 > 1024 {
            return None;
        }
        path.push_str("::");
        path.push_str(s);
    }
    Some(path)
}
