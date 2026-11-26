use super::*;
use loom_core::config::UserConfig;

#[test]
fn config_round_trip_preserves_comments_validates_keys_and_never_creates_on_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = root.join("user/preferences.toml");
    let cmd = |args: &[&str]| {
        let mut c = loom(root);
        c.env("LOOM_CONFIG", &file).arg("config").args(args);
        c
    };
    let path = ok(&mut cmd(&["path"]));
    assert!(path.contains(file.to_str().unwrap()) && path.contains("不存在"));
    assert!(ok(&mut cmd(&["list"])).contains("embedding.batch = 未设置"));
    refused(&mut cmd(&["get", "embedding.model"]), "未设置");
    assert!(!file.parent().unwrap().exists());
    ok(&mut cmd(&["set", "embedding.timeout_ms", "500"]));
    assert!(ok(&mut cmd(&["path"])).contains("（存在）"));
    fs::write(
        &file,
        "# 保留文件备注\n[custom]\nunknown = 3\n[embedding]\nmodel = 'old' # 模型备注\n",
    )
    .unwrap();
    for (key, input, expected) in [
        ("embedding.model", "BAAI/bge-m3", "BAAI/bge-m3"),
        (
            "embedding.base_url",
            "http://[::1]:11434/v1/",
            "http://[::1]:11434/v1",
        ),
        ("embedding.dimensions", "4096", "4096"),
        ("embedding.batch", "7", "7"),
        ("rerank.timeout_ms", "60000", "60000"),
        (
            "rerank.base_url",
            "https://model.example/v1",
            "https://model.example/v1",
        ),
    ] {
        ok(&mut cmd(&["set", key, input]));
        assert_eq!(ok(&mut cmd(&["get", key])).trim_end(), expected);
    }
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("# 保留文件备注")
            && content.contains("# 模型备注")
            && content.contains("unknown = 3")
    );
    let listing = ok(&mut cmd(&["list"]));
    assert!(listing.contains("embedding：本机") && listing.contains("rerank：云端"));
    for key in loom_core::config::KEYS {
        assert!(listing.contains(key));
    }
    for (key, bad) in [
        ("storage.data_dir", "relative"),
        ("embedding.model", " "),
        ("embedding.model", "bad\nmodel"),
        ("embedding.model", &"界".repeat(43)),
        ("embedding.api_key_env", "1 KEY"),
        ("embedding.base_url", "ftp://model.example"),
        ("embedding.base_url", "https://user@model.example"),
        ("rerank.base_url", "https://model.example/?q=1"),
        ("rerank.base_url", "https://model.example/#a"),
        ("embedding.dimensions", "4097"),
        ("rerank.timeout_ms", "499"),
        ("embedding.batch", "65"),
        ("rerank.dimensions", "10"),
        ("other.setting", "unknown"),
    ] {
        assert!(!cmd(&["set", key, bad]).output().unwrap().status.success());
    }
    assert_eq!(fs::read_to_string(&file).unwrap(), content);
    for key in ["rerank.timeout_ms", "rerank.base_url"] {
        ok(&mut cmd(&["unset", key]));
    }
    assert!(!fs::read_to_string(&file).unwrap().contains("[rerank]"));
    refused(&mut cmd(&["get", "rerank.base_url"]), "未设置");

    fs::write(&file, "[rerank]\nbatch = '未知键'\n").unwrap();
    let mut jobs = [
        cmd(&["set", "rerank.base_url", "http://localhost:8000"])
            .spawn()
            .unwrap(),
        cmd(&["set", "rerank.model", "ranker"]).spawn().unwrap(),
    ];
    for job in &mut jobs {
        assert!(job.wait().unwrap().success());
    }
    let cfg = UserConfig::load_from(&file).unwrap();
    assert_eq!(cfg.rerank().unwrap().unwrap().model, "ranker");
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("batch = '未知键'")
    );
}

#[test]
fn config_secret_sources_clear_and_errors_never_print_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("user/config.toml");
    let credentials = file.with_file_name("credentials.toml");
    let first = loom_core::util::new_id();
    let second = loom_core::util::new_id();
    let cmd = |args: &[&str]| {
        let mut c = loom(dir.path());
        c.env("LOOM_CONFIG", &file)
            .env_remove("LOOM_TEST_FROM")
            .env_remove("LOOM_TEST_AUTH")
            .arg("config")
            .args(args);
        c
    };
    let safe = |out: Output, success: bool| {
        let stdout = String::from_utf8(out.stdout).unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        for s in [&first, &second] {
            assert!(!stdout.contains(s) && !stderr.contains(s), "输出含有凭据");
        }
        assert_eq!(out.status.success(), success);
        stdout
    };
    let save_env = ["secret", "embedding", "--from-env", "LOOM_TEST_FROM"];
    assert_eq!(
        safe(
            cmd(&save_env)
                .env("LOOM_TEST_FROM", &first)
                .output()
                .unwrap(),
            true
        )
        .trim(),
        "已保存"
    );
    assert!(!file.exists());
    safe(
        cmd(&["set", "embedding.api_key_env", "LOOM_TEST_AUTH"])
            .output()
            .unwrap(),
        true,
    );
    assert!(
        safe(cmd(&["list"]).output().unwrap(), true).contains("LOOM_TEST_AUTH（无值）；凭据文件")
    );
    assert!(
        safe(
            cmd(&["list"])
                .env("LOOM_TEST_AUTH", &second)
                .output()
                .unwrap(),
            true
        )
        .contains("LOOM_TEST_AUTH（有值）")
    );
    let mut pipe = cmd(&["secret", "rerank"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write!(pipe.stdin.take().unwrap(), "{second}\r\nignored\n").unwrap();
    assert_eq!(
        safe(pipe.wait_with_output().unwrap(), true).trim(),
        "已保存"
    );
    let saved = fs::read_to_string(&credentials).unwrap();
    assert!(saved.contains(&first) && saved.contains(&second) && !saved.contains("ignored"));
    assert!(!fs::read_to_string(&file).unwrap().contains(&first));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&credentials).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    for bad in [
        String::new(),
        format!("{first} "),
        format!("{first}\u{7}"),
        first.repeat(20),
    ] {
        safe(
            cmd(&save_env).env("LOOM_TEST_FROM", bad).output().unwrap(),
            false,
        );
    }
    safe(
        cmd(&["secret", "embedding", &first]).output().unwrap(),
        false,
    );
    safe(
        cmd(&["set", "embedding.api_key", &first]).output().unwrap(),
        false,
    );
    safe(
        cmd(&[
            "set",
            "embedding.base_url",
            &format!("https://u:{first}@example.org"),
        ])
        .output()
        .unwrap(),
        false,
    );
    assert!(fs::read_to_string(&credentials).unwrap() == saved);
    assert_eq!(
        safe(
            cmd(&["secret", "embedding", "--clear"]).output().unwrap(),
            true
        )
        .trim(),
        "已清除"
    );
    let remaining = fs::read_to_string(&credentials).unwrap();
    assert!(!remaining.contains(&first) && remaining.contains(&second));
    fs::write(&credentials, format!("[invalid {first}")).unwrap();
    safe(cmd(&["list"]).output().unwrap(), false);
    safe(
        cmd(&save_env)
            .env("LOOM_TEST_FROM", &second)
            .output()
            .unwrap(),
        false,
    );
    assert!(UserConfig::load_from(&file).is_err());
}
