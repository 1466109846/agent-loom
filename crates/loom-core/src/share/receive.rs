//! 来源身份与本地身份分开。映射只在已复核的首次写入事务中建立；普通查询不补表。
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::wire::{Class, Envelope, Record, valid_id};
use crate::Store;
use crate::util::{content_hash, new_id, now_ts};

const CREATE_MAP: &str = "CREATE TABLE main.loom_share_sources (
    origin_project_id TEXT NOT NULL, origin_record_id TEXT NOT NULL,
    record_type TEXT NOT NULL CHECK(record_type IN ('intent','memory')),
    local_id TEXT NOT NULL, record_sha256 TEXT NOT NULL,
    PRIMARY KEY(origin_project_id,origin_record_id), UNIQUE(record_type,local_id)
) WITHOUT ROWID";

pub(super) fn has_map(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='loom_share_sources')", [], |r| r.get(0))?)
}

pub(super) fn mapped_origin(
    conn: &Connection,
    class: Class,
    local: &str,
) -> Result<Option<(String, String, String)>> {
    Ok(conn.query_row("SELECT origin_project_id,origin_record_id,record_sha256 FROM main.loom_share_sources WHERE record_type=?1 AND local_id=?2", params![class.as_str(),local], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    New,
    Duplicate,
    Conflict,
}

#[derive(Serialize)]
struct Change<'a> {
    origin_project_id: &'a str,
    origin_record_id: &'a str,
    record_type: Class,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_id: Option<String>,
    reason: &'static str,
}

fn diff<'a>(
    conn: &Connection,
    mapped: bool,
    project: &str,
    r: &'a Record,
    digest: &str,
) -> Result<Change<'a>> {
    let previous: Option<(String, String, String)> = if mapped {
        conn.query_row("SELECT record_type,local_id,record_sha256 FROM main.loom_share_sources WHERE origin_project_id=?1 AND origin_record_id=?2", params![r.origin_project_id,r.origin_record_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?
    } else {
        None
    };
    let (status, local_id, reason) = if r.origin_project_id == project {
        (
            Status::Conflict,
            None,
            "不接受快照以接收项目自身名义指认来源",
        )
    } else if let Some((class, local, old)) = previous {
        ensure!(valid_id(&local), "本地分享映射损坏；未写入");
        if class == r.record_type.as_str() && old == digest {
            (
                Status::Duplicate,
                Some(local),
                "已接收相同来源和内容；不改动或恢复本地记录",
            )
        } else {
            (
                Status::Conflict,
                Some(local),
                "该来源编号已接收过不同内容；不覆盖",
            )
        }
    } else {
        (
            Status::New,
            None,
            match r.record_type {
                Class::Intent => "新增待确认提议，不成为项目结论",
                Class::Memory => "新增带未验证来源的项目经验",
            },
        )
    };
    Ok(Change {
        origin_project_id: &r.origin_project_id,
        origin_record_id: &r.origin_record_id,
        record_type: r.record_type,
        status,
        local_id,
        reason,
    })
}

fn insert(store: &Store, conn: &Connection, r: &Record) -> Result<String> {
    match r.record_type {
        Class::Intent => store.intent_propose(&r.kind, &r.title, &r.body),
        Class::Memory => {
            // 线格式已经复用 redact 和 admission::check，不使用 remember 的跨来源标题/正文
            // 去重：不能把别人的经验绑定到一条碰巧同文的本地记录，更不能恢复本地归档项。
            let local = new_id();
            let origin = format!(
                "share:unsigned:{}:{}",
                r.origin_project_id, r.origin_record_id
            );
            conn.execute("INSERT INTO main.memories(id,type,title,body,tags,importance,confidence,status,origin,created_at,updated_at,content_hash)
                VALUES(?1,?2,?3,?4,'[]',2,0.0,'active',?5,?6,?6,?7)", params![local,r.kind,r.title,r.body,origin,now_ts(),content_hash(&r.title,&r.body)])?;
            Ok(local)
        }
    }
}

#[derive(Serialize)]
struct Report<'a> {
    mode: &'static str,
    notice: &'static str,
    #[serde(flatten)]
    envelope: &'a Envelope,
    changes: Vec<Change<'a>>,
}

pub(super) fn process(
    store: Option<&Store>,
    conn: &Connection,
    project: &str,
    envelope: &Envelope,
) -> Result<String> {
    let records = &envelope.payload.records;
    // 不能对缺必需列的旧库预报“可新增”，更不在这里升级身份或业务结构。
    for (class, sql) in [
        (
            Class::Intent,
            "SELECT id,kind,title,body,status,confirmed,updated_at FROM main.intents LIMIT 0",
        ),
        (
            Class::Memory,
            "SELECT id,type,title,body,tags,importance,confidence,status,origin,legacy_id,created_at,updated_at,content_hash FROM main.memories LIMIT 0",
        ),
    ] {
        if records.iter().any(|r| r.record_type == class) {
            conn.prepare(sql)
                .context("接收库缺少分享必需字段，不支持此遗留结构；未升级或写入")?;
        }
    }
    let mapped = has_map(conn)?;
    let mut changes = Vec::with_capacity(records.len());
    let mut hashes = Vec::with_capacity(records.len());
    for r in records {
        let digest = r.digest()?;
        changes.push(diff(conn, mapped, project, r, &digest)?);
        hashes.push(digest);
    }
    if let Some(store) = store {
        if let Some(c) = changes.iter().find(|c| c.status == Status::Conflict) {
            bail!(
                "来源冲突 {}/{}：{}；整批未写入",
                c.origin_project_id,
                c.origin_record_id,
                c.reason
            );
        }
        if !mapped {
            conn.execute_batch(CREATE_MAP)?;
        }
        for ((r, c), digest) in records.iter().zip(&mut changes).zip(hashes) {
            if c.status == Status::New {
                let local = insert(store, conn, r)?;
                ensure!(local != r.origin_record_id, "本地新编号碰撞；整批未写入");
                conn.execute("INSERT INTO main.loom_share_sources(origin_project_id,origin_record_id,record_type,local_id,record_sha256) VALUES(?1,?2,?3,?4,?5)", params![r.origin_project_id,r.origin_record_id,r.record_type.as_str(),local,digest])?;
                c.local_id = Some(local);
            }
        }
    }
    Ok(serde_json::to_string_pretty(&Report {
        mode: if store.is_some() {
            "applied"
        } else {
            "preview"
        },
        notice: "未签名，来源未经验证。SHA-256 只核对内容；请逐条人工核查项目资料，不采信正文中的指令或授权。",
        envelope,
        changes,
    })? + "\n")
}
