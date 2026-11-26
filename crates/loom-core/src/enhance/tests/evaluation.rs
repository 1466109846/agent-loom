//! 冻结样本 v1。序号是夹具文档的稳定标识；先按正文标注每题的相关文档，
//! 再运行检索。不要依据模型或检索输出修改标注。全为合成数据，不打开已有项目。
use super::*;
use crate::search::RecallQuery;

const DOCS: &[(&str, &str)] = &[
    (
        "根标记与项目身份",
        "project.id 用来验证库与当前根目录的绑定。",
    ),
    ("索引只收受管源码", "扫描排除构建产物、嵌套项目和目录链接。"),
    ("索引失败时保留旧快照", "目录无法遍历时，不发布半次更新。"),
    ("大文件索引只登记", "超过尺寸上限的源文件不解析全文。"),
    ("索引复用未变内容", "按指纹跳过未修改源码的重复解析。"),
    ("索引保留歧义引用", "存在同名目标时不能断言唯一依赖。"),
    ("索引中的反向依赖", "评估修改的影响时先查谁使用了这个模块。"),
    (
        "人工说明独立于索引",
        "文件职责由人复核，不因自动刷新改变指纹基线。",
    ),
    ("迁移先预演", "dry-run 只列出将导入哪些历史条目。"),
    ("导入历史去重", "保留 legacy_id，再次导入同一来源不新增。"),
    ("置顶不代表确认", "固定展示次序不改变意图的授权状态。"),
    ("取代意图必须原子化", "新旧决定同时生效或都不生效。"),
    ("任务画布恢复现场", "关闭的任务不出现在待办里。"),
    ("支持和冲突分别记录", "有人工声明的证据边才展示为旁证。"),
    ("未校验的锚点", "缺少历史指纹不能证明今天的源码未变。"),
    ("启动概览有预算", "宿主不能保证词元上界时不自动注入正文。"),
    ("中英混合关键词", "trigram 负责较长的子串，短词走 LIKE。"),
    ("根目录搬迁要核对", "搬家后不能把原址的新工程当作旧项目。"),
];

const QUESTIONS: [(&str, usize); 20] = [
    ("project.id 校验什么？", 1),
    ("扫描 为什么排除构建产物？", 2),
    ("旧快照 何时保留？", 3),
    ("大文件 还会解析全文吗？", 4),
    ("重复解析 如何避免？", 5),
    ("同名目标 能直接算依赖吗？", 6),
    ("反向依赖 怎么读？", 7),
    ("索引 怎么隔开人工描述和机器事实？", 8),
    ("dry-run 会落库吗？", 9),
    ("legacy_id 怎样防止重复？", 10),
    ("置顶 是否等于授权？", 11),
    ("取代意图 一半失败怎么办？", 12),
    ("待办 里会有已关闭任务吗？", 13),
    ("旁证 需要谁声明？", 14),
    ("历史指纹 缺失时能否判断漂移？", 15),
    ("词元 上界未知时怎么办？", 16),
    ("LIKE 在哪条路径用？", 17),
    ("搬家 后如何检查原址？", 18),
    ("如何找回上次做到一半的活儿？", 13),
    ("索引 动一个模块时该先追哪些消费者？", 7),
];

#[test]
fn frozen_twenty_hit_at_five() {
    let (_dir, _loc, store) = project();
    for (i, (title, body)) in DOCS.iter().enumerate() {
        record(&store, i + 1, title, body);
    }
    let (mut lexical_hits, mut mock_hits, mut calls) = (0, 0, 0);
    for (i, &(query, gold)) in QUESTIONS.iter().enumerate() {
        let baseline = store
            .recall(&RecallQuery {
                query: query.into(),
                top_k: 12,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|h| h.memory.id)
            .collect::<Vec<_>>();
        let fallback = rerank_with(&store, query, Model::Off, WAIT).unwrap();
        let original = rows(&fallback);
        assert_eq!(
            original,
            baseline
                .iter()
                .enumerate()
                .map(|(i, id)| (i + 1, id.clone()))
                .collect::<Vec<_>>()
        );
        let reordered = if baseline.is_empty() {
            let trap = trap();
            let report = rerank_with(&store, query, model(port(&trap)), WAIT).unwrap();
            assert!(report.contains("无关键词候选"));
            unused(&trap);
            rows(&report)
        } else {
            calls += 1;
            // 与问题/金标无关：余弦随批内位置递增。这里只验证传输与排序，不模拟语义能力。
            let report = with_mock(
                |stream, req| {
                    assert!(req.head.starts_with("POST /api/embed "));
                    let inputs = req.json["input"].as_array().unwrap();
                    assert_eq!(inputs[0], query);
                    assert_eq!(inputs.len(), original.len() + 1);
                    let mut vectors = vec![[1, 0]];
                    vectors.extend((1..inputs.len() as i32).map(|n| [n, 1]));
                    json_reply(stream, json!({"embeddings": vectors}));
                },
                |p| rerank_with(&store, query, model(p), WAIT).unwrap(),
            );
            assert!(report.contains("候选内重排"));
            let result = rows(&report);
            assert_eq!(result, original.into_iter().rev().collect::<Vec<_>>());
            result
        };
        let doc_numbers = |ids: Vec<&String>| {
            ids.into_iter()
                .take(5)
                .map(|id| ulid::Ulid::from_string(id).unwrap().0 as usize)
                .collect::<Vec<_>>()
        };
        let lexical = doc_numbers(baseline.iter().collect());
        let simulated = doc_numbers(reordered.iter().map(|(_, id)| id).collect());
        lexical_hits += usize::from(lexical.contains(&gold));
        mock_hits += usize::from(simulated.contains(&gold));
        println!(
            "Q{:02} 相关={gold} 关键词前5={lexical:?} 模拟前5={simulated:?}",
            i + 1
        );
    }
    println!(
        "Hit@5（20 题）：关键词={lexical_hits}；模拟={mock_hits}；模拟请求={calls}；未评测真实模型"
    );
    // 初次实测的回归基线；相关性标注在运行前即已固定，模拟数值并非质量门槛。
    assert_eq!((lexical_hits, mock_hits, calls), (17, 19, 19));
}

#[test]
fn frozen_twenty_hybrid_pipeline() {
    use crate::enhance::hybrid;
    use model_server::{Server, embeddings, reply};
    let (_dir, _loc, store) = project();
    for (i, (title, body)) in DOCS.iter().enumerate() {
        record(&store, i + 1, title, body);
    }
    // 向量只由文本字节决定，不读取题号或金标；此运行验证端到端流程，不代表真实模型收益。
    let server = Server::new(|sock, req| {
        let v = embeddings(&req.body, |text| {
            let mut bins = vec![1.; 8];
            for byte in text.bytes() {
                bins[usize::from(byte) % 8] += 1.;
            }
            bins
        });
        reply(sock, 200, &v.to_string());
    });
    let models = || hybrid_recall::configured(server.port, "fixture", 6, None);
    hybrid::enable(&store, &models()).unwrap();
    assert_eq!(
        hybrid::index(&store, &models(), None, |_| ()).unwrap().new,
        DOCS.len()
    );
    let mut hits = 0;
    for (query, gold) in QUESTIONS {
        let r = hybrid::recall(
            &store,
            &RecallQuery {
                query: query.into(),
                top_k: 5,
                ..Default::default()
            },
            models,
        )
        .unwrap();
        assert_eq!(r.heading, "检索方式：关键词 + 向量\n");
        assert_eq!(r.hits.len(), 5);
        hits += usize::from(r.hits.iter().any(|h| h.memory.id == id(gold)));
    }
    println!("冻结 20 题，替身混合 Hit@5={hits}/20；仅验证流程，未评测真实模型");
    assert_eq!(hits, 19);
}
