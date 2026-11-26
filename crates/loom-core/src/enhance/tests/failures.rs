use super::*;

#[test]
fn bad_batch_never_partially_reorders() {
    let (_dir, _loc, s) = project();
    record(&s, 1, "主题一", PRIVATE_BODY);
    record(&s, 2, "主题二", PRIVATE_BODY);
    let baseline = rows(&rerank_with(&s, "主题", Model::Off, WAIT).unwrap());
    for body in [
        json!({"error":PRIVATE_BODY}),
        json!({"embeddings":[[1],[2]]}),
        json!({"embeddings":[[1],[2],[0]]}),
        json!({"embeddings":[[],[],[]]}),
        json!({"embeddings":[[1],[2,3],[4]]}),
        json!({"embeddings":[[1], [2], [null]]}),
        json!({"embeddings":vec![vec![1; 4097]; 3]}),
        json!({"error":PRIVATE_BODY,"embeddings":[[1],[2],[3]]}),
    ]
    .iter()
    .map(Value::to_string)
    .chain([
        "{bad".into(),
        r#"{"embeddings":[[1],[1],[1e999]]}"#.into(),
        r#"{"embeddings":[[1],[NaN],[1]]}"#.into(),
    ]) {
        let report = with_mock(
            |stream, _| {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            },
            |p| rerank_with(&s, "主题", model(p), WAIT).unwrap(),
        );
        assert!(
            report.contains("关键词回退")
                && report.contains("不可用")
                && !report.contains(PRIVATE_BODY)
        );
        assert_eq!(rows(&report), baseline);
    }
}

#[test]
fn status_redirect_and_response_limits_fail_closed() {
    let (_dir, _loc, s) = project();
    let sink = trap();
    let chunk = json!({"response":"Z".repeat(40 * 1024)}).to_string();
    let ok = json!({"response":"正文不应显示"}).to_string();
    for (case, response) in [
        format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{}/private\r\nContent-Length: 0\r\n\r\n", port(&sink)),
        format!("HTTP/1.1 503 Unavailable\r\nContent-Length: {}\r\n\r\n{ok}", ok.len()),
        "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n".into(),
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{chunk}", chunk.len()),
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n", chunk.len()),
        format!("HTTP/1.1 200 OK\r\nX-Padding: {chunk}\r\nContent-Length: {}\r\n\r\n{ok}", ok.len()),
    ].into_iter().enumerate() {
        let report = with_mock(|stream, _| { let _ = stream.write_all(response.as_bytes()); },
            |p| extract_with(&s, "仅供提炼", model(p), WAIT).unwrap());
        assert!(report.contains("不可用") && !report.contains(PRIVATE_BODY) && !report.contains("ZZZ"), "HTTP 拒绝用例 {case}");
    }
    unused(&sink);
    for bad in [
        json!({}),
        json!({"response":[]}),
        json!({"response":"半份", "done":false}),
        json!({"response":"半份", "done":true, "done_reason":"length"}),
        json!({"error":PRIVATE_BODY,"response":"半份"}),
    ] {
        let report = with_mock(
            |stream, _| json_reply(stream, bad),
            |p| extract_with(&s, "仅供提炼", model(p), WAIT).unwrap(),
        );
        assert!(report.contains("不可用") && !report.contains("半份"));
    }
}

#[test]
fn partial_private_key_is_never_sent_or_rendered() {
    // 显式片段和受生成长度限制的回答都可能缺 PEM 结束边界，不能只匹配完整块。
    let (_dir, _loc, s) = project();
    let fragment = [
        "-----BEGIN ",
        "PRIVATE KEY-----\n",
        "fixture-material-without-end",
    ]
    .concat();
    with_mock(
        |stream, req| {
            assert!(!req.json.to_string().contains("fixture-material"));
            json_reply(stream, json!({"response": "建议"}));
        },
        |p| extract_with(&s, &fragment, model(p), WAIT).unwrap(),
    );
    let report = with_mock(
        |stream, _| json_reply(stream, json!({"response":fragment})),
        |p| extract_with(&s, "当次文本", model(p), WAIT).unwrap(),
    );
    assert!(!report.contains("fixture-material"));
}

#[test]
fn slow_stream_does_not_renew_the_deadline() {
    let (_dir, _loc, s) = project();
    let (report, elapsed) = with_mock(
        |stream, _| {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n");
            for _ in 0..64 {
                if stream.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
        },
        |p| {
            let start = Instant::now();
            let report =
                extract_with(&s, "当次输入", model(p), Duration::from_millis(350)).unwrap();
            (report, start.elapsed())
        },
    );
    assert!(
        report.contains("不可用") && elapsed < Duration::from_millis(1500),
        "耗时 {elapsed:?}"
    );
}

#[test]
fn valid_chunked_json_is_supported() {
    let (_dir, _loc, s) = project();
    let report = with_mock(
        |stream, _| {
            let body = json!({"response":"仅作建议"}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n", &body[..3], body.len()-3, &body[3..]).unwrap();
        },
        |p| extract_with(&s, "当次输入", model(p), WAIT).unwrap(),
    );
    assert!(report.contains("仅作建议") && report.contains("待人工审阅"));
}
