//! 从受支持的源码中提取可定位的结构事实。不查询文件系统：要用到的清单和配置
//! （go.mod、tsconfig.json、含 __init__.py 的目录）由调用方传入。

mod cfamily;
mod ecma;
mod go;
mod java;
mod paths;
mod python;
mod rust;
#[cfg(test)]
mod tests;
mod tsconfig;
mod vue;

use std::collections::HashSet;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser, Tree};

pub use go::module_path as go_module;
pub use tsconfig::is_config as ecma_config;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub syntax_error: bool,
    pub symbols: Vec<Declaration>,
    pub refs: Vec<Reference>,
    /// 声明证明独立于对外展示的公开符号（后者也包含重导出）。
    /// 可用于声明链的文件作用域；Some(true) 表示常规 Rust 入口。
    pub rust_scope: Option<bool>,
    pub rust_modules: Vec<RustModule>,
    pub rust_items: Vec<RustItem>,
    /// 别的文件按名字而不是按路径找到本文件时用的声明键，如 Java 文件所在的包。
    /// 键里必有冒号，项目内的路径不含冒号，两者不会相混。
    pub provides: Vec<String>,
    /// 所属根（Java 源根、Go 模块）。使用方和声明方同根时，按声明键找到的唯一目标才算确定。
    pub domain: Option<String>,
    /// 本文件里只是提到的名字（种类为 `mention` 的引用）在候选里没有着落时，再到这些声明键
    /// 里找公开的同名声明：Java 的按需导入、Go 的点导入。每个文件存一份，不随名字的个数膨胀。
    pub search: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RustModule {
    pub name: String,
    pub candidates: Vec<String>,
    /// 不可证明的声明只用于否决唯一归属，不能给任何文件授予归属。
    pub usable: bool,
    pub crate_visible: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RustItem {
    pub name: String,
    pub public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub kind: &'static str,
    pub line: usize,
    /// 不对外公开的顶层声明只供同包文件按名字查找，不作为公开符号展示。
    pub exported: bool,
}

/// 候选有三种写法。路径：项目内的相对路径。整键：与某个文件提供的声明键相等即命中。
/// 成员键（见 [`member`]）：文件既提供该声明键，又在顶层声明了这个名字才命中。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub spelling: String,
    pub kind: &'static str,
    pub line: usize,
    /// Rust 导入必须逐叶证明最终提供方；不能把公共前缀当目标。
    pub rust_paths: Vec<String>,
    pub candidates: Vec<String>,
    /// 语法可确定这组文件候选；不保证文件存在，也不代表候选唯一。
    pub exact: bool,
    /// 只靠工具约定才成立的替代候选（TypeScript 以 `./x.js` 指向 `x.ts`）。
    /// 与 `candidates` 一同参与消歧；即使唯一命中也只算“可能”，不受 `exact` 影响。
    pub substitutes: Vec<String>,
    /// 前两组都落空时才看的候选（C 的包含路径）：语言规定前者优先，
    /// 所以不与前者并列消歧；唯一命中也只算“可能”。
    pub fallbacks: Vec<String>,
}

/// 成员键：`name` 是声明键 `key` 所指的包里的一个顶层名字。名字是标识符，不含冒号。
pub fn member(name: &str, key: &str) -> String {
    format!("sym:{name}:{key}")
}

/// 把成员键拆回名字和声明键；不是成员键时返回 None。
pub fn member_parts(candidate: &str) -> Option<(&str, &str)> {
    candidate.strip_prefix("sym:")?.split_once(':')
}

#[derive(Clone, Copy)]
enum Grammar {
    Rust,
    Js,
    Ts,
    Tsx,
    Python,
    Go,
    Java,
    Vue,
    C,
    Cpp,
}

impl Grammar {
    fn for_path(path: &str) -> Option<Self> {
        let (_, ext) = path.rsplit(['/', '\\']).next()?.rsplit_once('.')?;
        Some(match ext {
            "rs" => Self::Rust,
            "js" | "jsx" | "cjs" | "mjs" => Self::Js,
            "ts" | "cts" | "mts" => Self::Ts,
            "tsx" => Self::Tsx,
            "py" => Self::Python,
            "go" => Self::Go,
            "java" => Self::Java,
            "vue" => Self::Vue,
            // `.h` 两种语言共用；提取只看预处理指令，不必分辨。
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Self::Cpp,
            _ => return None,
        })
    }
}

pub fn language(path: &str) -> Option<&'static str> {
    Some(match Grammar::for_path(path)? {
        Grammar::Rust => "rust",
        Grammar::Js => "javascript",
        Grammar::Ts | Grammar::Tsx => "typescript",
        Grammar::Python => "python",
        Grammar::Go => "go",
        Grammar::Java => "java",
        Grammar::Vue => "vue",
        Grammar::C => "c",
        Grammar::Cpp => "cpp",
    })
}

#[derive(Default)]
pub struct Extractor {
    parser: Parser,
    go: go::Modules,
    ecma: tsconfig::Configs,
    python_packages: Option<HashSet<String>>,
}

impl Extractor {
    /// 项目里每个 go.mod 所在的目录和它声明的模块路径。读不出模块路径的也要给出：
    /// 它仍是模块边界，其下的文件不属于外层模块。内容由调用方读取，这里不碰文件系统。
    pub fn set_go_modules(&mut self, modules: impl IntoIterator<Item = (String, Option<String>)>) {
        self.go = go::Modules::new(modules);
    }

    /// 项目里每份 tsconfig / jsconfig（文件名见 [`ecma_config`]）相对项目根的路径和它的文本，
    /// 用来解释 JS、TS 和 Vue 文件里的别名导入。读不出文本的也要给出：它照样管着所在的目录。
    pub fn set_ecma_configs(
        &mut self,
        configs: impl IntoIterator<Item = (String, Option<String>)>,
    ) {
        self.ecma = tsconfig::Configs::new(configs);
    }

    /// 含受管 `__init__.py` 的目录，相对项目根，以 `/` 分隔，根目录用空串表示。
    /// 未提供清单时不推断包根；空清单则表示所有使用方都在非包目录里。
    pub fn set_python_packages(&mut self, dirs: impl IntoIterator<Item = String>) {
        self.python_packages = Some(dirs.into_iter().collect());
    }

    /// 脚本里的导入语句所在的位置；Vue 的脚本块与所在的文件同一个位置。
    fn site<'a>(&'a self, path: &'a str, typescript: bool) -> ecma::Site<'a> {
        ecma::Site {
            dir: paths::parent(path),
            typescript,
            aliases: self.ecma.aliases(path),
        }
    }

    /// 只由路径决定的声明键，文件没能解析时仍然成立。
    pub fn path_keys(&self, path: &str) -> Vec<String> {
        let path = path.replace('\\', "/");
        match Grammar::for_path(&path) {
            Some(Grammar::C | Grammar::Cpp) => cfamily::path_keys(&path),
            Some(Grammar::Go) => self.go.path_keys(&path),
            _ => Vec::new(),
        }
    }

    pub fn extract(&mut self, path: &str, source: &str) -> Result<Parsed> {
        let path = paths::source(path).context("需要项目根内的相对源码路径")?;
        let grammar = Grammar::for_path(&path).context("不支持的源码扩展名")?;
        let mut parsed = Parsed::default();
        match grammar {
            Grammar::C | Grammar::Cpp => cfamily::extract(source, &path, &mut parsed),
            Grammar::Vue => vue::extract(self, source, &path, &mut parsed)?,
            _ => {
                // Go 的声明以换行结束，文件末尾没有换行同样合法；语法只认换行，这里补上。
                let padded;
                let source = if matches!(grammar, Grammar::Go) && !source.ends_with('\n') {
                    padded = format!("{source}\n");
                    padded.as_str()
                } else {
                    source
                };
                let tree = self.tree(grammar, source)?;
                let root = tree.root_node();
                parsed.syntax_error = root.has_error();
                if !parsed.syntax_error {
                    match grammar {
                        Grammar::Rust => rust::extract(root, source, &path, &mut parsed),
                        Grammar::Js | Grammar::Ts | Grammar::Tsx => {
                            let typescript = !matches!(grammar, Grammar::Js);
                            let site = self.site(&path, typescript);
                            ecma::extract(root, source, site, &mut parsed)
                        }
                        Grammar::Python => python::extract(
                            root,
                            source,
                            &path,
                            self.python_packages.as_ref(),
                            &mut parsed,
                        ),
                        Grammar::Go => go::extract(root, source, &path, &self.go, &mut parsed),
                        Grammar::Java => java::extract(root, source, &path, &mut parsed),
                        Grammar::Vue | Grammar::C | Grammar::Cpp => (),
                    }
                }
            }
        }
        if parsed.syntax_error {
            // 不完整的文件不留半份事实。
            parsed = Parsed {
                syntax_error: true,
                ..Parsed::default()
            };
        }
        Ok(parsed)
    }

    /// 只供有语法树的语言使用；Vue 的脚本块也经这里按块上声明的语言解析。
    fn tree(&mut self, grammar: Grammar, source: &str) -> Result<Tree> {
        let tree = self.parse(grammar, source)?;
        // JavaScript 语法库不接受保留字作 JSX 属性名，Vue、Solid、Preact 里的 `class=`、`for=`
        // 会让整份文件报错；TSX 语法库接受。读不通时用 TSX 再读一遍，读通了就以它为准：
        // 提取用到的导入、导出和声明节点，两套语法里是同一批。
        if matches!(grammar, Grammar::Js) && tree.root_node().has_error() {
            let retry = self.parse(Grammar::Tsx, source)?;
            if !retry.root_node().has_error() {
                return Ok(retry);
            }
        }
        Ok(tree)
    }

    fn parse(&mut self, grammar: Grammar, source: &str) -> Result<Tree> {
        let lang = match grammar {
            Grammar::Rust => tree_sitter_rust::LANGUAGE,
            Grammar::Js => tree_sitter_javascript::LANGUAGE,
            Grammar::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            Grammar::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
            Grammar::Python => tree_sitter_python::LANGUAGE,
            Grammar::Go => tree_sitter_go::LANGUAGE,
            Grammar::Java => tree_sitter_java::LANGUAGE,
            Grammar::Vue | Grammar::C | Grammar::Cpp => anyhow::bail!("该语言不经语法树解析"),
        };
        self.parser.set_language(&lang.into())?;
        self.parser.parse(source, None).context("语法树未生成")
    }
}

impl Parsed {
    /// 也限制不展示的声明、候选和分组叶，避免一条 use 绕过文件证据预算。
    pub fn fact_count(&self) -> usize {
        self.refs.len()
            + self.symbols.len()
            + self.rust_items.len()
            + self
                .rust_modules
                .iter()
                .map(|m| m.candidates.len())
                .sum::<usize>()
            + self.refs.iter().map(|r| r.rust_paths.len()).sum::<usize>()
            + self.provides.len()
            + self.search.len()
    }

    fn symbol(&mut self, name: &str, kind: &'static str, node: Node<'_>) {
        self.declare(name, kind, node, true);
    }

    fn declare(&mut self, name: &str, kind: &'static str, node: Node<'_>, exported: bool) {
        self.symbols.push(Declaration {
            name: name.into(),
            kind,
            line: line(node),
            exported,
        });
    }

    fn reference(
        &mut self,
        spelling: &str,
        kind: &'static str,
        node: Node<'_>,
        candidates: Vec<String>,
        exact: bool,
    ) {
        self.reference_at(spelling, kind, line(node), candidates, exact);
    }

    /// 不经语法树定位的提取（C 的预处理指令、Vue 的块标签）直接给出行号。
    fn reference_at(
        &mut self,
        spelling: &str,
        kind: &'static str,
        line: usize,
        candidates: Vec<String>,
        exact: bool,
    ) {
        // 调用方只传词法已验证的模块名或固定标记；不截取、回显表达式。
        let label = if spelling.len() <= 120 {
            spelling
        } else {
            "module"
        };
        self.refs.push(Reference {
            spelling: label.into(),
            kind,
            line,
            rust_paths: Vec::new(),
            candidates,
            exact,
            substitutes: Vec::new(),
            fallbacks: Vec::new(),
        });
    }

    fn unresolved(&mut self, label: &str, node: Node<'_>) {
        self.reference(label, "unsupported", node, Vec::new(), false);
    }
}

fn text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    &source[node.byte_range()]
}
fn line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

fn child<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut c = node.walk();
    node.named_children(&mut c).find(|n| n.kind() == kind)
}

// 使用语法树游标而非递归 Rust 调用栈，深层表达式也只遍历一次。
fn visit(root: Node<'_>, mut f: impl FnMut(Node<'_>)) {
    let mut c = root.walk();
    loop {
        if c.node().is_named() {
            f(c.node());
        }
        if c.goto_first_child() {
            continue;
        }
        while !c.goto_next_sibling() {
            if !c.goto_parent() {
                return;
            }
        }
    }
}
