//! 单文件组件只取根级 `<script>` 块，按块上声明的语言交给脚本提取器。
//! 模板里用到的组件都经脚本导入；模板和样式本身不产出事实。

use anyhow::Result;

use crate::{Extractor, Grammar, Parsed, ecma};

struct Tag<'s> {
    name: &'s str,
    attrs: Vec<(&'s str, Option<&'s str>)>,
    /// 开标签之后的第一个字节。
    end: usize,
    /// 写成 `<x />`，没有内容。
    closed: bool,
}

impl<'s> Tag<'s> {
    fn attr(&self, key: &str) -> Option<Option<&'s str>> {
        self.attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
}

/// 属性值里可以有 `>`，所以要逐个读属性。只在 ASCII 定界符处切分，不会落在字符中间。
/// 标签没有写完时返回 None。
fn tag(src: &str, start: usize) -> Option<Tag<'_>> {
    let b = src.as_bytes();
    let space = |i: usize| b.get(i).is_some_and(u8::is_ascii_whitespace);
    let mut i = start + 1;
    while b
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(*c, b'-' | b'_' | b':' | b'.'))
    {
        i += 1;
    }
    let name = &src[start + 1..i];
    let mut attrs = Vec::new();
    loop {
        while space(i) {
            i += 1;
        }
        match *b.get(i)? {
            b'>' => {
                return Some(Tag {
                    name,
                    attrs,
                    end: i + 1,
                    closed: false,
                });
            }
            b'/' if b.get(i + 1) == Some(&b'>') => {
                return Some(Tag {
                    name,
                    attrs,
                    end: i + 2,
                    closed: true,
                });
            }
            b'/' | b'=' => {
                i += 1;
                continue;
            }
            _ => (),
        }
        let key = i;
        while b
            .get(i)
            .is_some_and(|c| !c.is_ascii_whitespace() && !matches!(*c, b'=' | b'>' | b'/'))
        {
            i += 1;
        }
        let key = &src[key..i];
        while space(i) {
            i += 1;
        }
        let mut value = None;
        if b.get(i) == Some(&b'=') {
            i += 1;
            while space(i) {
                i += 1;
            }
            match *b.get(i)? {
                quote @ (b'"' | b'\'') => {
                    let close = i + 1 + src[i + 1..].find(quote as char)?;
                    value = Some(&src[i + 1..close]);
                    i = close + 1;
                }
                _ => {
                    let from = i;
                    while b
                        .get(i)
                        .is_some_and(|c| !c.is_ascii_whitespace() && *c != b'>')
                    {
                        i += 1;
                    }
                    value = Some(&src[from..i]);
                }
            }
        }
        attrs.push((key, value));
    }
}

fn boundary(rest: &str, extra: &[char]) -> bool {
    rest.starts_with(|c: char| c == '>' || c.is_ascii_whitespace() || extra.contains(&c))
}

/// 模板之外的块不按标记解析，内容到同名结束标签为止。返回内容的末尾和结束标签之后的位置。
fn raw_end(src: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let mut at = from;
    loop {
        let found = at + src[at..].find("</")?;
        let rest = &src[found + 2..];
        if let Some(after) = rest.strip_prefix(name)
            && boundary(after, &[])
        {
            return Some((found, found + 2 + rest.find('>')? + 1));
        }
        at = found + 2;
    }
}

/// 模板按元素嵌套配对，其中出现的 `<script>` 不是根级块。返回结束标签之后的位置。
fn template_end(src: &str, mut i: usize) -> Option<usize> {
    let mut depth = 1usize;
    while depth > 0 {
        let lt = i + src[i..].find('<')?;
        let rest = &src[lt..];
        if rest.starts_with("<!--") {
            i = lt + rest.find("-->")? + 3;
        } else if let Some(after) = rest.strip_prefix("</template")
            && boundary(after, &[])
        {
            depth -= 1;
            i = lt + rest.find('>')? + 1;
        } else if let Some(after) = rest.strip_prefix("<template")
            && boundary(after, &['/'])
        {
            let open = tag(src, lt)?;
            if !open.closed {
                depth += 1;
            }
            i = open.end;
        } else {
            i = lt + 1;
        }
    }
    Some(i)
}

fn rows(src: &str, end: usize) -> usize {
    src.as_bytes()[..end]
        .iter()
        .filter(|c| **c == b'\n')
        .count()
}

pub(super) fn extract(ex: &mut Extractor, src: &str, path: &str, out: &mut Parsed) -> Result<()> {
    // 块没有写完整时，Vue 自己也无法编译这份文件。
    let Some(scripts) = scripts(src) else {
        out.syntax_error = true;
        return Ok(());
    };
    for (open, start, end) in scripts {
        let line = rows(src, start) + 1;
        if let Some(spec) = open.attr("src") {
            // 写了 src 的块由外部文件提供内容，按脚本里的导入同样的规则去找。
            // 这里的路径照写的用，不套 TypeScript 以 `.js` 指向 `.ts` 的约定。
            match spec {
                Some(spec) => {
                    ecma::spec_ref(spec, ecma::Load::Import, line, ex.site(path, false), out)
                }
                None => out.reference_at("script", "unsupported", line, Vec::new(), false),
            }
            continue;
        }
        let grammar = match open.attr("lang").flatten() {
            None | Some("js" | "javascript" | "jsx") => Grammar::Js,
            Some("ts" | "typescript") => Grammar::Ts,
            Some("tsx") => Grammar::Tsx,
            Some(_) => {
                out.reference_at("script", "unsupported", line, Vec::new(), false);
                continue;
            }
        };
        let body = &src[open.end..end];
        let tree = ex.tree(grammar, body)?;
        if tree.root_node().has_error() {
            out.syntax_error = true;
            return Ok(());
        }
        let (refs, symbols) = (out.refs.len(), out.symbols.len());
        let typescript = !matches!(grammar, Grammar::Js);
        ecma::extract(tree.root_node(), body, ex.site(path, typescript), out);
        // 语法树只见到块内的文本，行号要换算回整份文件。
        let offset = rows(src, open.end);
        for r in &mut out.refs[refs..] {
            r.line += offset;
        }
        for s in &mut out.symbols[symbols..] {
            s.line += offset;
        }
    }
    Ok(())
}

/// 根级 `<script>` 块：开标签、开标签的位置、内容的末尾。结构不完整时返回 None。
fn scripts(src: &str) -> Option<Vec<(Tag<'_>, usize, usize)>> {
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(lt) = src[i..].find('<').map(|p| p + i) {
        let rest = &src[lt..];
        if rest.starts_with("<!--") {
            i = lt + rest.find("-->")? + 3;
            continue;
        }
        if !rest[1..].starts_with(|c: char| c.is_ascii_alphabetic()) {
            i = lt + 1;
            continue;
        }
        let open = tag(src, lt)?;
        let markup = open.name == "template"
            && open
                .attr("lang")
                .is_none_or(|lang| lang.is_none_or(|l| l == "html"));
        let (end, next) = if open.closed {
            (open.end, open.end)
        } else if markup {
            let next = template_end(src, open.end)?;
            (next, next)
        } else {
            raw_end(src, open.end, open.name)?
        };
        if open.name == "script" {
            found.push((open, lt, end));
        }
        i = next;
    }
    Some(found)
}
