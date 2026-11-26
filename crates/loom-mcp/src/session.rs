//! 会话记住首次核对的项目和已交付的快照，不记住库的位置或连接。
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use loom_core::Store;
use loom_core::project::{ProjectIdentity, ProjectLocation, Unbound};
use rmcp::handler::server::{router::tool::ToolRouter, tool::ToolCallContext};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{Peer, RequestContext, ServerInitializeError};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

use crate::{CallStore, LoomServer, snapshot};

const GUIDE: &str =
    "这个项目还没有接入 Loom。运行 `loom guide`，按其中的步骤由你完成接入；完成后再调一次 resume。";
const REPAIR: &str = "按上面的提示处理后再调一次 resume；不要删除或新建 .loom 里的任何东西";

/// 助手手边的 `loom` 可能不在 PATH 里，或者是还没有 guide 的旧版：把这个服务自己的程序位置一并给出。
fn hint(first_time: bool) -> String {
    if !first_time {
        return REPAIR.into();
    }
    match std::env::current_exe() {
        Ok(exe) => format!(
            "{GUIDE}\n找不到 loom 命令、或它没有 guide 子命令时，用这个程序的完整路径：{}",
            loom_core::util::display_path(&exe)
        ),
        Err(_) => GUIDE.into(),
    }
}

struct Launch {
    cwd: PathBuf,
    root: Option<PathBuf>,
    db: Option<PathBuf>,
}

impl Launch {
    fn select(&self) -> Result<ProjectLocation> {
        ProjectLocation::select(&self.cwd, self.root.as_deref(), self.db.as_deref())
    }

    fn open(&self, was_bound: bool) -> Result<Store, String> {
        let location = self.select().map_err(|e| {
            let first = !was_bound && matches!(e.downcast_ref(), Some(Unbound::NoProject));
            format!("{e:#}\n{}", hint(first))
        })?;
        location.open(true).map_err(|e| {
            // 显式选一个新根时 select 本身会成功。库丢失（有标记却没有库）或指定错外置库时，
            // 定位器报的是别的错，不能引导去建库。
            let first = !was_bound
                && self.db.is_none()
                && matches!(e.downcast_ref(), Some(Unbound::NoDatabase(_)));
            format!("{e:#}\n{}", hint(first))
        })
    }
}

#[derive(Default)]
pub(super) struct Turn {
    awaiting_binding: bool,
}

struct Session {
    launch: Arc<Launch>,
    turn: Arc<tokio::sync::Mutex<Turn>>,
    identity: OnceLock<ProjectIdentity>,
    delivered_snapshot: Arc<Mutex<Option<String>>>,
    router: ToolRouter<LoomServer>,
}

impl Session {
    async fn with_project<T, F: Future<Output = T>>(
        &self,
        peer: Option<&Peer<RoleServer>>,
        work: impl FnOnce(LoomServer) -> F,
    ) -> Result<T, String> {
        let turn = self.turn.clone().lock_owned().await;
        let launch = self.launch.clone();
        let was_bound = self.identity.get().is_some();
        let (db, mut turn) = tokio::task::spawn_blocking(move || (launch.open(was_bound), turn))
            .await
            .map_err(|e| format!("定位项目的任务未完成：{e}\n{REPAIR}"))?;
        let db = db.inspect_err(|_| turn.awaiting_binding = !was_bound)?;
        let live = db.project().expect("ProjectLocation::open 已核对身份");
        if self.identity.get_or_init(|| live.clone()) != live {
            return Err(format!(
                "项目身份不符；请核对所选根目录和库，未加载项目内容\n{REPAIR}"
            ));
        }
        let newly_bound = std::mem::take(&mut turn.awaiting_binding);
        if newly_bound
            && let Some(peer) = peer
            && let Err(e) = peer.notify_tool_list_changed().await
        {
            eprintln!("无法通知宿主刷新 Loom 工具列表：{e}");
        }
        let store = Arc::new(Mutex::new(CallStore { db, _turn: turn }));
        let result = work(LoomServer {
            store: store.clone(),
            delivered_snapshot: self.delivered_snapshot.clone(),
            newly_bound,
        })
        .await;
        // 最后一个连接关闭时 SQLite 可能要做 WAL 检查点；回复前等它释放句柄。
        tokio::task::spawn_blocking(move || drop(store))
            .await
            .map_err(|e| format!("关闭项目库的任务未完成：{e}"))?;
        Ok(result)
    }
}

impl ServerHandler for Session {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("loom", env!("CARGO_PKG_VERSION")))
        .with_instructions("按需使用 overview；中断后使用 resume。")
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let automatic = snapshot::accepts_byte_bound(&request.capabilities);
        let mut response = self.negotiate_initialize(&request)?;
        context.peer.set_peer_info(request);
        response.instructions = self
            .with_project(None, |tools| async move {
                if !automatic {
                    return Ok(None);
                }
                let text = tools.snapshot_report(false, 800).await.map_err(|_| {
                    ErrorData::internal_error("项目开场快照核对失败，未加载内容", None)
                })?;
                Ok((!text.is_empty()).then_some(text))
            })
            .await
            .unwrap_or(Ok(None))?;
        Ok(response)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if self.identity.get().is_none() {
            let _ = self
                .with_project(Some(&context.peer), |_| std::future::ready(()))
                .await;
        }
        let mut tools = self.router.list_all();
        if self.identity.get().is_none() {
            tools.retain(|tool| tool.name == "resume");
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if name != "resume" && self.identity.get().is_none() {
            return None;
        }
        self.router.get(name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let peer = context.peer.clone();
        match self
            .with_project(Some(&peer), |tools| async move {
                self.router
                    .call(ToolCallContext::new(&tools, request, context))
                    .await
            })
            .await
        {
            Ok(result) => result,
            Err(reason) => Ok(CallToolResult::error(vec![ContentBlock::text(reason)]).into()),
        }
    }
}

/// 在 stdio 上运行 MCP 服务；启动时只固定选择条件，不固定解析出的库路径。
pub async fn serve_stdio(cwd: PathBuf, root: Option<PathBuf>, db: Option<PathBuf>) -> Result<()> {
    let launch = Arc::new(Launch { cwd, root, db });
    let session = Session {
        launch: launch.clone(),
        turn: Arc::default(),
        identity: OnceLock::new(),
        delivered_snapshot: Arc::default(),
        router: LoomServer::tool_router(),
    };
    let service = match session.serve(rmcp::transport::stdio()).await {
        Err(e @ ServerInitializeError::ConnectionClosed(_)) => {
            // stdin 在握手前关闭时没有宿主能收到工具错误；命令行仍须看见选错库的原因。
            let check =
                tokio::task::spawn_blocking(move || launch.select()?.open(false).map(drop)).await?;
            return Err(match check {
                Err(reason) => reason.context(e),
                Ok(()) => e.into(),
            });
        }
        result => result?,
    };
    service.waiting().await?;
    Ok(())
}
