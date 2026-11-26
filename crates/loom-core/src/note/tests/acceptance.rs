use super::*;

/// 预设问题不从查询结果生成。职责题同时核对独立的符号/源行；影响题要求精确的完整边集和使用方行号。
const APP: &str = "import { encode } from './codec.ts';\nimport { publish } from './queue.ts';\nexport function submit(x: string) { return publish(encode(x)); }\n";
const CODEC: &str =
    "// Stable wire spelling\nexport function encode(x: string) { return JSON.stringify(x); }\n";
const QUEUE: &str = "import { append } from './journal.ts';\nexport function publish(x: string) { return append(x); }\n";
const JOURNAL: &str =
    "const rows: string[] = [];\nexport function append(x: string) { return rows.push(x); }\n";
const REPORT: &str = "import { encode } from './codec.ts';\nexport function render(x: string) { return encode(x); }\n";
const SAMPLE: &[(&str, &str, &str)] = &[
    ("app.ts", APP, "提交入口：编码后投递"),
    ("codec.ts", CODEC, "把字符串编码成 JSON 文本"),
    ("queue.ts", QUEUE, "投递时调用日志追加接口"),
    ("journal.ts", JOURNAL, "把记录追加到内存数组"),
    ("report.ts", REPORT, "生成编码后的展示文本"),
];

fn populate(root: &Path) -> Result<Store> {
    let s = bind(root)?.open(true)?;
    for &(path, source, _) in SAMPLE {
        put(root, path, source)?;
    }
    assert_eq!(s.index_structure()?.parsed, SAMPLE.len());
    for &(path, source, note) in SAMPLE {
        s.note_set(path, &sha256_hex(source.as_bytes()), note)?;
    }
    Ok(s)
}

fn role(
    question: &str,
    path: &str,
    answer: &str,
    symbol: &str,
    line: i64,
    source_fragment: &str,
) -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = populate(d.path())?;
    let text = fs::read(d.path().join(path))?;
    let out = s.note_get(path)?;
    assert_eq!(field(&out, "路径"), path, "{question}");
    assert_eq!(field(&out, "状态"), "当前", "{question}");
    assert!(
        field(&out, "人工职责").contains(answer),
        "{question}: {out}"
    );
    assert_eq!(field(&out, "已知指纹"), sha256_hex(&text));
    let facts = s.query_structure(path, crate::structure::Direction::Outgoing, 1)?;
    assert!(
        facts
            .symbols
            .iter()
            .any(|it| it.name == symbol && it.line == line),
        "{question}: {facts:?}"
    );
    let source_line = std::str::from_utf8(&text)?
        .lines()
        .nth(line as usize - 1)
        .unwrap();
    assert!(
        source_line.contains(source_fragment),
        "{question}: {path}:{line} {source_line}"
    );
    Ok(())
}

// 题号、目标、答案及证据行先冻结，再写实现。每题都是单独的 cargo test 用例。
#[test]
fn q01_submission_role() -> Result<()> {
    role(
        "提交入口如何处理输入？",
        "app.ts",
        "编码后投递",
        "submit",
        3,
        "publish(encode(x))",
    )
}
#[test]
fn q02_codec_role() -> Result<()> {
    role(
        "线协议编码器输出什么？",
        "codec.ts",
        "JSON 文本",
        "encode",
        2,
        "JSON.stringify(x)",
    )
}
#[test]
fn q03_queue_role() -> Result<()> {
    role(
        "投递由哪个接口落到日志？",
        "queue.ts",
        "日志追加",
        "publish",
        2,
        "append(x)",
    )
}
#[test]
fn q04_journal_role() -> Result<()> {
    role(
        "日志具体保存在哪里？",
        "journal.ts",
        "内存数组",
        "append",
        2,
        "rows.push(x)",
    )
}
#[test]
fn q05_report_role() -> Result<()> {
    role(
        "展示入口负责什么？",
        "report.ts",
        "展示文本",
        "render",
        2,
        "encode(x)",
    )
}

use crate::structure::Direction::{self, Incoming, Outgoing};
type Edge<'a> = (&'a str, i64, &'a str);

fn edges(
    question: &str,
    path: &str,
    dir: Direction,
    depth: usize,
    expected: &[Edge<'_>],
) -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = populate(d.path())?;
    let out = s.query_structure(path, dir, depth)?;
    assert_eq!(out.status, "indexed");
    assert!(!out.truncated && out.issues.is_empty());
    let mut actual = Vec::new();
    for it in &out.relations {
        assert_eq!(it.confidence, "exact", "{question}");
        let text = fs::read_to_string(d.path().join(&it.source))?;
        assert!(
            text.lines()
                .nth(it.line as usize - 1)
                .unwrap()
                .contains(&format!("'./{}'", it.target))
        );
        actual.push((it.source.as_str(), it.line, it.target.as_str()));
    }
    actual.sort_unstable();
    assert_eq!(actual, expected, "{question}");
    Ok(())
}

#[test]
fn q06_codec_consumers() -> Result<()> {
    edges(
        "改编码器须检查哪两个直接使用方？",
        "codec.ts",
        Incoming,
        1,
        &[("app.ts", 1, "codec.ts"), ("report.ts", 1, "codec.ts")],
    )
}
#[test]
fn q07_journal_consumer() -> Result<()> {
    edges(
        "谁直接依赖日志？展示入口是否无关？",
        "journal.ts",
        Incoming,
        1,
        &[("queue.ts", 1, "journal.ts")],
    )
}
#[test]
fn q08_journal_two_hops() -> Result<()> {
    edges(
        "改日志如何间接影响提交入口？",
        "journal.ts",
        Incoming,
        2,
        &[("app.ts", 2, "queue.ts"), ("queue.ts", 1, "journal.ts")],
    )
}
#[test]
fn q09_submit_dependencies() -> Result<()> {
    edges(
        "提交的直接依赖中有日志或展示吗？",
        "app.ts",
        Outgoing,
        1,
        &[("app.ts", 1, "codec.ts"), ("app.ts", 2, "queue.ts")],
    )
}
#[test]
fn q10_report_dependencies() -> Result<()> {
    edges(
        "展示是否也会投递？",
        "report.ts",
        Outgoing,
        1,
        &[("report.ts", 1, "codec.ts")],
    )
}
