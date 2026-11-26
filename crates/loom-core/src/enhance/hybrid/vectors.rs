use super::super::text;
use super::{Service, table};
use crate::{
    Store,
    redact::sensitive,
    search::RecallQuery,
    util::{now_ts, sha256_hex},
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

pub(super) fn document(title: &str, body: &str) -> Option<String> {
    let mut doc = format!("{}\n{}", text::clean(title), text::clean(body));
    if sensitive(&doc) {
        return None;
    }
    if let Some((end, _)) = doc.char_indices().nth(2000) {
        doc.truncate(end);
    }
    (!sensitive(&doc)).then_some(doc)
}

pub(super) fn key(model: &Service, dim: usize) -> String {
    format!("{}:{dim}", model.profile)
}

pub(super) fn model_key(s: &Store, model: Option<&Service>) -> Result<Option<String>> {
    let Some(model) = model else { return Ok(None) };
    if let Some(dim) = model.dimensions() {
        return Ok(Some(key(model, dim)));
    }
    if !table(s, "memory_vectors")? {
        return Ok(None);
    }
    Ok(s.conn
        .prepare_cached(
            "SELECT model_key FROM main.memory_vectors WHERE model_key GLOB ?1 LIMIT 1",
        )?
        .query_row([format!("{}:*", model.profile)], |r| r.get(0))
        .optional()?)
}

pub(super) struct Pending {
    pub id: String,
    pub text: String,
    digest: String,
}
#[derive(Default)]
pub(super) struct Inventory {
    pub current: usize,
    pub sensitive: usize,
    pub pending: usize,
    pub batch: Vec<Pending>,
    pub cursor: Option<(i64, String)>,
}

// 建库按更新时间和编号游标向前走；只持有一批待发文本，统计不读取 BLOB。
pub(super) fn inventory(
    s: &Store,
    key: Option<&str>,
    after: Option<&(i64, String)>,
    limit: usize,
    count_all: bool,
) -> Result<Inventory> {
    let mut out = Inventory::default();
    if limit == 0 && !count_all {
        return Ok(out);
    }
    let (join, fields) = if table(s, "memory_vectors")? {
        (
            "LEFT JOIN main.memory_vectors v ON v.memory_id=m.id",
            "v.model_key,v.digest",
        )
    } else {
        ("", "NULL,NULL")
    };
    let bound = if after.is_some() {
        "AND m.updated_at<=?1 AND (m.updated_at<?1 OR m.id>?2)"
    } else {
        ""
    };
    let sql = format!(
        "SELECT m.id,m.updated_at,m.title,m.body,{fields} FROM main.memories m {join}
        WHERE m.status='active' {bound} ORDER BY m.updated_at DESC,m.id"
    );
    let mut st = s.conn.prepare_cached(&sql)?;
    let mut rows = if let Some((time, id)) = after {
        st.query(params![time, id])?
    } else {
        st.query([])?
    };
    while let Some(r) = rows.next()? {
        if !count_all {
            out.cursor = Some((r.get(1)?, r.get(0)?));
        }
        let Some(text) = document(r.get_ref(2)?.as_str()?, r.get_ref(3)?.as_str()?) else {
            out.sensitive += 1;
            continue;
        };
        let digest = sha256_hex(text.as_bytes());
        if key.is_some()
            && r.get_ref(4)?.as_str_or_null()? == key
            && r.get_ref(5)?.as_str_or_null()? == Some(&digest)
        {
            out.current += 1;
            continue;
        }
        out.pending += 1;
        if out.batch.len() < limit {
            let id: String = r.get(0)?;
            out.batch.push(Pending { id, text, digest });
            if !count_all && out.batch.len() == limit {
                break;
            }
        }
    }
    Ok(out)
}

pub(super) fn persist(
    s: &Store,
    model_key: &str,
    batch: &[Pending],
    vectors: &[Vec<f32>],
) -> Result<(usize, usize)> {
    let mut read=s.conn.prepare_cached("SELECT title,body,EXISTS(SELECT 1 FROM main.memory_vectors WHERE memory_id=?1) FROM main.memories WHERE id=?1 AND status='active'")?;
    let mut write=s.conn.prepare_cached("INSERT OR REPLACE INTO main.memory_vectors(memory_id,model_key,digest,dim,vector,embedded_at) VALUES(?1,?2,?3,?4,?5,?6)")?;
    let (mut new, mut updated) = (0, 0);
    for (p, v) in batch.iter().zip(vectors) {
        let current = read
            .query_row([&p.id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })
            .optional()?;
        if let Some((title, body, exists)) = current
            && document(&title, &body).as_ref() == Some(&p.text)
        {
            let blob: Vec<_> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            write.execute(params![
                p.id,
                model_key,
                p.digest,
                v.len() as i64,
                blob,
                now_ts()
            ])?;
            if exists {
                updated += 1;
            } else {
                new += 1;
            }
        }
    }
    Ok((new, updated))
}

pub(super) fn nearest(s: &Store, q: &RecallQuery, key: &str, query: &[f32]) -> Result<Vec<String>> {
    let mut st=s.conn.prepare_cached("SELECT m.id,m.type,m.title,m.body,v.digest,v.vector FROM main.memory_vectors v JOIN main.memories m ON m.id=v.memory_id
        WHERE m.status='active' AND v.model_key=?1 AND v.dim=?2 AND length(v.vector)=4*v.dim")?;
    let mut rows = st.query(params![key, query.len() as i64])?;
    let mut top: Vec<(f64, String)> = Vec::new();
    while let Some(r) = rows.next()? {
        let ty = r.get_ref(1)?.as_str()?;
        if !q.types.is_empty() && !q.types.iter().any(|t| t.as_str() == ty) {
            continue;
        }
        let Some(doc) = document(r.get_ref(2)?.as_str()?, r.get_ref(3)?.as_str()?) else {
            continue;
        };
        if sha256_hex(doc.as_bytes()) != r.get_ref(4)?.as_str()? {
            continue;
        }
        let dot: f64 = r
            .get_ref(5)?
            .as_blob()?
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query)
            .map(|(&b, &q)| f64::from(f32::from_le_bytes(b)) * f64::from(q))
            .sum();
        if !dot.is_finite() {
            continue;
        }
        let id = r.get_ref(0)?.as_str()?;
        let pos = top.partition_point(|(score, other)| {
            *score > dot || (*score == dot && other.as_str() < id)
        });
        if pos < 30 {
            top.insert(pos, (dot, id.to_owned()));
            top.truncate(30);
        }
    }
    Ok(top.into_iter().map(|(_, id)| id).collect())
}
