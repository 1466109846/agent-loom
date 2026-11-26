use super::super::{models::Failure, text};
use super::{
    CHANGED, Consent, Models, Service, authorized, checked, consent, inventory, model_key, table,
    vectors,
};
use crate::{
    Store,
    project::ProjectLocation,
    search::{Hit, RecallQuery},
};
use anyhow::{Context, Result, ensure};
use std::{collections::HashMap, path::Path};

pub struct Recall {
    pub heading: String,
    pub hits: Vec<Hit>,
}
impl Recall {
    fn plain(s: &Store, q: &RecallQuery) -> Result<Self> {
        Ok(Self {
            heading: String::new(),
            hits: s.recall(q)?,
        })
    }
    fn report(hits: Vec<Hit>, mode: &str, pending: usize) -> Self {
        let mut heading = format!("检索方式：{mode}\n");
        if pending != 0 {
            heading.push_str(&format!(
                "另有 {pending} 条经验尚未建向量，执行 loom semantic index\n"
            ));
        }
        Self { heading, hits }
    }
}

// generation 还区分等待期间撤销后在同一地址重新授权的情形。
fn guarded<T>(s: &Store, g: &Consent, f: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
    checked(s, || {
        if consent(s)?.as_ref() == Some(g) {
            f().map(Some)
        } else {
            Ok(None)
        }
    })
}
fn fallback(
    s: &Store,
    q: &RecallQuery,
    g: &Consent,
    model: Option<&Service>,
    why: &str,
) -> Result<Recall> {
    match guarded(s, g, || {
        // 模型没配好时谈不上“待建”：这时让人去建向量只会再碰一次同样的错。
        let n = match model {
            Some(_) => {
                let key = model_key(s, model)?;
                inventory(s, key.as_deref(), None, 0, true)?.pending
            }
            None => 0,
        };
        Ok(Recall::report(
            s.recall(q)?,
            &format!("仅关键词（向量不可用：{why}）"),
            n,
        ))
    })? {
        Some(r) => Ok(r),
        None => Recall::plain(s, q),
    }
}

fn lexical(s: &Store, q: &RecallQuery) -> Result<Vec<String>> {
    let query = RecallQuery {
        top_k: 30,
        ..q.clone()
    };
    Ok(s.keyword_candidates(&query)?
        .into_iter()
        .map(|h| h.memory.id)
        .collect())
}
fn fuse(lex: &[String], vec: Vec<String>) -> Vec<String> {
    let mut weights: HashMap<String, f64> = HashMap::new();
    for list in [lex, &vec] {
        for (i, id) in list.iter().enumerate() {
            *weights.entry(id.clone()).or_default() += 1.0 / (61 + i) as f64;
        }
    }
    let mut ranked: Vec<_> = weights.into_iter().collect();
    ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(id, _)| id).collect()
}
fn version(s: &Store) -> Result<i64> {
    Ok(s.conn
        .query_row("PRAGMA main.data_version", [], |r| r.get(0))?)
}

fn documents(s: &Store, ids: &[String]) -> Result<Vec<(usize, String)>> {
    let mut out = Vec::new();
    for (i, id) in ids.iter().take(24).enumerate() {
        if let Some(m) = s.get(id)?
            && m.status == "active"
            && let Some(doc) = vectors::document(&m.title, &m.body)
        {
            out.push((i, doc));
        }
    }
    Ok(out)
}

// CLI 起步仍是只读，且只在已有项目表实际要补建时重开同一个库。
fn writer(s: &Store) -> Result<Store> {
    let root = s.root().context("缺少项目根")?;
    let db = Path::new(s.conn.path().context("缺少项目库位置")?);
    let w = ProjectLocation::select(root, Some(root), Some(db))?.open(true)?;
    ensure!(w.project() == s.project(), "项目身份不符；未写入向量");
    Ok(w)
}

pub fn recall(s: &Store, q: &RecallQuery, load: impl FnOnce() -> Models) -> Result<Recall> {
    if q.query.trim().is_empty() || !table(s, "project_semantic")? {
        return Recall::plain(s, q);
    }
    let Some(g) = checked(s, || consent(s))? else {
        return Recall::plain(s, q);
    };
    // 可选缓存的 SQLite 错误不能阻塞原检索；项目身份或关键词库本身失效仍须报错。
    match enabled(s, q, &g, load()) {
        Err(e) if e.downcast_ref::<rusqlite::Error>().is_some() => checked(s, || {
            let mut r = Recall::plain(s, q)?;
            if consent(s)?.as_ref() == Some(&g) {
                r.heading = "检索方式：仅关键词（向量不可用：向量缓存不可读）\n".into();
            }
            Ok(r)
        }),
        r => r,
    }
}

fn enabled(s: &Store, q: &RecallQuery, g: &Consent, models: Models) -> Result<Recall> {
    let model = match &models.embedding {
        // 项目启用过、这台机器上却没有配置：照样说明，不能悄悄退回关键词。
        Ok(None) => return fallback(s, q, g, None, &Failure::Missing.to_string()),
        Err(e) => return fallback(s, q, g, None, &e.to_string()),
        Ok(Some(m)) => m,
    };
    let back = |why: &str| fallback(s, q, g, Some(model), why);
    if g.embedding != model.origin() {
        return back(CHANGED);
    }
    let Ok(query) = text::input(&q.query, 512) else {
        return back("查询为空或超限，未发送");
    };
    if crate::redact::sensitive(&query) {
        return back("查询仍含敏感内容，未发送");
    }
    if !table(s, "memory_vectors")? {
        return back("向量表未建立");
    }
    let Some((lex, ver, expected, page)) = guarded(s, g, || {
        let key = model_key(s, Some(model))?;
        let page = inventory(s, key.as_deref(), None, model.batch() - 1, false)?;
        Ok((lexical(s, q)?, version(s)?, key, page))
    })?
    else {
        return Recall::plain(s, q);
    };
    let input = std::iter::once(query.as_str())
        .chain(page.batch.iter().map(|p| p.text.as_str()))
        .collect::<Vec<_>>();
    if !authorized(s, g)? {
        return Recall::plain(s, q);
    }
    let reply = model.embed(&input);
    if !authorized(s, g)? {
        return Recall::plain(s, q);
    }
    let batch = match reply {
        Ok(v) => v,
        Err(e) => return back(&e.to_string()),
    };
    let key = vectors::key(model, batch[0].len());
    if expected.is_some_and(|k| k != key) {
        return back(&Failure::Format.to_string());
    }
    if !page.batch.is_empty() {
        let w;
        let dest = if s.conn.is_readonly("main")? {
            match writer(s) {
                Ok(store) => {
                    w = store;
                    &w
                }
                Err(_) => return back("向量缓存不可写"),
            }
        } else {
            s
        };
        match guarded(dest, g, || {
            Ok(dest.savepoint(|_| vectors::persist(dest, &key, &page.batch, &batch[1..])))
        })? {
            None => return Recall::plain(s, q),
            Some(Err(_)) => return back("向量缓存不可写"),
            Some(Ok(_)) => (),
        }
    }
    let ranked = |lex: &[String]| -> Result<Vec<String>> {
        Ok(fuse(lex, vectors::nearest(s, q, &key, &batch[0])?))
    };
    let Some((mut ids, docs, ver)) = guarded(s, g, || {
        let now = version(s)?;
        let lex = if now == ver { lex } else { lexical(s, q)? };
        let ids = ranked(&lex)?;
        let docs = if matches!(&models.rerank,Ok(Some(m)) if Some(m.origin())==g.rerank.as_deref())
        {
            documents(s, &ids)?
        } else {
            Vec::new()
        };
        Ok((ids, docs, now))
    })?
    else {
        return Recall::plain(s, q);
    };
    let rank = match &models.rerank {
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
        Ok(Some(m)) if g.rerank.as_deref() != Some(m.origin()) => Err(CHANGED.into()),
        Ok(Some(_)) if docs.is_empty() => Ok(None),
        Ok(Some(_)) if !authorized(s, g)? => return Recall::plain(s, q),
        Ok(Some(m)) => m
            .rank(
                &query,
                &docs.iter().map(|(_, d)| d.as_str()).collect::<Vec<_>>(),
            )
            .map(Some)
            .map_err(|e| e.to_string()),
    };
    let result = guarded(s, g, || {
        let mut mode = "关键词 + 向量".to_owned();
        if version(s)? != ver {
            ids = ranked(&lexical(s, q)?)?;
            if rank.as_ref().is_ok_and(Option::is_some) {
                mode.push_str("（重排不可用：候选已变化）");
            }
        } else if let Ok(Some(scores)) = &rank {
            let mut order: Vec<_> = docs
                .iter()
                .zip(scores)
                .map(|((i, _), score)| (ids[*i].clone(), score))
                .collect();
            order.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
            for ((i, _), (id, _)) in docs.iter().zip(order) {
                ids[*i] = id;
            }
            mode.push_str(" + 重排");
        }
        if let Err(reason) = rank {
            mode.push_str(&format!("（重排不可用：{reason}）"));
        }
        let span = ids.len().saturating_sub(1).max(1) as f64;
        let mut memories = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            if let Some(m) = s.get(id)? {
                memories.push((m, 1.0 - 0.6 * i as f64 / span));
            }
        }
        let n = inventory(s, Some(&key), None, 0, true)?.pending;
        Ok(Recall::report(
            s.score_candidates(q, memories, crate::search::Selection::Recall)?,
            &mode,
            n,
        ))
    })?;
    match result {
        Some(r) => Ok(r),
        None => Recall::plain(s, q),
    }
}
