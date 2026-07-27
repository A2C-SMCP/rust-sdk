/*!
* 文件名: redaction.rs
* 作者: JQQ
* 创建日期: 2026/07/23
* 最后修改日期: 2026/07/23
* 版权: 2023 JQQ. All rights reserved.
* 依赖: url
* 描述: settings / Marketplace 公开边界共用的 URL 凭据脱敏。
*       Shared URL credential redaction for settings and Marketplace public boundaries.
*/

//! URL 凭据脱敏纯函数 / Pure URL credential-redaction helpers。
//!
//! URL 的 userinfo、query、fragment 都可能承载凭据。公开错误不得保存这些原始片段，因为派生
//! `Debug` / `Display` 会把枚举字段直接带入日志；配置导出则只替换已确认的 userinfo 面，保留其余 URL。

use url::Url;

/// 无法安全呈现的 Git 来源占位符 / placeholder for a Git source that cannot be shown safely.
pub(crate) const REDACTED_GIT_SOURCE: &str = "<redacted-git-source>";

/// 若 `url` 的 authority 段含非空 userinfo，返回以 `replacement` 替换 userinfo 后的新串。
///
/// 只识别 `scheme://authority` 形态；authority 终止于首个 `/`、`?` 或 `#`。调用方决定替换哨兵，
/// 从而让配置导出保留显式 `${REDACTED}`，错误边界使用无凭据的安全形态。
pub(crate) fn url_with_redacted_userinfo(url: &str, replacement: &str) -> Option<String> {
    let after_scheme = url.find("://")? + 3;
    let authority_end = url[after_scheme..]
        .find(['/', '?', '#'])
        .map(|i| after_scheme + i)
        .unwrap_or(url.len());
    let at_rel = url[after_scheme..authority_end].rfind('@')?;
    let at = after_scheme + at_rel;
    if at <= after_scheme {
        return None;
    }
    let suffix_start = if replacement.is_empty() { at + 1 } else { at };
    Some(format!(
        "{}{}{}",
        &url[..after_scheme],
        replacement,
        &url[suffix_start..]
    ))
}

/// 把任意 Git URL 规整为可进入 CLI / 日志 / 公开错误的安全展示值。
///
/// 仅在 [`Url::parse`] 严格成功后保留 scheme/host/path；完整 userinfo、query、fragment 一律移除。
/// scp-like 或畸形输入无法可靠划分 authority，故完全隐藏。
pub(crate) fn git_url_for_display(raw: &str) -> String {
    let Ok(mut parsed) = Url::parse(raw.trim()) else {
        return REDACTED_GIT_SOURCE.to_string();
    };
    if !matches!(parsed.scheme(), "ssh" | "git" | "http" | "https" | "file") {
        return REDACTED_GIT_SOURCE.to_string();
    }

    if !parsed.username().is_empty() && parsed.set_username("").is_err() {
        return REDACTED_GIT_SOURCE.to_string();
    }
    if parsed.password().is_some() && parsed.set_password(None).is_err() {
        return REDACTED_GIT_SOURCE.to_string();
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

/// 非法 Git 来源的公开错误载荷与 CLI 展示共用同一严格安全边界。
pub(crate) fn git_source_for_error(raw: &str) -> String {
    git_url_for_display(raw)
}

fn source_end_boundary(ch: char) -> bool {
    ch.is_whitespace()
}

fn wrapper_suffix_boundary(ch: char) -> bool {
    matches!(
        ch,
        '\'' | '"' | '`' | ':' | ';' | ',' | '.' | '!' | '?' | ')' | ']' | '}'
    )
}

/// 若 scheme 前紧邻引号包装符，只把**最后一个**且其后全为外层标点的同类引号视为闭合符。
///
/// URL userinfo/query 自身允许单引号；按第一个引号截断会泄露后半凭据。无法证明是外层闭合符时，
/// 整个非空白 token 都留给 URL sanitizer fail-closed 处理。
fn split_outer_wrapper<'a>(prefix: &str, candidate: &'a str) -> (&'a str, &'a str) {
    let Some(wrapper) = prefix
        .chars()
        .next_back()
        .filter(|ch| matches!(ch, '\'' | '"' | '`'))
    else {
        return (candidate, "");
    };
    let Some((close, _)) = candidate.char_indices().rev().find(|(index, ch)| {
        *ch == wrapper && candidate[*index..].chars().all(wrapper_suffix_boundary)
    }) else {
        return (candidate, "");
    };
    (&candidate[..close], &candidate[close..])
}

fn redact_scp_like_sources(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let start = rest[..at].rfind(source_end_boundary).map_or(0, |index| {
            index + rest[index..].chars().next().map_or(0, char::len_utf8)
        });

        let token_end = rest[at + 1..]
            .find(source_end_boundary)
            .map_or(rest.len(), |index| at + 1 + index);
        let userinfo = &rest[start..at];
        let destination = &rest[at + 1..token_end];
        let colon_rel = destination.find(':');
        let ssh_possessive = destination.contains("'s");
        if start == at
            || destination.is_empty()
            || colon_rel == Some(0)
            || (colon_rel.is_none() && !userinfo.contains(':') && !ssh_possessive)
        {
            let consumed = at + 1;
            out.push_str(&rest[..consumed]);
            rest = &rest[consumed..];
            continue;
        }

        let path_end = token_end;
        if start < at {
            out.push_str(&rest[..start]);
            out.push_str(REDACTED_GIT_SOURCE);
            rest = &rest[path_end..];
        } else {
            let consumed = at + 1;
            out.push_str(&rest[..consumed]);
            rest = &rest[consumed..];
        }
    }
    out.push_str(rest);
    out
}

fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
}

/// 脱敏散文中的完整 Git source token；用于 Git stderr、公开错误等上游诊断文本。
///
/// 任何 `scheme://` 形态都按不可信来源处理：受支持且可解析的 scheme 仅保留
/// scheme/host/path，畸形或不受支持的 scheme 完全隐藏。scp-like source 无法可靠拆分凭据，
/// 同样完全隐藏。
pub(crate) fn redact_git_urls_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(marker) = rest.find("://") {
        let mut scheme_start = marker;
        while scheme_start > 0 && is_scheme_byte(rest.as_bytes()[scheme_start - 1]) {
            scheme_start -= 1;
        }
        while scheme_start < marker && !rest.as_bytes()[scheme_start].is_ascii_alphabetic() {
            scheme_start += 1;
        }
        let scheme = &rest[scheme_start..marker];
        if scheme.is_empty() || !scheme.bytes().all(is_scheme_byte) {
            let consumed = marker + 3;
            out.push_str(&rest[..consumed]);
            rest = &rest[consumed..];
            continue;
        }

        out.push_str(&rest[..scheme_start]);
        let token = &rest[scheme_start..];
        let end = token.find(source_end_boundary).unwrap_or(token.len());
        let candidate = &token[..end];
        let (candidate, wrapper_suffix) = split_outer_wrapper(&rest[..scheme_start], candidate);
        let first_marker_end = marker - scheme_start + 3;
        if candidate[first_marker_end..].contains("://") {
            out.push_str(REDACTED_GIT_SOURCE);
        } else {
            out.push_str(&git_url_for_display(candidate));
        }
        out.push_str(wrapper_suffix);
        rest = &token[end..];
    }
    out.push_str(rest);
    redact_scp_like_sources(&out)
}

/// 未验证的 marketplace name/target 的公开展示边界。
///
/// 合法 kebab 名不含 URL 结构，调用本函数后逐字保持；恶意或畸形输入若嵌入 Git source，则按与公开错误
/// 相同的 fail-closed 规则脱敏。
pub(crate) fn untrusted_name_for_display(raw: &str) -> String {
    redact_git_urls_in_text(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_complete_userinfo_and_preserves_destination() {
        assert_eq!(
            url_with_redacted_userinfo(
                "https://alice:s3cr3t@[::1]:8080/org/repo.git",
                "${REDACTED}"
            ),
            Some("https://${REDACTED}@[::1]:8080/org/repo.git".to_string())
        );
    }

    #[test]
    fn public_error_source_drops_credentials_query_and_fragment() {
        let got = git_url_for_display(
            "https://cnb:FAKE_TOKEN@example.com/org/repo.git?token=QUERY#token=FRAGMENT",
        );
        assert_eq!(got, "https://example.com/org/repo.git");
        for secret in ["cnb", "FAKE_TOKEN", "QUERY", "FRAGMENT"] {
            assert!(!got.contains(secret));
        }
    }

    #[test]
    fn opaque_invalid_source_is_not_echoed() {
        for source in [
            "cnb:FAKE_TOKEN@example.com/org/repo.git",
            "cnb:FAKE_TOKEN@https://example.com/org/repo.git",
            "ftp://cnb:FAKE_TOKEN",
        ] {
            assert_eq!(git_url_for_display(source), REDACTED_GIT_SOURCE);
        }
    }

    #[test]
    fn redacts_git_url_embedded_in_diagnostic_text() {
        let got = redact_git_urls_in_text(
            "fatal: unable to access 'https://cnb:FAKE_TOKEN@example.com/repo.git?token=QUERY#FRAGMENT/': failed",
        );
        assert_eq!(
            got,
            "fatal: unable to access 'https://example.com/repo.git': failed"
        );
    }

    #[test]
    fn hides_unsupported_malformed_and_scp_like_sources_in_text() {
        for text in [
            "bad ftp://cnb:FAKE_TOKEN@example.com/repo.git source",
            "bad https://cnb:FAKE_TOKEN source",
            "bad git@example.com:org/repo.git source",
        ] {
            let got = redact_git_urls_in_text(text);
            assert!(got.contains(REDACTED_GIT_SOURCE), "{got}");
            for secret in ["cnb", "FAKE_TOKEN", "git@example.com"] {
                assert!(!got.contains(secret), "{got}");
            }
        }
    }

    #[test]
    fn finds_sources_after_arbitrary_prefixes_and_multiple_delimiters() {
        let got = redact_git_urls_in_text(
            "key=https://user:PW_ONE@example.com/a.git;\
             prefix:https://user:PW_TWO@example.com/b.git|\
             path/ssh://user:PW_THREE@example.com/c.git;\
             source=git@example.com:org/repo.git",
        );
        for secret in ["user", "PW_ONE", "PW_TWO", "PW_THREE", "git@example.com"] {
            assert!(!got.contains(secret), "{got}");
        }
        assert!(got.contains(REDACTED_GIT_SOURCE), "{got}");
    }

    #[test]
    fn nested_urls_and_unicode_scp_like_are_hidden_fail_closed() {
        for text in [
            "x=https://public.example/a=https://user2:PW_TWO@secret.example/repo.git",
            "key=用户@example.com:org/repo.git",
            "key=alice@my_host:org/repo.git",
            "key=用户@例子.公司:org/repo.git",
            "ssh: alice@my_host: Permission denied (publickey).",
            "ssh: 用户@例子.公司: Permission denied (publickey).",
            "ssh: alice@my_host's password:",
            "ssh: 用户@例子.公司's password:",
            "path=/tmp/x=https:/alice:PW_PATH@example.com/repo.git missing",
            "x=https://example.com/r.git?token=;QUERY_SECRET",
            "x=https://example.com/r.git?token=,COMMA_SECRET",
            "x=https://example.com/r.git?token=QUERY_QUOTE'LEAK_SECRET",
            "x=https://alice:PW_ONE'PW_TWO@example.com/repo.git",
        ] {
            let got = redact_git_urls_in_text(text);
            for secret in [
                "user2",
                "PW_TWO",
                "用户",
                "alice",
                "PW_PATH",
                "QUERY_SECRET",
                "COMMA_SECRET",
                "QUERY_QUOTE",
                "LEAK_SECRET",
                "PW_ONE",
                "PW_TWO",
            ] {
                assert!(!got.contains(secret), "{got}");
            }
        }
    }

    #[test]
    fn ordinary_marketplace_name_is_preserved_for_display() {
        assert_eq!(
            untrusted_name_for_display("lowercase-kebab"),
            "lowercase-kebab"
        );
    }
}
