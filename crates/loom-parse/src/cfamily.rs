//! C 和 C++ 只取包含指令。包含由预处理器在语法分析之前处理，宏又常让语法树不完整，
//! 所以这里按预处理的词法规则扫描，不建语法树；不展开宏，不求值条件。

use crate::{Parsed, paths};

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// 反斜杠后紧跟换行是续行；返回要跳过的字节数。
fn splice(b: &[u8], i: usize) -> Option<usize> {
    match (b.get(i), b.get(i + 1), b.get(i + 2)) {
        (Some(b'\\'), Some(b'\n'), _) => Some(2),
        (Some(b'\\'), Some(b'\r'), Some(b'\n')) => Some(3),
        _ => None,
    }
}

/// 字符串或字符字面量的结束引号；字面量不跨行，本行找不到时这个引号不算开头。
fn literal_end(b: &[u8], start: usize) -> Option<usize> {
    let quote = b[start];
    let mut i = start + 1;
    while let Some(&c) = b.get(i) {
        if let Some(skip) = splice(b, i) {
            i += skip;
        } else if c == b'\\' {
            i += 2;
        } else if c == b'\n' {
            return None;
        } else if c == quote {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// C++ 原始字符串 `R"界(` … `)界"` 结束引号之后的位置；内容可以跨行，也可以含引号和 `#include`。
fn raw_string_end(src: &str, quote: usize, before: &str) -> Option<usize> {
    let word = before.len() - before.bytes().rev().take_while(|c| ident(*c)).count();
    if !matches!(&before[word..], "R" | "LR" | "uR" | "UR" | "u8R") {
        return None;
    }
    let rest = &src[quote + 1..];
    let open = rest.find('(').filter(|n| *n <= 16)?;
    if rest[..open].contains(['"', ')', '\\', ' ', '\n']) {
        return None;
    }
    let close = format!("){}\"", &rest[..open]);
    Some(quote + 1 + open + 1 + rest[open + 1..].find(&close)? + close.len())
}

/// 去掉注释并接好续行后的逻辑行，带行号：行里第一个不是空白也不是注释的字符所在的那一行。
/// 块注释没有结束时返回 None。
fn logical_lines(src: &str, raw_strings: bool) -> Option<Vec<(usize, String)>> {
    let b = src.as_bytes();
    let newlines = |from: usize, to: usize| b[from..to].iter().filter(|c| **c == b'\n').count();
    let mut lines = Vec::new();
    let (mut text, mut start, mut line, mut i) = (String::new(), 1, 1, 0);
    // 这一行到目前为止只有空白和注释。
    let mut blank = true;
    while let Some(&c) = b.get(i) {
        if let Some(skip) = splice(b, i) {
            line += 1;
            i += skip;
            continue;
        }
        match c {
            b'\n' => {
                lines.push((start, std::mem::take(&mut text)));
                line += 1;
                blank = true;
                i += 1;
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while b.get(i).is_some_and(|c| *c != b'\n') {
                    match splice(b, i) {
                        Some(skip) => {
                            line += 1;
                            i += skip;
                        }
                        None => i += 1,
                    }
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = i + 2 + src[i + 2..].find("*/")?;
                line += newlines(i, end);
                text.push(' ');
                i = end + 2;
                continue;
            }
            _ => (),
        }
        if blank && !c.is_ascii_whitespace() {
            blank = false;
            start = line;
        }
        match c {
            b'"' | b'\'' => {
                let raw = (raw_strings && c == b'"')
                    .then(|| raw_string_end(src, i, &text))
                    .flatten();
                if let Some(end) = raw {
                    line += newlines(i, end);
                    text.push_str("\"\"");
                    i = end;
                } else if let Some(end) = literal_end(b, i) {
                    line += newlines(i, end);
                    text.push_str(&src[i..=end]);
                    i = end + 1;
                } else {
                    text.push(c as char);
                    i += 1;
                }
            }
            // 数字里的撇号是分隔符（`1'000'000`），不是字符字面量的开头。
            b'0'..=b'9' if !text.bytes().last().is_some_and(ident) => {
                let from = i;
                while b.get(i).is_some_and(|c| {
                    ident(*c)
                        || *c == b'.'
                        || (*c == b'\'' && b.get(i + 1).is_some_and(|n| ident(*n)))
                }) {
                    i += 1;
                }
                text.push_str(&src[from..i]);
            }
            _ => {
                let len = src[i..].chars().next().map_or(1, char::len_utf8);
                text.push_str(&src[i..i + len]);
                i += len;
            }
        }
    }
    lines.push((start, text));
    Some(lines)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Open,
    Alternative,
    Close,
    Include,
    IncludeNext,
    Define,
    Once,
    Other,
}

struct Directive<'s> {
    kind: Kind,
    line: usize,
    rest: &'s str,
    /// `#ifndef X` 或 `#if !defined(X)` 检查的宏名。
    guards: Option<&'s str>,
}

fn first_word(s: &str) -> &str {
    let s = s.trim_start();
    &s[..s.bytes().take_while(|c| ident(*c)).count()]
}

fn guarded<'s>(name: &str, rest: &'s str) -> Option<&'s str> {
    let word = match name {
        "ifndef" => first_word(rest),
        "if" => {
            let inner = rest.trim().strip_prefix('!')?.trim_start();
            let inner = inner.strip_prefix("defined")?.trim_start();
            let (inner, close) = match inner.strip_prefix('(') {
                Some(inner) => (inner, ")"),
                None => (inner, ""),
            };
            let word = first_word(inner);
            if inner.trim_start()[word.len()..].trim() != close {
                return None;
            }
            word
        }
        _ => return None,
    };
    (!word.is_empty()).then_some(word)
}

fn directives(lines: &[(usize, String)]) -> (Vec<Directive<'_>>, Option<(usize, usize)>) {
    let mut found = Vec::new();
    // 指令之外第一行和最后一行代码的位置，用来判断头文件保护之外有没有内容。
    let mut code: Option<(usize, usize)> = None;
    for (line, text) in lines {
        let text = text.trim();
        let Some(rest) = text.strip_prefix('#') else {
            if !text.is_empty() {
                code = Some((code.map_or(*line, |c| c.0), *line));
            }
            continue;
        };
        let name = first_word(rest);
        let rest = &rest.trim_start()[name.len()..];
        let kind = match name {
            "if" | "ifdef" | "ifndef" => Kind::Open,
            "elif" | "elifdef" | "elifndef" | "else" => Kind::Alternative,
            "endif" => Kind::Close,
            "include" | "import" => Kind::Include,
            "include_next" => Kind::IncludeNext,
            "define" => Kind::Define,
            "pragma" if rest.trim() == "once" => Kind::Once,
            _ => Kind::Other,
        };
        found.push(Directive {
            kind,
            line: *line,
            rest,
            guards: guarded(name, rest),
        });
    }
    (found, code)
}

pub(super) fn extract(src: &str, path: &str, out: &mut Parsed) {
    // 原始字符串只有 C++ 有；`.h` 可能属于任一种语言，按 C++ 处理。
    let Some(lines) = logical_lines(src, !path.ends_with(".c")) else {
        out.syntax_error = true;
        return;
    };
    let (directives, code) = directives(&lines);
    // 头文件保护：`#pragma once` 之外的第一条指令检查某个宏未定义，紧接着定义它，配对的
    // #endif 是最后一条指令，中间没有 #else，保护之外没有代码。每次首次包含都会走到
    // 保护内的第一层，所以那一层不算条件。
    let first = directives
        .iter()
        .position(|d| d.kind != Kind::Once)
        .unwrap_or(directives.len());
    let mut whole = directives.len() >= first + 3
        && directives[first].guards.is_some_and(|g| {
            directives[first + 1].kind == Kind::Define
                && first_word(directives[first + 1].rest) == g
        })
        && code.is_none_or(|(head, tail)| {
            head > directives[first].line && tail < directives[directives.len() - 1].line
        });
    let mut depth = 0usize;
    for (i, d) in directives.iter().enumerate() {
        match d.kind {
            Kind::Open => {
                whole &= depth > 0 || i == first;
                depth += 1;
            }
            Kind::Alternative | Kind::Close if depth == 0 => {
                out.syntax_error = true;
                return;
            }
            Kind::Alternative => whole &= depth > 1,
            Kind::Close => {
                depth -= 1;
                whole &= depth > 0 || i == directives.len() - 1;
            }
            _ => (),
        }
    }
    if depth != 0 {
        out.syntax_error = true;
        return;
    }
    let dir = paths::parent(path);
    let floor = usize::from(whole);
    for d in &directives {
        match d.kind {
            Kind::Open => depth += 1,
            Kind::Close => depth -= 1,
            Kind::Include => include(d, depth > floor, dir, out),
            // 从当前文件所在位置之后的包含路径接着找，目标取决于编译参数。
            Kind::IncludeNext => {
                out.reference_at("include_next", "unsupported", d.line, Vec::new(), false)
            }
            _ => (),
        }
    }
}

fn include(d: &Directive<'_>, conditional: bool, dir: &str, out: &mut Parsed) {
    let rest = d.rest.trim();
    let (quoted, spec) = if let Some(r) = rest.strip_prefix('"') {
        (true, r.split_once('"'))
    } else if let Some(r) = rest.strip_prefix('<') {
        (false, r.split_once('>'))
    } else {
        // 由宏给出文件名。
        (false, None)
    };
    let spec = spec.map(|(name, _)| name.replace('\\', "/"));
    let Some(spec) =
        spec.filter(|s| paths::literal(s) && !s.starts_with('/') && !paths::directory_spec(s))
    else {
        out.reference_at("include", "unsupported", d.line, Vec::new(), false);
        return;
    };
    // 引号形式先在使用方所在目录找，主流编译器都是如此；这一处能直接确定。
    let here = quoted.then(|| paths::resolve(dir, &spec)).flatten();
    // 其余取决于编译时的包含路径，这里读不到：项目里路径以这一段结尾的头文件都是候选。
    // 尖括号里只有文件名时不猜：与系统头同名的本地文件（如兼容层的 string.h）太容易连错。
    let dotted = spec.split('/').any(|s| matches!(s, "." | ".."));
    let elsewhere = (!dotted && (quoted || spec.contains('/')))
        .then(|| paths::resolve("", &spec))
        .flatten();
    out.reference_at(
        &spec,
        "include",
        d.line,
        here.into_iter().collect(),
        quoted && !conditional,
    );
    out.refs.last_mut().unwrap().fallbacks =
        elsewhere.map(|p| format!("inc:{p}")).into_iter().collect();
}

/// 包含指令按包含路径查找，头文件路径的任何后缀都可能是写在指令里的那一段。
pub(super) fn path_keys(path: &str) -> Vec<String> {
    if !matches!(path.rsplit_once('.'), Some((_, "h" | "hh" | "hpp" | "hxx"))) {
        return Vec::new();
    }
    std::iter::successors(Some(path), |p| p.split_once('/').map(|(_, rest)| rest))
        .map(|suffix| format!("inc:{suffix}"))
        .collect()
}
