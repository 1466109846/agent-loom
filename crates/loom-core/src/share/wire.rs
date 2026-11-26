//! v1 线格式：只从有界字节解出严格的对象；入站与出站共用同一白名单。
use std::sync::LazyLock;

use anyhow::{Result, anyhow, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::admission::{self, Verdict};
use crate::memory::MemoryType;
use crate::redact::redact;
use crate::util::sha256_hex;

pub(super) const MAX_BYTES: usize = 256 * 1024;
pub(super) const MAX_RECORDS: usize = 64;
const FORMAT: &str = "loom-project-share/v1";

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Class {
    Intent,
    Memory,
}

impl Class {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Memory => "memory",
        }
    }
    pub(super) fn limits(self) -> (usize, usize) {
        match self {
            // 与本地意图提议入口的字节上限一致。
            Self::Intent => (192, 1024),
            Self::Memory => (
                4 * admission::MAX_TITLE_CHARS,
                4 * admission::MAX_BODY_CHARS,
            ),
        }
    }
}

/// v1 只交付 Loom 新建的规范 ULID，不猜测旧 ID 是否含用户名、路径或秘密。
pub(super) fn valid_id(id: &str) -> bool {
    id.len() == 26 && ulid::Ulid::from_string(id).is_ok_and(|n| n.to_string() == id)
}

// 已知凭据由现有脱敏器识别。分享比普通保存更保守：可疑字段整项拒绝，不在
// 预览和错误信息中回显原文。不尝试推断其他自然语言是项目资料还是用户授权。
static PRIVATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?ix)
    [a-z]:[/\\] | \\ | :// | //[^\s/] | (?:^|[^a-z0-9_./-])/(?:[^/\s]+)
    | /(?:home|users|root|tmp|etc|var|opt|mnt|volumes)(?:/|\b) | ~[^/\s]*/ | \.\./
    | %(?:userprofile|appdata|localappdata|homedrive|homepath|temp)% | \$(?:\{?home\b|env:)
    | \blocalhost\b | \[?::1\]? | \b(?:\d{1,3}\.){3}\d{1,3}\b | \b[\w.-]+\.\w+:\d{2,5}\b
    | (?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|credential|authorization|proxy|代理地址|密钥|密码|口令)["']?\s*[:=：]
    | \b(?:bearer|basic)\s+[a-z0-9._~+/=-]+ | -----BEGIN\s+[^\r\n]*PRIVATE\s+KEY
    | \b(?:sk-(?:proj-|ant-)?[a-z0-9_-]{12,}|gh[pousr]_[a-z0-9]{12,}|github_pat_[a-z0-9_]+|AKIA[0-9A-Z]{16})
    | (?:user|personal)[\s_-]+(?:preference|profile) | (?:local|machine)[\s_-]+(?:fact|settings)
    | (?:用户|个人|我的)偏好 | 本机(?:事实|配置|代理) | 个人设置
    | \b(?:https?|all|socks)_proxy\b | (?:user|personal|machine)[_-](?:private|style|config)
    | \beyJ[a-z0-9_-]{8,}\.eyJ[a-z0-9_-]{8,}\.[a-z0-9_-]{8,}
"#).expect("内置分享过滤规则无效")
});

fn public_text(text: &str, limit: usize) -> Result<()> {
    ensure!(
        !text.is_empty() && text.len() <= limit && text.trim() == text,
        "分享字段为空、未规整或超过上限"
    );
    ensure!(!text.chars().any(|c| (c.is_control() && c != '\n' && c != '\t') || matches!(c, '\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2060}'..='\u{206f}'|'\u{feff}')), "分享字段含有妨碍人工核查的控制字符");
    ensure!(
        redact(text).1 == 0 && !PRIVATE.is_match(text),
        "疑似含本机路径、地址、凭据或个人资料；拒绝整项分享，请人工整理后再预览"
    );
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub body: String,
    pub kind: String,
    pub origin_project_id: String,
    pub origin_record_id: String,
    pub record_type: Class,
    pub title: String,
}

impl Record {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            valid_id(&self.origin_project_id) && valid_id(&self.origin_record_id),
            "分享来源须有规范的稳定项目及记录编号"
        );
        let (t, b) = self.record_type.limits();
        public_text(&self.title, t)?;
        public_text(&self.body, b)?;
        match self.record_type {
            Class::Intent => {
                ensure!(
                    matches!(self.kind.as_str(), "goal" | "constraint" | "non_goal"),
                    "仅分享项目目标和红线（含非目标）"
                );
                // 不让接收侧 intent_propose 的单行化悄悄改变已经审阅的文字。
                for field in [&self.title, &self.body] {
                    ensure!(
                        field.split_whitespace().collect::<Vec<_>>().join(" ") == *field,
                        "意图文本不符合本地提议格式；请先人工整理"
                    );
                }
            }
            Class::Memory => {
                let ty = MemoryType::ALL
                    .into_iter()
                    .find(|t| t.as_str() == self.kind)
                    .ok_or_else(|| anyhow!("不允许该经验类型"))?;
                if let Verdict::Reject(reason) = admission::check(ty, &self.title, &self.body) {
                    anyhow::bail!("分享经验未通过本地准入：{reason}");
                }
            }
        }
        Ok(())
    }
    fn key(&self) -> (&str, &str) {
        (&self.origin_project_id, &self.origin_record_id)
    }
    pub(super) fn digest(&self) -> Result<String> {
        canonical_sha(self)
    }
}

fn canonical_sha(value: &impl Serialize) -> Result<String> {
    let mut object = serde_json::to_value(value)?;
    object.sort_all_objects();
    Ok(sha256_hex(&serde_json::to_vec(&object)?))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Payload {
    format: String,
    pub records: Vec<Record>,
}

impl Payload {
    fn validate(&self) -> Result<()> {
        ensure!(self.format == FORMAT, "不支持的分享格式或版本");
        ensure!(
            (1..=MAX_RECORDS).contains(&self.records.len()),
            "分享条数须在 1 至 {MAX_RECORDS} 之间"
        );
        for row in &self.records {
            row.validate()?;
        }
        ensure!(
            self.records.windows(2).all(|w| w[0].key() < w[1].key()),
            "分享记录来源重复或未按规范顺序排列"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    pub sha256: String,
    pub payload: Payload,
}

impl Envelope {
    pub(super) fn from_selection(mut records: Vec<Record>) -> Result<Self> {
        records.sort_by(|a, b| a.key().cmp(&b.key()));
        let payload = Payload {
            format: FORMAT.into(),
            records,
        };
        payload.validate()?;
        Ok(Self {
            sha256: canonical_sha(&payload)?,
            payload,
        })
    }
    pub(super) fn text(&self) -> Result<String> {
        let text = serde_json::to_string_pretty(self)? + "\n";
        ensure!(
            text.len() <= MAX_BYTES,
            "分享文件超过 {MAX_BYTES} 字节，请减少选择"
        );
        Ok(text)
    }
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MAX_BYTES, "分享文件超限");
        let invalid =
            || anyhow!("分享 JSON 格式无效：需要完整的 v1 对象，不接受重复、未知或缺失字段");
        // 直接反序列化检查重复键；再核对 JSON 形状，禁止 serde 结构体的位置数组简写。
        let env: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        let raw: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        ensure!(
            raw == serde_json::to_value(&env)?,
            "分享文件必须是具有明确字段名、类型和层次的 v1 对象"
        );
        env.payload.validate()?;
        ensure!(
            env.sha256 == canonical_sha(&env.payload)?,
            "分享文件内容与所附 SHA-256 不符"
        );
        Ok(env)
    }
    pub(super) fn review(&self, reviewed: &str) -> Result<()> {
        ensure!(
            reviewed == self.sha256,
            "内容与人工复核的 SHA-256 不符；请重新预览整份文本"
        );
        Ok(())
    }
}
