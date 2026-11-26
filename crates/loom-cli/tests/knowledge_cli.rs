//! 从用户拿到的 `loom` 二进制出发：每一步都是一个新进程，不用 Store 帮 CLI 准备数据。
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom"))
        .current_dir(root)
        .env_remove("LOOM_EMBED_MODEL")
        .env_remove("LOOM_EXTRACT_MODEL")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[track_caller]
fn success(args: &[&str], out: Output) -> String {
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("CLI 输出 UTF-8")
}

#[track_caller]
fn ok(root: &Path, args: &[&str]) -> String {
    success(args, run(root, args))
}

#[track_caller]
fn refused(root: &Path, args: &[&str], why: &str) -> String {
    let out = run(root, args);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "不应成功：{args:?}");
    assert!(out.stdout.is_empty(), "失败不能输出结果：{args:?}");
    assert!(err.contains(why), "{args:?}: {err}");
    err
}

fn init(root: &Path) -> String {
    ok(root, &["--root", ".", "init"])
}

fn project() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    dir
}

#[track_caller]
fn id(root: &Path, args: &[&str]) -> String {
    let out = ok(root, args);
    let mut lines = out.lines();
    let value = lines.next().expect("缺少编号");
    assert!(
        lines.next().is_none()
            && value.len() == 26
            && value.bytes().all(|b| b.is_ascii_alphanumeric()),
        "期望只有新编号：{out:?}"
    );
    value.to_owned()
}

fn propose(root: &Path, kind: &str, title: &str, body: &str) -> String {
    id(root, &["intent", "propose", kind, title, body])
}

fn remember(root: &Path, ty: &str, title: &str) -> String {
    id(
        root,
        &[
            "remember",
            ty,
            title,
            "必须保留当时依据，以便交接时核对结论。",
        ],
    )
}

#[test]
fn fresh_cli_remembers_and_recalls_without_bypassing_admission_or_redaction() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    refused(
        root,
        &[
            "--root",
            ".",
            "remember",
            "decision",
            "装机",
            "因为现场没有联网条件",
        ],
        "未初始化",
    );
    assert!(!root.join(".loom").exists());
    init(root);
    fs::write(root.join("boot.rs"), "pub fn boot() {}\n").unwrap();

    let marker = "disposable-cli-fixture";
    let title = format!("装机依据 token={marker}");
    let body = format!("因为离线维护时无法访问云端，先验证本地索引。 token={marker}");
    let args = [
        "remember",
        "decision",
        &title,
        &body,
        "--anchor",
        "boot.rs#boot",
    ];
    let saved = id(root, &args);
    let found = ok(root, &["recall", "离线维护"]);
    assert!(
        found.contains("装机依据 token=***") && found.contains("Clean"),
        "{found}"
    );
    assert!(!found.contains(marker));
    assert!(!ok(root, &["recall", marker]).contains("装机依据"));
    let pack = ok(root, &["evidence", "pack", "离线维护"]);
    assert!(
        pack.contains(&saved) && pack.contains("boot.rs#boot"),
        "{pack}"
    );
    fs::write(root.join("boot.rs"), "pub fn boot_v2() {}\n").unwrap();
    assert!(ok(root, &["recall", "离线维护"]).contains("Drift"));

    assert_eq!(id(root, &args), saved, "完全重复时返回原编号");
    let err = refused(
        root,
        &[
            "remember",
            "decision",
            &title,
            "因为环境已经变化，需要核对旧结论。",
        ],
        "相似",
    );
    assert!(err.contains(&saved) && !err.contains(marker));
    refused(root, &["remember", "decision", "拒收", "短"], "信息量不足");
    refused(
        root,
        &["remember", "decision", &title, &body, "--force"],
        "--force",
    );
    refused(
        root,
        &[
            "remember", "decision", &title, &body, "--anchor", "a", "--anchor", "b",
        ],
        "--anchor",
    );
    assert!(ok(root, &["stats"]).contains("合计 1"));
}

#[test]
fn intent_transitions_need_yes_and_only_effective_conclusions_reach_overview() {
    let dir = project();
    let r = dir.path();
    let old = propose(r, "goal", "离线交付", "不把内网可用性作为先决条件");
    let new = propose(r, "goal", "便携交付", "新的完整理由只在按号查询时展开");
    let list = ok(r, &["intent", "list"]);
    assert!(list.contains(&old) && list.contains(&new) && list.contains("待确认"));
    assert!(!list.contains("不把内网可用性"));
    let pending = ok(r, &["overview"]);
    assert!(pending.contains("尚未确认") && !pending.contains(&old));
    refused(r, &["intent", "confirm", &old], "--yes");
    assert!(ok(r, &["intent", "get", &old]).contains("待确认"));
    ok(r, &["intent", "confirm", &old, "--yes"]);
    let active = ok(r, &["overview"]);
    assert!(active.contains(&old) && !active.contains(&new));
    assert!(!active.contains("不把内网可用性"));

    refused(r, &["intent", "replace", &old, &new], "--yes");
    refused(r, &["intent", "replace", &old, &old, "--yes"], "不同");
    refused(r, &["intent", "replace", &old, "absent", "--yes"], "提议");
    assert_eq!(ok(r, &["overview"]), active, "非法替换不能部分撤销旧项");
    ok(r, &["intent", "replace", &old, &new, "--yes"]);
    let changed = ok(r, &["overview"]);
    assert!(changed.contains(&new) && !changed.contains(&old));
    let history = ok(r, &["intent", "get", &old]);
    assert!(
        history.contains("已取代") && history.contains(&new) && history.contains("不把内网可用性")
    );
    let detail = ok(r, &["intent", "get", &new]);
    assert!(detail.contains("已确认") && detail.contains(&old) && detail.contains("新的完整理由"));
    refused(r, &["intent", "revoke", &new], "--yes");
    assert_eq!(ok(r, &["overview"]), changed);
    ok(r, &["intent", "revoke", &new, "--yes"]);
    assert!(ok(r, &["intent", "get", &new]).contains("已撤销"));
    assert_eq!(ok(r, &["overview"]), pending);
}

#[test]
fn optional_enhancements_without_models_return_the_baseline_and_never_save_input() {
    let dir = project();
    let r = dir.path();
    let memory = remember(r, "decision", "机房离线检修");
    let db = r.join(".loom/loom.db");
    let before = fs::read(&db).unwrap();
    let ranked = ok(r, &["rerank", "离线检修"]);
    assert!(
        ranked.contains(&memory) && ranked.contains("机房离线检修"),
        "{ranked}"
    );
    let proposal = "候选文本不应成为项目意图";
    let extracted = ok(r, &["extract", proposal]);
    assert!(extracted.contains("未启用"), "{extracted}");
    assert!(!ok(r, &["intent", "list"]).contains(proposal));
    assert_eq!(fs::read(db).unwrap(), before);
}

#[test]
fn pin_and_unpin_are_explicit_and_do_not_turn_experience_into_a_goal() {
    let dir = project();
    let r = dir.path();
    let memory = remember(r, "gotcha", "首次装机先验证时钟");
    refused(r, &["intent", "pin", &memory], "--yes");
    assert!(!ok(r, &["overview"]).contains(&memory));
    ok(r, &["intent", "pin", &memory, "--yes"]);
    let pinned = ok(r, &["overview"]);
    assert!(pinned.contains(&memory) && pinned.contains("尚未确认"));
    refused(r, &["intent", "unpin", &memory], "--yes");
    assert_eq!(ok(r, &["overview"]), pinned);
    ok(r, &["intent", "unpin", &memory, "--yes"]);
    assert!(!ok(r, &["overview"]).contains(&memory));
    assert!(ok(r, &["recall", "首次装机"]).contains("首次装机先验证时钟"));
}

#[test]
fn manage_reads_full_text_and_only_changes_memories_that_exist() {
    let dir = project();
    let r = dir.path();
    fs::write(r.join("boot.rs"), "pub fn boot() {}\n").unwrap();
    let old = id(
        r,
        &[
            "remember",
            "decision",
            "离线装机流程",
            "因为现场没有联网条件，先核对本地索引再装机。",
            "--anchor",
            "boot.rs#boot",
        ],
    );
    let new = remember(r, "bugfix", "改用便携包交付");
    let recalled = || ok(r, &["recall", "离线装机"]).contains(&old);
    let status = |memory: &str| {
        let detail = ok(r, &["manage", "get", memory]);
        let head = detail.lines().next().unwrap();
        head.split(" · ").nth(2).unwrap().to_owned()
    };

    // 全文：类型、标题、编号、状态、正文和锚点；多个编号各给一段。
    let detail = ok(r, &["manage", "get", &old]);
    for expected in [
        "DECISION 离线装机流程",
        old.as_str(),
        "因为现场没有联网条件，先核对本地索引再装机。",
        "↳ boot.rs#boot",
    ] {
        assert!(detail.contains(expected), "{expected}: {detail}");
    }
    assert_eq!(status(&old), "active");
    let both = ok(r, &["manage", "get", &old, &new]);
    assert!(both.contains(&old) && both.contains("改用便携包交付"));

    // 任一编号不存在：不输出已有的那条，也不改库。
    refused(r, &["manage", "get", &old, "absent"], "不存在");
    refused(r, &["manage", "archive", &old, "absent"], "不存在");
    assert!(recalled() && status(&old) == "active");

    // 归档后默认检索不再出现，全文仍可按编号取回；恢复后重新出现。
    assert_eq!(ok(r, &["manage", "archive", &old]).trim(), "已归档 1 条");
    assert!(!recalled() && status(&old) == "archived");
    assert_eq!(ok(r, &["manage", "activate", &old]).trim(), "已恢复 1 条");
    assert!(recalled());

    // 取代要指明新记忆，不能取代自身；旧条目保留并标为已取代。
    refused(r, &["manage", "supersede", &old], "--by");
    refused(r, &["manage", "supersede", &old, "--by", &old], "相同");
    refused(
        r,
        &["manage", "supersede", &old, "--by", "absent"],
        "不存在",
    );
    assert_eq!(status(&old), "active");
    let receipt = ok(r, &["manage", "supersede", &old, "--by", &new]);
    assert!(
        receipt.contains("1 条旧记忆") && receipt.contains(&new),
        "{receipt}"
    );
    assert!(!recalled() && status(&old) == "superseded");
    assert_eq!(status(&new), "active");

    // 删除不可恢复：没有 --yes 不执行。
    refused(r, &["manage", "delete", &old], "--yes");
    assert!(ok(r, &["stats"]).contains("合计 2"));
    assert_eq!(
        ok(r, &["manage", "delete", &old, "--yes"]).trim(),
        "已删除 1 条"
    );
    refused(r, &["manage", "get", &old], "不存在");
    assert!(ok(r, &["stats"]).contains("合计 1"));
}

#[test]
fn manual_overview_counts_the_entire_stdout_even_at_the_exact_byte_boundary() {
    let dir = project();
    let r = dir.path();
    let add_rule = |title: &str| {
        let rule = propose(r, "constraint", title, "红线的完整依据不应自动展开");
        ok(r, &["intent", "confirm", &rule, "--yes"]);
    };
    let empty = ok(r, &["overview"]);
    assert!(empty.lines().next().unwrap().contains("标识="));
    add_rule("核对");
    let first = ok(r, &["overview"]);
    // 从真实输出量出每行编号与标签占用，不假定项目目录长度；让候选恰好填满 2000 字节。
    let frame = first.trim_end().len() - empty.trim_end().len() - "核对".len();
    let more = 11;
    let title_bytes = 2_000 - first.trim_end().len() - more * frame;
    for i in 0..more {
        let n = title_bytes / more + usize::from(i < title_bytes % more);
        add_rule(&format!("{}{}", "界".repeat(n / 3), "x".repeat(n % 3)));
    }
    let exact = ok(r, &["overview"]);
    assert!(
        exact.len() > 800 && exact.len() <= 2_000,
        "整份 stdout：{} 字节",
        exact.len()
    );
    add_rule("超出预算后的新约束");
    let limited = ok(r, &["overview"]);
    assert!(limited.len() <= 2_000 && limited.contains("还有约束未展示"));
    assert!(!limited.contains("红线的完整依据"));
    assert!(!ok(r, &["stats"]).contains("还有约束未展示"));
}

#[test]
fn note_review_uses_the_observed_sha_and_does_not_follow_reindex_implicitly() {
    let dir = project();
    let r = dir.path();
    let path = "writer.ts";
    let role = "保存离线凭证";
    fs::write(r.join(path), "export const version = 1;\n").unwrap();
    ok(r, &["index"]);
    let get = || ok(r, &["note", "get", path]);
    let live_sha = |report: &str| {
        report
            .lines()
            .find_map(|s| s.strip_prefix("当前指纹："))
            .unwrap()
            .to_owned()
    };
    let first = live_sha(&get());
    assert_eq!(first.len(), 64);
    refused(r, &["note", "set", path, &"0".repeat(64), role], "指纹");
    ok(r, &["note", "set", path, &first, role]);
    assert!(get().contains(&format!("人工职责：{role}")));
    fs::write(r.join(path), "export const version = 2;\n").unwrap();
    ok(r, &["index"]);
    let stale = get();
    let second = live_sha(&stale);
    assert_ne!(first, second);
    assert!(stale.contains("需复核") && stale.contains(&format!("已知指纹：{first}")));
    refused(r, &["note", "review", path, &second], "--yes");
    refused(r, &["note", "review", path, &first, "--yes"], "指纹");
    assert_eq!(get(), stale);
    ok(r, &["note", "review", path, &second, "--yes"]);
    let current = get();
    assert!(current.contains("状态：当前") && current.contains(&format!("已知指纹：{second}")));
    fs::remove_file(r.join(path)).unwrap();
    let missing = get();
    assert!(missing.contains("文件消失") && missing.contains(role));
}

#[test]
fn canvas_resumes_only_open_tasks_but_keeps_history_after_close() {
    let dir = project();
    let r = dir.path();
    let task = id(r, &["canvas", "open", "断点续做", "核对接口", "spec.rs:12"]);
    let pending = || ok(r, &["canvas", "list"]);
    let list = pending();
    for expected in [&task[..], "断点续做", "核对接口", "spec.rs:12"] {
        assert!(list.contains(expected), "{list}");
    }
    ok(r, &["canvas", "update", &task, "跑验收", "test.rs:7"]);
    let list = pending();
    assert!(list.contains("跑验收") && list.contains("test.rs:7") && !list.contains("核对接口"));
    ok(r, &["canvas", "close", &task]);
    assert!(!pending().contains(&task));
    let history = ok(r, &["canvas", "get", &task]);
    for expected in ["已关闭", "跑验收", "核对接口", "spec.rs:12", "test.rs:7"] {
        assert!(history.contains(expected), "{history}");
    }
    refused(
        r,
        &["canvas", "update", &task, "不能再写", "spec.rs:14"],
        "关闭",
    );
    assert!(ok(r, &["stats"]).contains("合计 0"));
}

#[test]
fn evidence_requires_explicit_typed_links_and_preserves_their_direction() {
    let dir = project();
    let r = dir.path();
    let topic = remember(r, "decision", "离线协议");
    let pro = remember(r, "bugfix", "离线现场记录");
    let con = remember(r, "gotcha", "断网柜台反例");
    let pack = || ok(r, &["evidence", "pack", "离线协议"]);
    let before = pack();
    assert!(
        before.contains(&topic)
            && before.contains("缺人工支持")
            && before.contains("缺人工冲突记录")
    );
    refused(r, &["evidence", "link", &pro, &topic, "supports"], "--yes");
    refused(
        r,
        &["evidence", "link", &pro, &topic, "related", "--yes"],
        "supports",
    );
    assert_eq!(pack(), before);
    ok(r, &["evidence", "link", &pro, &topic, "supports", "--yes"]);
    ok(r, &["evidence", "link", &topic, &con, "conflicts", "--yes"]);
    let after = pack();
    assert!(
        after.contains(&format!("{pro} --supports--> {topic}")),
        "{after}"
    );
    assert!(
        after.contains(&format!("{topic} --conflicts--> {con}")),
        "{after}"
    );
    assert!(!after.contains("缺人工支持") && !after.contains("缺人工冲突记录"));
}

#[test]
fn queries_work_on_a_read_only_database_without_changing_its_bytes() {
    let dir = project();
    let r = dir.path();
    let goal = propose(r, "goal", "只读核对", "只查不改");
    let task = id(
        r,
        &["canvas", "open", "核对提交", "查看快照", "writer.ts:1"],
    );
    let memory = remember(r, "decision", "只读核对的依据");
    let db = r.join(".loom/loom.db");
    let before = fs::read(&db).unwrap();
    let original = fs::metadata(&db).unwrap().permissions();
    let mut permissions = original.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&db, permissions).unwrap();
    let queries: &[&[&str]] = &[
        &["overview"],
        &["intent", "list"],
        &["intent", "get", &goal],
        &["note", "get", "writer.ts"],
        &["canvas", "list"],
        &["canvas", "get", &task],
        &["evidence", "pack", "无记录"],
        &["rerank", "无记录"],
        &["extract", "本次交接须有明确依据"],
        &["manage", "get", &memory],
    ];
    let outputs: Vec<_> = queries.iter().map(|args| run(r, args)).collect();
    fs::set_permissions(&db, original).unwrap();
    for (args, out) in queries.iter().zip(outputs) {
        assert!(!success(args, out).is_empty(), "{args:?}");
    }
    assert_eq!(fs::read(db).unwrap(), before);
}

#[test]
fn every_knowledge_entry_refuses_a_foreign_db_without_disclosure_or_writes() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a/same");
    let b = dir.path().join("b/same");
    for root in [&a, &b] {
        fs::create_dir_all(root).unwrap();
        init(root);
    }
    let private = "甲方私有约束";
    let memory = remember(&a, "decision", private);
    let goal = propose(&a, "goal", private, "不应在另一项目显示");
    let task = id(&a, &["canvas", "open", private, "查资料", "local.md"]);
    let adb = a.join(".loom/loom.db");
    let bdb = b.join(".loom/loom.db");
    let before = (fs::read(&adb).unwrap(), fs::read(&bdb).unwrap());
    let sha = "0".repeat(64);
    let cases: &[&[&str]] = &[
        &["overview"],
        &["remember", "decision", "误写", "不能写入别人的库"],
        &["intent", "propose", "goal", "误写", "理由"],
        &["intent", "list"],
        &["intent", "get", &goal],
        &["intent", "confirm", &goal, "--yes"],
        &["intent", "revoke", &goal, "--yes"],
        &["intent", "replace", &goal, &task, "--yes"],
        &["intent", "pin", &memory, "--yes"],
        &["intent", "unpin", &memory, "--yes"],
        &["note", "get", "lib.rs"],
        &["note", "set", "lib.rs", &sha, "职责"],
        &["note", "review", "lib.rs", &sha, "--yes"],
        &["canvas", "open", "目标", "下一步", "lib.rs"],
        &["canvas", "list"],
        &["canvas", "get", &task],
        &["canvas", "update", &task, "下一步", "lib.rs"],
        &["canvas", "close", &task],
        &["evidence", "pack", "约束"],
        &["evidence", "link", &memory, &task, "supports", "--yes"],
        &["rerank", "约束"],
        &["extract", "仅审阅本次给出的文本"],
        &["manage", "get", &memory],
        &["manage", "archive", &memory],
        &["manage", "activate", &memory],
        &["manage", "supersede", &memory, "--by", &task],
        &["manage", "delete", &memory, "--yes"],
    ];
    for args in cases {
        let mut call = vec!["--root", ".", "--db", adb.to_str().unwrap()];
        call.extend_from_slice(args);
        let err = refused(&b, &call, "项目身份不符");
        assert!(!err.contains(private));
    }
    assert_eq!((fs::read(adb).unwrap(), fs::read(bdb).unwrap()), before);
}
