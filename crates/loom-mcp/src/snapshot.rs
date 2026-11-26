//! 一个 stdio 连接只保留已交付的快照编号，不缓存正文、不在库中为一次读取建表。
use std::sync::Arc;

use anyhow::anyhow;
use loom_core::util::new_id;
use rmcp::model::ClientCapabilities;

use crate::LoomServer;

pub(super) fn accepts_byte_bound(client: &ClientCapabilities) -> bool {
    client
        .experimental
        .as_ref()
        .and_then(|extensions| extensions.get("loom/overview"))
        .and_then(|spec| spec.get("textBudget"))
        .and_then(|value| value.as_str())
        == Some("utf8-byte-upper-bound")
}

impl LoomServer {
    pub(super) async fn snapshot_report(
        &self,
        refresh: bool,
        budget: usize,
    ) -> Result<String, String> {
        let delivered = Arc::clone(&self.delivered_snapshot);
        // 与其它操作沿用同一锁序；并发请求最多有一个能取得快照正文。
        self.knowledge_task(move |store| {
            let mut current = delivered
                .lock()
                .map_err(|_| anyhow!("快照锁已损坏，请重启会话"))?;
            if !refresh && let Some(id) = current.as_deref() {
                // 不返回旧正文，不因缓存命中而绕过现场项目身份核对。
                store.overview_text(0)?;
                return Ok(format!("快照：{id}"));
            }
            let id = new_id();
            let footer = format!("\n快照：{id}");
            let mut text = store.overview_text(budget.saturating_sub(footer.len()))?;
            if text.is_empty() {
                return Ok(text);
            }
            text.push_str(&footer);
            debug_assert!(text.len() <= budget);
            *current = Some(id);
            Ok(text)
        })
        .await
    }
}
