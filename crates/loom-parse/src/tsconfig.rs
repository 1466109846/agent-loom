//! TypeScript 和 JavaScript 项目在 `tsconfig.json`、`jsconfig.json` 的 `paths` 里给导入起别名，
//! 如 `@/x` 指向 `src/x`。这里只读这一项，以及定位它要用到的 `baseUrl`、`extends` 和
//! `references`。配置的文本由调用方读出后传入，这里不碰文件系统。
//!
//! 打包器真正使用的别名写在它自己的配置里，那是要执行才知道结果的脚本。两边通常一致，
//! 但没有保证，所以经别名找到的目标一律只算“可能”。

use std::collections::{HashMap, HashSet};

use jsonc_parser::{JsonValue, ParseOptions, parse_to_value};

use crate::paths;

/// 继承和引用最多跟这么多份配置，免得成环或过长的链拖住索引。
const MAX_CHAIN: usize = 32;

/// 写在路径开头时指“用这份配置的那个目录”，供被继承的公共配置使用（TypeScript 5.5 起）。
const CONFIG_DIR: &str = "${configDir}";

/// 按文件名认配置：`tsconfig.json`、`jsconfig.json`，以及 `tsconfig.app.json` 这类变体。
pub fn is_config(name: &str) -> bool {
    (name.starts_with("tsconfig") || name.starts_with("jsconfig")) && name.ends_with(".json")
}

/// 一份配置里与别名有关的内容；继承和引用已换成相对项目根的路径。
struct Config {
    /// 继承的配置，后面的盖过前面的。指向包里预设的不在其中。
    extends: Vec<String>,
    references: Vec<String>,
    /// `baseUrl` 照写的样子：开头的 `${configDir}` 要等知道用的是哪份配置才能换掉。
    base_url: Option<String>,
    /// 各个模式和它的替换目标，按书写的顺序：几个模式一样好时取写在前面的。
    paths: Option<Vec<(String, Vec<String>)>>,
}

/// 一组生效的别名：模式，和解释替换目标时用的两个目录。
#[derive(PartialEq)]
struct Table {
    /// 替换目标的基准：`baseUrl`，没写时是写 `paths` 的那份配置所在的目录。
    base: String,
    /// 用这组别名的那份配置所在的目录，`${configDir}` 指的就是它。
    leaf: String,
    patterns: Vec<(String, Vec<String>)>,
}

/// 配置里写的一个路径落在项目里的哪里：开头是 `${configDir}` 的相对于 `leaf`，其余的相对于
/// `base`。绝对路径、越出项目根的和含有不认识的字符的都没有着落。
fn place(base: &str, leaf: &str, written: &str) -> Option<String> {
    let Some(rest) = written.strip_prefix(CONFIG_DIR) else {
        return paths::resolve(base, written);
    };
    match rest.trim_start_matches('/') {
        "" => Some(leaf.to_owned()),
        rest => paths::resolve(leaf, rest),
    }
}

type Loaded = HashMap<String, Option<Config>>;

fn strings(value: &JsonValue<'_>) -> Vec<String> {
    let JsonValue::Array(list) = value else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| match item {
            JsonValue::String(s) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// `extends` 写的相对路径指向哪份配置，省略 `.json` 时补上。
/// 不以 `./`、`../` 开头的是包里的预设，这里读不到。
fn inherited(dir: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let path = paths::resolve(dir, spec)?;
    Some(if path.ends_with(".json") {
        path
    } else {
        format!("{path}.json")
    })
}

/// `references` 里的路径可以指向一份配置，也可以指向放着 `tsconfig.json` 的目录。
fn referenced(dir: &str, spec: &str) -> Option<String> {
    let path = paths::resolve(dir, spec)?;
    Some(if path.ends_with(".json") {
        path
    } else {
        paths::join(&path, "tsconfig.json")
    })
}

/// 配置允许注释和末尾多余的逗号。读不成对象的配置没有可用的内容。
fn parse(path: &str, text: &str) -> Option<Config> {
    let dir = paths::parent(path);
    let text = text.trim_start_matches('\u{feff}');
    let Ok(Some(JsonValue::Object(mut root))) = parse_to_value(text, &ParseOptions::default())
    else {
        return None;
    };
    let extends = match root.get("extends") {
        Some(JsonValue::String(one)) => vec![one.to_string()],
        Some(several) => strings(several),
        None => Vec::new(),
    };
    let extends = extends
        .iter()
        .filter_map(|spec| inherited(dir, spec))
        .collect();
    let references = root
        .get_array("references")
        .map(|list| {
            list.iter()
                .filter_map(|item| match item {
                    JsonValue::Object(reference) => reference.get_string("path"),
                    _ => None,
                })
                .filter_map(|spec| referenced(dir, spec))
                .collect()
        })
        .unwrap_or_default();
    let mut options = root.take_object("compilerOptions");
    let base_url = options
        .as_ref()
        .and_then(|o| o.get_string("baseUrl"))
        .map(|spec| spec.to_string());
    let paths = options
        .as_mut()
        .and_then(|o| o.take_object("paths"))
        .map(|table| {
            table
                .into_iter()
                .map(|(pattern, targets)| (pattern.into_owned(), strings(&targets)))
                .collect()
        });
    Some(Config {
        extends,
        references,
        base_url,
        paths,
    })
}

/// 沿继承链取第一处写了的值：先看自己，再看 `extends` 里靠后的，逐层向上。
fn first<T>(
    all: &Loaded,
    path: &str,
    seen: &mut HashSet<String>,
    pick: &dyn Fn(&str, &Config) -> Option<T>,
) -> Option<T> {
    if seen.len() >= MAX_CHAIN || !seen.insert(path.to_owned()) {
        return None;
    }
    let config = all.get(path)?.as_ref()?;
    pick(path, config).or_else(|| {
        config
            .extends
            .iter()
            .rev()
            .find_map(|parent| first(all, parent, seen, pick))
    })
}

/// 这份配置自己写的或继承来的别名；整条链上都没写 `paths` 时为 None。
/// 子配置写了 `paths` 就整体取代父配置的，不逐项合并。
fn own(all: &Loaded, path: &str) -> Option<Table> {
    let leaf = paths::parent(path).to_owned();
    let (home, patterns) = first(all, path, &mut HashSet::new(), &|at, config| {
        let list = config.paths.clone()?;
        Some((paths::parent(at).to_owned(), list))
    })?;
    // 写了 `baseUrl` 就以它为基准，它相对于写它的那份配置；否则以写 `paths` 的那份配置
    // 所在的目录为基准。TypeScript 7 不再支持 `baseUrl`，写了的项目仍按 6 及以前的规则读。
    let written = first(all, path, &mut HashSet::new(), &|at, config| {
        let spec = config.base_url.clone()?;
        Some((paths::parent(at).to_owned(), spec))
    });
    let base = match written {
        None => home,
        // 空的 `baseUrl` 也算写了，指写它的那份配置所在的目录。
        Some((dir, spec)) if spec.is_empty() => dir,
        Some((dir, spec)) => match place(&dir, &leaf, &spec) {
            Some(base) => base,
            // 基准落在项目之外：别名指不到项目里的文件。
            None => {
                return Some(Table {
                    base: String::new(),
                    leaf,
                    patterns: Vec::new(),
                });
            }
        },
    };
    Some(Table {
        base,
        leaf,
        patterns,
    })
}

/// 归这份配置管的文件用到的别名。它自己没有时看它引用的各份配置：只起汇总作用的
/// `tsconfig.json` 把别名写在被引用的配置里。哪一份真正管着某个文件要看 `include`，
/// 这里不求值，几份都算上；它们说法不一时，候选就不止一个。
fn tables(all: &Loaded, path: &str, seen: &mut HashSet<String>, found: &mut Vec<Table>) {
    if seen.len() >= MAX_CHAIN || !seen.insert(path.to_owned()) {
        return;
    }
    if let Some(table) = own(all, path) {
        if !found.contains(&table) {
            found.push(table);
        }
    } else if let Some(Some(config)) = all.get(path) {
        for next in &config.references {
            tables(all, next, seen, found);
        }
    }
}

/// 写法对上的模式的替换目标，以及 `*` 代表的那一段（模式里没有 `*` 时为 None）。
/// 规则与 TypeScript 相同：没有 `*` 的模式要整个相等，对上了就用它；其余的取前缀最长的，
/// 一样长时取写在前面的。写了不止一个 `*` 的模式不合法，不用。
fn matched<'t, 's>(
    patterns: &'t [(String, Vec<String>)],
    spec: &'s str,
) -> Option<(&'t [String], Option<&'s str>)> {
    if let Some((_, targets)) = patterns
        .iter()
        .find(|(pattern, _)| !pattern.contains('*') && pattern == spec)
    {
        return Some((targets, None));
    }
    let mut best: Option<(usize, &'t [String], &'s str)> = None;
    for (pattern, targets) in patterns {
        let Some((prefix, suffix)) = pattern.split_once('*') else {
            continue;
        };
        let fits = !suffix.contains('*')
            && spec.len() >= prefix.len() + suffix.len()
            && spec.starts_with(prefix)
            && spec.ends_with(suffix);
        if fits && best.is_none_or(|(longest, ..)| prefix.len() > longest) {
            let part = &spec[prefix.len()..spec.len() - suffix.len()];
            best = Some((prefix.len(), targets, part));
        }
    }
    best.map(|(_, targets, part)| (targets, Some(part)))
}

/// 项目里各份配置给出的别名，按配置所在的目录存放。
#[derive(Default)]
pub(crate) struct Configs(HashMap<String, Vec<Table>>);

impl Configs {
    /// 每份配置相对项目根的路径和它的文本；读不出文本的也要给出，它照样管着所在的目录。
    pub(crate) fn new(list: impl IntoIterator<Item = (String, Option<String>)>) -> Self {
        let all: Loaded = list
            .into_iter()
            .map(|(path, text)| {
                let path = path.replace('\\', "/");
                let config = text.as_deref().and_then(|text| parse(&path, text));
                (path, config)
            })
            .collect();
        let mut by_dir = HashMap::new();
        // 同一个目录里两份都有时，编辑器先认 tsconfig.json。
        for name in ["tsconfig.json", "jsconfig.json"] {
            for path in all.keys().filter(|p| p.rsplit('/').next() == Some(name)) {
                by_dir
                    .entry(paths::parent(path).to_owned())
                    .or_insert_with(|| {
                        let mut found = Vec::new();
                        tables(&all, path, &mut HashSet::new(), &mut found);
                        found
                    });
            }
        }
        Self(by_dir)
    }

    /// 一个源码文件适用的别名：离它最近的那份 `tsconfig.json` 或 `jsconfig.json` 说了算。
    /// 那份配置读不出内容或没有别名时，不再向上找。
    pub(crate) fn aliases(&self, path: &str) -> Aliases<'_> {
        if self.0.is_empty() {
            return Aliases(&[]);
        }
        let mut dir = paths::parent(path);
        loop {
            if let Some(found) = self.0.get(dir) {
                return Aliases(found);
            }
            if dir.is_empty() {
                return Aliases(&[]);
            }
            dir = paths::parent(dir);
        }
    }
}

/// 某个源码文件适用的别名。
#[derive(Clone, Copy)]
pub(crate) struct Aliases<'c>(&'c [Table]);

impl Aliases<'_> {
    /// 写法经别名指向的项目内路径，以及替换目标是不是写成了目录。没有别名对得上时为空。
    /// 别名只用于裸名：以 `/` 开头的写法和相对路径一样，不经别名。
    pub(crate) fn targets(&self, spec: &str) -> Vec<(String, bool)> {
        let mut found = Vec::new();
        if spec.starts_with('/') {
            return found;
        }
        for table in self.0 {
            let Some((targets, part)) = matched(&table.patterns, spec) else {
                continue;
            };
            for target in targets {
                // `*` 代表的一段为空时，TypeScript 照写的用，留着 `*` 的路径对不上文件。
                let written = match part {
                    Some(part) if !part.is_empty() => target.replacen('*', part, 1),
                    _ => target.clone(),
                };
                // 绝对路径、越出项目根和还带着 `*` 的目标都不要。
                if let Some(path) = place(&table.base, &table.leaf, &written) {
                    let item = (path, paths::directory_spec(&written));
                    if !found.contains(&item) {
                        found.push(item);
                    }
                }
            }
        }
        found
    }
}
