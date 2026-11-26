//! 纯词法的项目相对路径。不使用宿主操作系统的路径解析规则。

pub(crate) fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// `base` 已规范化，`tail` 是调用方构造的安全相对路径。
pub(crate) fn join(base: &str, tail: &str) -> String {
    if base.is_empty() {
        tail.into()
    } else {
        format!("{base}/{tail}")
    }
}

pub(crate) fn directory_spec(raw: &str) -> bool {
    matches!(raw.rsplit('/').next(), None | Some("" | "." | ".."))
}

pub(crate) fn source(raw: &str) -> Option<String> {
    let raw = raw.replace('\\', "/");
    if directory_spec(&raw) {
        return None;
    }
    normalize("", &raw)
}

/// 模块字面量宁可漏报也不解释 URL、转义、查询串、控制字符或任意文本。
pub(crate) fn literal(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_alphanumeric() || "/._-@+$~".contains(c))
}

pub(crate) fn resolve(base: &str, spec: &str) -> Option<String> {
    if !literal(spec) {
        return None;
    }
    normalize(base, spec)
}

fn normalize(base: &str, raw: &str) -> Option<String> {
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.contains([':', '\\'])
        || raw.chars().any(char::is_control)
    {
        return None;
    }
    let mut parts: Vec<_> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in raw.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop()?;
            }
            part if part.ends_with([' ', '.']) => return None,
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

pub(crate) fn module_pair(base: &str, name: &str, ext: &str, index: &str) -> Vec<String> {
    let stem = join(base, name);
    vec![format!("{stem}.{ext}"), join(&stem, index)]
}
