//! MCP 可执行的知识操作与默认文本预览；本地确认、复核和声明关系不在此分派。
use std::fmt::Write as _;

use anyhow::{Context, Result, bail, ensure};
use loom_core::{Memory, Store, redact::redact, search::Hit};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::{Drift, LoomServer, fmt_date, marks, now_ts, truncate_chars};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum IntentAction {
    List,
    Get,
    Propose,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct IntentArgs {
    action: IntentAction,
    /// get 必填；本项目意图编号。
    id: Option<String>,
    /// propose 必填：goal / constraint / non_goal / decision。
    kind: Option<String>,
    /// propose 必填。
    title: Option<String>,
    /// propose 必填。
    body: Option<String>,
}

impl IntentArgs {
    pub(super) fn run(self, s: &Store) -> Result<String> {
        use IntentAction::*;
        match (self.action, self.id, self.kind, self.title, self.body) {
            (List, None, None, None, None) => s.intent_list(),
            (Get, Some(id), None, None, None) => s.intent_get(&id),
            (Propose, None, Some(kind), Some(title), Some(body)) => {
                s.intent_propose(&kind, &title, &body)
            }
            (List, ..) => bail!("intent list 不接受附加参数"),
            (Get, ..) => bail!("intent get 只需要 id"),
            (Propose, ..) => bail!("intent propose 需要 kind、title、body，不接受 id"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum NoteAction {
    #[default]
    Get,
    Set,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NoteArgs {
    #[serde(default)]
    action: NoteAction,
    /// 项目内受管文件的相对路径。
    path: String,
    /// set 必填：本次核对的完整 SHA-256，不由服务端代填。
    expected_sha: Option<String>,
    /// set 必填：新的人工职责文本。
    role: Option<String>,
}

impl NoteArgs {
    pub(super) fn run(self, s: &Store) -> Result<String> {
        ensure!(
            !self.path.trim().is_empty() && !self.path.contains(unsafe_display),
            "note 需要非空、可安全显示的单行 path"
        );
        match (self.action, self.expected_sha, self.role) {
            (NoteAction::Get, None, None) => s.note_get(&self.path).map(safe_report),
            (NoteAction::Set, Some(sha), Some(role)) => {
                s.note_set(&self.path, &sha, &role).map(report_preview)
            }
            (NoteAction::Get, ..) => bail!("note get 只需要 path"),
            (NoteAction::Set, ..) => bail!("note set 需要 path、expected_sha、role"),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum CanvasAction {
    List,
    Get,
    Open,
    Update,
    Close,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CanvasArgs {
    action: CanvasAction,
    /// get、update、close 必填：本项目任务编号。
    id: Option<String>,
    /// open 必填。
    goal: Option<String>,
    /// open、update 必填：具体下一步。
    next: Option<String>,
    /// open、update 必填：用户给出的证据位置。
    evidence: Option<String>,
}

impl CanvasArgs {
    pub(super) fn run(self, s: &Store) -> Result<String> {
        use CanvasAction::*;
        match (self.action, self.id, self.goal, self.next, self.evidence) {
            (List, None, None, None, None) => Ok(pending_tasks(s.canvas_pending()?)),
            (Get, Some(id), None, None, None) => s.canvas_get(&id).map(safe_report),
            (Open, None, Some(goal), Some(next), Some(evidence)) => {
                s.canvas_open(&goal, &next, &evidence)
            }
            (Update, Some(id), None, Some(next), Some(evidence)) => {
                s.canvas_update(&id, &next, &evidence)
            }
            (Close, Some(id), None, None, None) => s.canvas_close(&id),
            (List, ..) => bail!("canvas list 不接受附加参数"),
            (Get | Close, ..) => bail!("canvas get/close 只需要 id"),
            (Open, ..) => bail!("canvas open 需要 goal、next、evidence，不接受 id"),
            (Update, ..) => bail!("canvas update 需要 id、next、evidence，不接受 goal"),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct QueryArgs {
    /// 本次显式查询的问题或关键词。
    pub(super) query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TextArgs {
    /// 本次提交供提炼的文本；不从文件、项目记忆或对话补取。
    pub(super) text: String,
}

impl LoomServer {
    /// 文件、SQLite、共享连接锁和可选的本机模型均在阻塞线程中访问。
    pub(super) async fn knowledge_task<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T, String> {
        let server = self.clone();
        tokio::task::spawn_blocking(move || server.with_store(work))
            .await
            .map_err(|e| format!("知识任务未完成：{e}"))?
    }
}

const PREVIEW_BYTES: usize = 1500;
const OMITTED: &str =
    "… 已省略部分内容，不能据此判断没有其他条目或依据。请缩小查询或按编号查看详情。";

/// 未超额的文本原样返回（包括没有换行的 EOF）。否则只在真实行尾回退，
/// 保留提示本身的字节；是否省略来自计量/行数，不搜索不受信的标题或正文。
fn preview(text: String, omitted: bool) -> String {
    preview_with_budget(text, omitted, PREVIEW_BYTES)
}

fn preview_with_budget(mut text: String, omitted: bool, budget: usize) -> String {
    if !omitted && text.len() <= budget {
        return text;
    }
    let room = budget - OMITTED.len();
    let newline = !text.is_empty() && !text.ends_with('\n');
    if text.len() + usize::from(newline) > room {
        let prefix = &text.as_bytes()[..room.min(text.len())];
        text.truncate(
            prefix
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |p| p + 1),
        );
    } else if newline {
        text.push('\n');
    }
    text.push_str(OMITTED);
    text
}

fn unsafe_display(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn one_line(raw: &str) -> String {
    let safe = redact(raw).0;
    if safe.contains(unsafe_display) {
        redact(&safe.replace(unsafe_display, " ")).0
    } else {
        safe
    }
}

/// 换行由核心报告生成；其它控制符或 Unicode 行分隔符不交给宿主解释。
fn safe_report(text: String) -> String {
    let unsafe_in_report = |c| c != '\n' && unsafe_display(c);
    if text.contains(unsafe_in_report) {
        text.replace(unsafe_in_report, " ")
    } else {
        text
    }
}

pub(super) fn report_preview(text: String) -> String {
    preview(safe_report(text), false)
}

fn pending_tasks(text: String) -> String {
    if text.is_empty() {
        "未完成任务：无。\n".into()
    } else {
        safe_report(text)
    }
}

fn resume_prefix(id: &str, heading: &str, tasks: &str, budget: usize) -> String {
    // 长根路径不能挤掉当前任务或真实编号；不保留一个似是而非的半截根路径。
    let heading = if heading.len() + 1 + tasks.len() + OMITTED.len() > budget
        || heading.contains(unsafe_display)
        || redact(heading).1 != 0
    {
        format!("项目：标识={id} · 名称与根路径已省略")
    } else {
        heading.to_owned()
    };
    format!("{heading}\n{tasks}")
}

pub(super) fn resume(store: &Store, hours: i64, newly_bound: bool) -> Result<String> {
    const READY: &str = "其余工具已就绪。宿主没有自动刷新工具列表时，请重连 Loom 服务或新开会话；在那之前可以用同名的命令行。";
    const WINDOW: usize = 200;
    const RECENT: usize = 4;
    const REVIEW: usize = 3;
    let project = store.project().context("恢复现场需要已绑定的项目")?;
    let tasks = pending_tasks(store.canvas_pending()?);
    let budget = if newly_bound {
        PREVIEW_BYTES - 1 - READY.len()
    } else {
        PREVIEW_BYTES
    };
    let mut out = resume_prefix(project.id(), &project.heading(), &tasks, budget);
    let since = now_ts() - hours * 3600;
    let rows = store.recent(0, WINDOW + 1)?;
    let mut omitted = rows.len() > WINDOW;
    let mut recent = Vec::new();
    let mut review = Vec::new();
    for m in rows.iter().take(WINDOW) {
        let (group, limit, label) = match store.drift(&m.id)? {
            Drift::Drift => (&mut review, REVIEW, "需复核·源码已变"),
            Drift::Orphaned => (&mut review, REVIEW, "需复核·锚点已消失"),
            Drift::Unverified => (&mut review, REVIEW, "需复核·锚点未校验"),
            Drift::Clean if m.updated_at >= since => (&mut recent, RECENT, "近期"),
            Drift::Clean => continue,
        };
        if group.len() == limit {
            omitted = true;
            continue;
        }
        group.push(format!(
            "- {label} {} · id={} · {}",
            m.mem_type,
            one_line(&m.id),
            preview_title(m, &mut omitted)
        ));
    }
    let _ = writeln!(
        out,
        "记忆标题（最近 {hours} 小时；锚点复核仅查最近 {WINDOW} 条有效记忆）："
    );
    if recent.is_empty() && review.is_empty() {
        out.push_str("此窗口内无近期标题或需复核项。\n");
    }
    for line in review.into_iter().chain(recent) {
        let _ = writeln!(out, "{line}");
    }
    let mut out = preview_with_budget(out, omitted, budget);
    if newly_bound {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(READY);
    }
    Ok(out)
}

/// 超长的旧标题不应挤掉可取回的完整编号或变成一条半截的证据。
fn preview_title(memory: &Memory, omitted: &mut bool) -> String {
    let title = one_line(&memory.title);
    if title.chars().count() <= loom_core::admission::MAX_TITLE_CHARS {
        title
    } else {
        *omitted = true;
        "（长标题已省略）".into()
    }
}

pub(super) fn recall_preview(hits: &[Hit], mut out: String) -> String {
    if hits.is_empty() {
        out.push_str("没有找到相关记忆。");
        return out;
    }
    let mut omitted = false;
    // 先分配标题、编号和状态，再列完整的锚点，最后才用剩余空间放关联编号的正文预览。
    for (i, h) in hits.iter().enumerate() {
        let m = &h.memory;
        let _ = writeln!(
            out,
            "[{}] {} {} · {} · id={}{}",
            i + 1,
            m.mem_type,
            preview_title(m, &mut omitted),
            fmt_date(m.updated_at),
            one_line(&m.id),
            one_line(&marks(h))
        );
        if out.len() > PREVIEW_BYTES {
            return preview(out, true);
        }
    }
    for (i, h) in hits.iter().enumerate() {
        if let Some(note) = h.fold_note() {
            let _ = writeln!(out, "[{}] {}", i + 1, one_line(&note));
            if out.len() > PREVIEW_BYTES {
                return preview(out, true);
            }
        }
    }
    for (i, h) in hits.iter().enumerate() {
        for anchor in &h.anchors {
            let _ = writeln!(out, "[{}] ↳ {}", i + 1, one_line(anchor));
            if out.len() > PREVIEW_BYTES {
                return preview(out, true);
            }
        }
    }
    for (i, h) in hits.iter().enumerate() {
        let body = one_line(&h.memory.body);
        let brief = truncate_chars(&body, 240);
        omitted |= brief != body;
        let _ = writeln!(out, "[{}] 摘要：{brief}", i + 1);
        if out.len() > PREVIEW_BYTES {
            break;
        }
    }
    preview(out, omitted)
}

#[cfg(test)]
mod tests {
    use crate::{LoomServer, Parameters, RecallArgs, ResumeArgs};
    use loom_core::project::{Bind, ProjectLocation};
    use loom_core::{MemoryType, NewMemory, Store};
    use std::{fs, path::PathBuf};

    struct Project(PathBuf);

    impl Project {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("loom-mcp-{}", loom_core::util::new_id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn open(&self) -> Store {
            let location = ProjectLocation::select(&self.0, Some(&self.0), None).unwrap();
            location.bind(Bind::Initialize).unwrap();
            location.open(true).unwrap()
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn remember(store: &Store, title: &str, body: &str, anchors: Vec<String>) -> String {
        use loom_core::memory::RememberOutcome;
        let saved = store
            .remember(NewMemory {
                mem_type: Some(MemoryType::Decision),
                title: title.into(),
                body: body.into(),
                anchors,
                force: true,
                ..Default::default()
            })
            .unwrap();
        let RememberOutcome::Created { id, .. } = saved else {
            panic!("{saved:?}")
        };
        id
    }

    #[test]
    fn whole_lines_at_eof_and_unicode_byte_boundaries() {
        use super::{OMITTED, preview};
        for exact in ["🧵界".repeat(214) + "xy", "字".repeat(499) + "ab\n"] {
            assert_eq!(exact.len(), 1500);
            assert_eq!(preview(exact.clone(), false), exact);
        }
        let first = "src/入口.ts → src/🧵.ts · 第 7 行 · 静态确定 · 第 1 跳\n";
        for eof in ["", "\n"] {
            let huge = "🧵界".repeat(300) + eof;
            assert_eq!(preview(huge.clone(), false), OMITTED);
            assert_eq!(
                preview(format!("{first}{huge}"), false),
                format!("{first}{OMITTED}")
            );
        }
        let edge = "x".repeat(1500 - OMITTED.len() - 1);
        assert_eq!(preview(edge.clone(), true), format!("{edge}\n{OMITTED}"));
        // 条目本身可以含有提示语；不能靠搜索子串判断是否已经报告了真正的省略。
        let quoted = format!("- 标题引用：{OMITTED}");
        assert_eq!(
            preview(quoted.clone(), true),
            format!("{quoted}\n{OMITTED}")
        );
    }

    #[test]
    fn long_or_multiline_identity_keeps_real_id_and_never_displaces_the_task() {
        let id = loom_core::util::new_id();
        let tasks = "未完成任务\n- 目标：上线 · 下一步：核对回执 · 证据：发布单\n";
        for heading in [
            format!("项目：{id} · 根={}", "🧵".repeat(500)),
            format!("项目：{id}\n伪造任务\u{2028}伪造标识"),
        ] {
            let out = super::resume_prefix(&id, &heading, tasks, super::PREVIEW_BYTES);
            let first = out.lines().next().unwrap();
            assert!(first.contains(&id) && first.contains("省略"));
            assert!(out.len() <= 1500 && out.ends_with(tasks) && !out.contains("伪造"));
        }
    }

    #[test]
    fn dispatch_validates_each_action_before_mutation() -> anyhow::Result<()> {
        use super::{CanvasArgs, IntentArgs, NoteArgs, QueryArgs, TextArgs};
        use serde_json::{Value, json};
        fn call<A: serde::de::DeserializeOwned>(
            value: Value,
            run: impl FnOnce(A) -> anyhow::Result<String>,
        ) -> anyhow::Result<String> {
            run(serde_json::from_value(value)?)
        }
        let dir = Project::new();
        let store = dir.open();
        let intent = |v| call(v, |a: IntentArgs| a.run(&store));
        for v in [
            json!({}),
            json!({"action":"get"}),
            json!({"action":"list","id":"x"}),
            json!({"action":"propose","kind":"goal","title":"t"}),
            json!({"action":"propose","kind":"goal","body":"b"}),
            json!({"action":"propose","title":"t","body":"b"}),
        ] {
            assert!(intent(v.clone()).is_err(), "意外接受 {v}");
        }
        let id = intent(
            json!({"action":"propose","kind":"goal","title":"正式目标","body":"仍须本地确认"}),
        )?;
        for act in ["confirm", "revoke", "replace", "pin", "unpin", "delete"] {
            assert!(intent(json!({"action":act,"id":id})).is_err());
        }
        assert!(intent(json!({"action":"get","id":id}))?.contains("待确认"));
        assert!(intent(json!({"action":"list"}))?.contains(&id));
        let note = |v| call(v, |a: NoteArgs| a.run(&store));
        let path = "main.ts";
        let src = "export const ready = true;";
        fs::write(dir.0.join(path), src)?;
        store.index_structure()?;
        let sha = loom_core::util::sha256_hex(src.as_bytes());
        assert!(note(json!({"path":path}))?.contains(&sha));
        for v in [
            json!({"action":"get"}),
            json!({"action":"other","path":path}),
            json!({"action":"review","expected_sha":sha,"path":path}),
            json!({"action":"set","path":path,"role":"不应写入"}),
            json!({"action":"set","path":path,"expected_sha":sha}),
            json!({"path":path,"role":"不应写入"}),
            json!({"path":" "}),
            json!({"action":"set","path":path,"expected_sha":"0".repeat(64),"role":"不应写入"}),
        ] {
            assert!(note(v.clone()).is_err(), "意外接受 {v}");
        }
        assert!(!note(json!({"path":path}))?.contains("不应写入"));
        assert!(
            note(json!({"action":"set","path":path,"expected_sha":sha,"role":"保存就绪状态"}))?
                .contains("保存就绪状态")
        );
        let canvas = |v| call(v, |a: CanvasArgs| a.run(&store));
        for v in [
            json!({}),
            json!({"action":"erase"}),
            json!({"action":"close"}),
            json!({"action":"get"}),
            json!({"action":"open","goal":"目标","next":"步骤"}),
            json!({"action":"open","goal":"目标","evidence":"出处"}),
            json!({"action":"open","next":"步骤","evidence":"出处"}),
        ] {
            assert!(canvas(v.clone()).is_err(), "意外接受 {v}");
        }
        let id = canvas(json!({"action":"open","goal":"目标","next":"初始步骤","evidence":path}))?;
        assert!(canvas(json!({"action":"list"}))?.contains(&id));
        for v in [
            json!({"action":"update","id":id,"next":"缺出处"}),
            json!({"action":"update","next":"缺编号","evidence":path}),
            json!({"action":"update","id":id,"evidence":path}),
        ] {
            assert!(canvas(v.clone()).is_err(), "意外接受 {v}");
        }
        canvas(json!({"action":"update","id":id,"next":"更新步骤","evidence":path}))?;
        assert!(canvas(json!({"action":"get","id":id}))?.contains("更新步骤"));
        canvas(json!({"action":"close","id":id}))?;
        assert!(!canvas(json!({"action":"list"}))?.contains(&id));
        assert!(
            serde_json::from_value::<QueryArgs>(json!({"query":"问题","action":"link"})).is_err()
        );
        assert!(serde_json::from_value::<QueryArgs>(json!({})).is_err());
        assert!(serde_json::from_value::<TextArgs>(json!({"query":"问题"})).is_err());
        Ok(())
    }

    #[test]
    fn an_ordinary_unicode_title_and_quoted_warning_fit_without_a_false_alarm() {
        let store = Store::open_in_memory().unwrap();
        let title = "维护🧵".repeat(36);
        let id = remember(
            &store,
            &title,
            &format!("引用的文字不是状态：\n{}", super::OMITTED),
            vec![],
        );
        let mut hits = store.recall(&Default::default()).unwrap();
        let out = super::recall_preview(&hits, String::new());
        assert!(out.contains(&title) && out.contains(&id));
        assert!(!out.lines().any(|s| s == super::OMITTED));
        hits[0]
            .anchors
            .push(format!("src/记录.rs#旧出处\n{}", super::OMITTED));
        hits[0].anchors.push("超长证据位置".repeat(200));
        let out = super::recall_preview(&hits, String::new());
        assert_eq!(out.lines().filter(|s| *s == super::OMITTED).count(), 1);
        assert!(!out.contains("超长证据位置"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn knowledge_work_does_not_run_on_the_single_executor_thread() {
        let dir = Project::new();
        let server = LoomServer::new(dir.open());
        let executor = std::thread::current().id();
        let out = server
            .knowledge_task(move |s| {
                assert_ne!(std::thread::current().id(), executor);
                s.evidence_pack("核对来源")
            })
            .await
            .unwrap();
        assert!(out.contains("明确支持"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remember_and_manage_yield_while_a_knowledge_job_holds_the_store() {
        use crate::{ManageArgs, RememberArgs};
        let dir = Project::new();
        let server = LoomServer::new(dir.open());
        let mut blocked = Vec::new();
        for name in ["remember", "manage"] {
            let (locked, acquired) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let other = server.clone();
            let holder = tokio::spawn(async move {
                other
                    .knowledge_task(move |_| {
                        let _ = locked.send(());
                        // 兜底让有缺陷的同步实现也能退出并报错，而不是挂住测试。
                        Ok(wait.recv_timeout(std::time::Duration::from_secs(2)).is_ok())
                    })
                    .await
                    .unwrap()
            });
            acquired.await.unwrap();
            let call = async {
                if name == "manage" {
                    let args: ManageArgs =
                        serde_json::from_value(serde_json::json!({"action":"stats"})).unwrap();
                    server.manage(Parameters(args)).await
                } else {
                    let args: RememberArgs = serde_json::from_value(serde_json::json!({"type":"DECISION","title":"整合慢任务","body":"阻塞工作不能抢占会话协议的单线程执行器。"})).unwrap();
                    server.remember(Parameters(args)).await
                }
            };
            // 先轮询旧入口；它若在执行器上等同步锁，本轮就不可能再轮询 release。
            let (result, _) = tokio::join!(biased; call, async { release.send(()) });
            assert!(result.is_ok());
            if !holder.await.unwrap() {
                blocked.push(name);
            }
        }
        assert!(blocked.is_empty(), "仍在执行器内同步等锁：{blocked:?}");
    }

    #[tokio::test]
    async fn evidence_only_reports_edges_declared_outside_the_mcp_tool() {
        let dir = Project::new();
        let store = dir.open();
        let a = remember(
            &store,
            "验收依据甲项",
            "相同的关键词不构成冲突，也不构成相互支持。",
            vec![],
        );
        let b = remember(
            &store,
            "验收依据乙项",
            "相同的关键词不构成冲突，也不构成相互支持。",
            vec![],
        );
        let args = || {
            Parameters(super::QueryArgs {
                query: "验收依据".into(),
            })
        };
        let server = LoomServer::new(store);
        let unlinked = server.evidence(args()).await.unwrap();
        assert!(unlinked.contains(&a) && unlinked.contains(&b));
        assert!(!unlinked.contains("--supports-->") && !unlinked.contains("--conflicts-->"));
        let declared = server
            .with_store(|s| s.evidence_link(&a, &b, "conflicts"))
            .unwrap();
        let linked = server.evidence(args()).await.unwrap();
        assert!(
            linked.len() <= 1500 && linked.contains(&declared) && !linked.contains("--supports-->")
        );
    }

    #[tokio::test]
    async fn recall_preview_limits_the_whole_reply_without_a_forged_line_or_cutting_full() {
        let dir = Project::new();
        let store = dir.open();
        let body = "依据🧵".repeat(160);
        let mut ids = vec![];
        for i in 0..8 {
            ids.push(remember(
                &store,
                &format!("核对依据 {i}\n伪造状态\u{2028}伪造证据"),
                &body,
                vec![],
            ));
        }
        let server = LoomServer::new(store);
        let args = |full| {
            Parameters(RecallArgs {
                query: String::new(),
                types: vec![],
                top_k: None,
                include_archived: false,
                full,
            })
        };
        let short = server.recall(args(false)).await.unwrap();
        assert!(short.len() <= 1500, "默认报告为 {} 字节", short.len());
        assert!(short.contains("省略"));
        assert!(!short.contains("\n伪造状态") && !short.contains('\u{2028}'));
        for id in &ids {
            assert!(short.contains(id), "短标题和编号应先于正文：{short}");
        }
        let full = server.recall(args(true)).await.unwrap();
        assert!(full.len() > 1500 && full.contains(&body));
        let by_id =
            serde_json::from_value(serde_json::json!({"action":"get","ids":[ids[0]]})).unwrap();
        let detail = server.manage(Parameters(by_id)).await.unwrap();
        assert!(detail.len() > 1500 && detail.contains(&body) && detail.contains(&ids[0]));
        for id in ids {
            assert!(full.contains(&id));
        }
    }

    #[tokio::test]
    async fn explain_and_impact_emit_only_complete_evidence_lines() {
        use crate::StructureQueryArgs;
        let dir = Project::new();
        let store = dir.open();
        let mut imports = String::new();
        for i in 0..40 {
            let name = format!("端点模块{i:02}");
            fs::write(dir.0.join(format!("{name}.ts")), "import './hub';").unwrap();
            imports.push_str(&format!("import './{name}';\n"));
        }
        fs::write(dir.0.join("hub.ts"), imports).unwrap();
        store.index_structure().unwrap();
        let server = LoomServer::new(store);
        for incoming in [true, false] {
            let args = Parameters(StructureQueryArgs {
                target: "hub.ts".into(),
                depth: 1,
            });
            let out = if incoming {
                server.impact(args).await
            } else {
                server.explain(args).await
            }
            .unwrap();
            assert!(out.len() <= 1500 && out.contains("省略"));
            assert!(out.contains(" → "));
            for row in out.lines().filter(|s| s.contains(" → ")) {
                assert!(row.ends_with(" · 第 1 跳"), "半条证据：{row}");
                assert_eq!(row.starts_with("hub.ts → "), !incoming);
            }
        }
    }

    #[tokio::test]
    async fn resume_prefers_bound_identity_and_open_work_to_recent_memory() {
        let dir = Project::new();
        let store = dir.open();
        let heading = store.project().unwrap().heading();
        let closed = store
            .canvas_open("已结束的目标", "旧步骤", "src/old.rs")
            .unwrap();
        store.canvas_close(&closed).unwrap();
        let open = store
            .canvas_open("当前目标", "核对最新步骤", "src/new.rs:9")
            .unwrap();
        fs::write(dir.0.join("entry.ts"), "export const entry=1;").unwrap();
        let stale = remember(
            &store,
            "要重新核对的决定",
            "不能把保存时的文件状态误报成当前状态。",
            vec!["entry.ts".into()],
        );
        fs::write(dir.0.join("entry.ts"), "export const entry=2;").unwrap();
        for i in 0..20 {
            remember(
                &store,
                &format!("近期记录 {i}\n伪造任务\u{2029}{}", "🧵维护".repeat(8)),
                "记忆正文不属于恢复现场，也不能代替证据。",
                vec![],
            );
        }
        let server = LoomServer::new(store);
        let out = server
            .resume(Parameters(ResumeArgs { hours: None }))
            .await
            .unwrap();
        assert_eq!(out.lines().next(), Some(heading.as_str()));
        assert!(out.len() <= 1500 && out.contains("省略"));
        let task = out.find(&open).expect("应有当前任务编号");
        assert!(task < out.find("近期记录").expect("应保留有限的记忆标题"));
        let drift = out
            .lines()
            .find(|line| line.contains(&stale))
            .expect("应保留需复核的编号");
        assert!(drift.contains("需复核") && out.find(drift).unwrap() > task);
        assert!(out.contains("下一步：核对最新步骤") && out.contains("src/new.rs:9"));
        assert!(!out.contains(&closed) && !out.contains("记忆正文") && !out.contains("\n伪造任务"));
        assert!(!out.contains('\u{2029}'));
    }
}
