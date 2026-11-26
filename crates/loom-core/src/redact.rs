//! 凭据脱敏：写入前把密钥、口令、连接串里的凭据替换掉。
//!
//! 注意：不要在键名模式前加 `\b`。`_` 属于单词字符，`NEO4J_PASSWORD=x`
//! 里 `_P` 之间没有单词边界，加了 `\b` 就会漏掉（oh-memos 踩过的坑）。

use std::sync::{LazyLock, OnceLock};

use regex::{Match, Regex, RegexSet};

const MASK: &str = "***";
// 捕获左侧分隔符以原样保留；不要从 task- 或 risk- 的中间开始匹配。只把 ASCII 字母和数字
// 算作词的一部分：中文里密钥常紧跟在汉字后面，中间没有空格。
const SK: &str = r"(^|[^A-Za-z0-9])sk-[A-Za-z0-9_-]{16,}";

struct Rule {
    pattern: &'static str,
    replace: &'static str,
    // 用到才编译：结构索引对每个名字只过预筛，绝大多数进程一条都用不上。
    compiled: OnceLock<Regex>,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let r = |pattern: &'static str, replace: &'static str| Rule {
        pattern,
        replace,
        compiled: OnceLock::new(),
    };
    vec![
        // PEM 私钥块
        r(
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY-----***-----END PRIVATE KEY-----",
        ),
        // URL 里的 user:pass@
        r(
            r"(?i)([a-z][a-z0-9+.\-]*://)([^:/\s@]+):([^@\s/]+)@",
            "${1}${2}:***@",
        ),
        // Authorization: Bearer xxx
        r(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=\-]{16,}", "${1}***"),
        // 常见厂商密钥前缀
        r(SK, "${1}sk-***"),
        r(r"gh[pousr]_[A-Za-z0-9]{20,}", "gh*_***"),
        r(r"github_pat_[A-Za-z0-9_]{50,}", "github_pat_***"),
        r(r"glpat-[A-Za-z0-9_-]{20,}", "glpat-***"),
        r(r"AIza[0-9A-Za-z_-]{35}", "AIza***"),
        r(
            r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            "eyJ***",
        ),
        r(r"AKIA[0-9A-Z]{16}", "AKIA***"),
        r(r"xox[abprs]-[A-Za-z0-9\-]{10,}", "xox*-***"),
        // key = value / key: value 形式
        r(
            r#"(?i)((?:api[_\-]?key|secret(?:[_\-]?key)?|access[_\-]?key|auth[_\-]?token|token|pass(?:word|wd)?|pwd)["']?\s*[:=]\s*["']?)([^\s"'`,;]{4,})"#,
            "${1}***",
        ),
    ]
});

static PREFILTER: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(RULES.iter().map(|r| r.pattern)).expect("内置脱敏正则必须合法"));

impl Rule {
    fn re(&self) -> &Regex {
        self.compiled
            .get_or_init(|| Regex::new(self.pattern).expect("内置脱敏正则必须合法"))
    }

    fn accepts(&self, m: Match<'_>) -> bool {
        let text = m.as_str();
        if self.pattern == SK {
            // 连字符拼起来的小写单词是名字（sk-folding-cube-wrapper），其余都当密钥：
            // 随机串里也有分隔符，全小写的长串也是密钥，宁可多掩。
            let (_, tail) = text.split_once("sk-").unwrap();
            return !tail
                .split(['-', '_'])
                .all(|s| s.len() < 16 && s.bytes().all(|b| b.is_ascii_lowercase()));
        }
        !text.contains(MASK)
    }
}

/// 用于不能替换为掩码的定位键；普通标识符只过一次预筛。
pub fn sensitive(text: &str) -> bool {
    PREFILTER.is_match(text)
        && PREFILTER.matches(text).iter().any(|i| {
            let rule = &RULES[i];
            rule.re().find_iter(text).any(|m| rule.accepts(m))
        })
}

/// 返回脱敏后的文本和命中的处数。
pub fn redact(input: &str) -> (String, usize) {
    let mut text = input.to_string();
    let mut hits = 0;
    for rule in RULES.iter() {
        let mut out = String::new();
        let mut end = 0;
        for caps in rule.re().captures_iter(&text) {
            let m = caps.get(0).unwrap();
            if rule.accepts(m) {
                out.push_str(&text[end..m.start()]);
                caps.expand(rule.replace, &mut out);
                end = m.end();
                hits += 1;
            }
        }
        if end != 0 {
            out.push_str(&text[end..]);
            text = out;
        }
    }
    (text, hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_env_style_with_underscore_prefix() {
        let (out, n) = redact("NEO4J_PASSWORD=hunter2hunter");
        assert_eq!(out, "NEO4J_PASSWORD=***");
        assert_eq!(n, 1);
    }

    #[test]
    fn masks_url_credentials() {
        let (out, _) = redact("bolt://neo4j:s3cretpw@localhost:7687");
        assert_eq!(out, "bolt://neo4j:***@localhost:7687");
    }

    #[test]
    fn masks_vendor_keys_and_bearer() {
        let input = format!(
            "key sk-{} and Bearer abcdefghijklmnopqrstuvwx",
            "aB9c".repeat(5)
        );
        assert_eq!(redact(&input), ("key sk-*** and Bearer ***".to_owned(), 2));
    }

    #[test]
    fn sk_keys_do_not_mask_hyphenated_names() {
        // 词中间的 sk-，和连字符拼起来的小写单词，都是名字。
        let names = format!(
            "task-scheduler-configuration.ts risk-assessment-dashboard-view \
             ask-{0} 7sk-{0} sk-{1} sk-{2}",
            "a1b2c3d4".repeat(2),
            "scheduler-configuration",
            "folding-cube-wrapper_container",
        );
        let input = format!("{names} (sk-proj-{})", "a1b2c3d4".repeat(2));
        assert!(!sensitive(&names));
        assert!(sensitive(&input));
        assert_eq!(redact(&input), (format!("{names} (sk-***)"), 1));
        // 其余都当密钥：纯数字、不含数字的大小写混合、全小写的长串、被分隔符切短的随机串。
        check_token(format!("sk-{}", "12345678".repeat(2)), "sk-***");
        check_token(format!("sk-{}", "aBcD".repeat(4)), "sk-***");
        check_token(format!("sk-{}", "abcdefgh".repeat(2)), "sk-***");
        check_token(format!("sk-{}", "aB3_xY9-".repeat(2)), "sk-***");
    }

    #[test]
    fn sk_keys_are_masked_right_after_chinese_text() {
        let key = format!("sk-{}", "aB9c".repeat(5));
        let input = format!("密钥{key}紧跟在汉字后面，引号里也一样：“{key}”。");
        assert_eq!(
            redact(&input),
            (
                "密钥sk-***紧跟在汉字后面，引号里也一样：“sk-***”。".to_owned(),
                2
            )
        );
    }

    #[test]
    fn leaves_innocent_text_alone() {
        let s = "tokenize = 'trigram'，max_tokens: 1000，password 字段要脱敏";
        assert_eq!(redact(s), (s.to_string(), 0));
    }

    fn check_token(token: String, mask: &str) {
        let short = &token[..token.len() - 1];
        assert!(!sensitive(short));
        assert_eq!(redact(short), (short.to_owned(), 0));
        assert!(sensitive(&token));
        assert_eq!(redact(&token), (mask.to_owned(), 1));
        assert_eq!(redact(mask), (mask.to_owned(), 0));
    }

    #[test]
    fn masks_github_pat() {
        check_token(
            format!("github_pat_{}", "a_b12".repeat(10)),
            "github_pat_***",
        );
    }

    #[test]
    fn masks_gitlab_pat() {
        check_token(format!("glpat-{}", "a-b_0".repeat(4)), "glpat-***");
    }

    #[test]
    fn masks_google_api_key() {
        check_token(format!("AIza{}", "a_B-0".repeat(7)), "AIza***");
    }

    #[test]
    fn masks_three_part_jwt() {
        for name in [
            "keyJsonSchema.validation_rules.generated_types.ts",
            "com.x.MonkeyJavaParser.InnerClassName.CONSTANT_NAME",
        ] {
            assert!(!sensitive(name), "{name}");
            assert_eq!(redact(name), (name.to_owned(), 0));
        }
        check_token(
            format!(
                "eyJ{}.eyJ{}.{}",
                "a".repeat(10),
                "b-".repeat(5),
                "c_".repeat(5)
            ),
            "eyJ***",
        );
    }

    #[test]
    fn is_idempotent() {
        let (once, _) = redact("api_key: abcdef123456");
        let (twice, n) = redact(&once);
        assert_eq!(once, twice);
        assert_eq!(n, 0);
    }
}
