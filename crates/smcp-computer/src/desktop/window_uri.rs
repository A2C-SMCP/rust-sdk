/*!
* 文件名: window_uri
* 作者: JQQ
* 创建日期: 2025/12/18
* 最后修改日期: 2025/12/18
* 版权: 2023 JQQ. All rights reserved.
* 依赖: url, serde
* 描述: Window URI 解析与处理，对应 Python 侧的 window_uri.py
*/

use url::Url;

/// Window URI 解析器（v0.2 纯标识符）/ Window URI parser (v0.2 pure identifier)。
///
/// 对应 Python 侧的 `is_window_uri` 守卫 + URI 解析。v0.2.0 起 `window://host/path` 退化为
/// **纯标识符**：`priority` / `fullscreen` 等元数据已下沉至 MCP `Resource.annotations` / `_meta`
/// （见 [`crate::desktop::metadata`]），URI **不再承载 query 元数据**。遇到带 query 的旧式输入时
/// 记录 WARN 并丢弃 query（兼容降级，不报错）。
///
/// Since v0.2.0 `window://host/path` is a pure identifier: layout metadata moved to MCP
/// `Resource.annotations` / `_meta`; any URI query is logged (WARN) and dropped (compat downgrade).
#[derive(Debug, Clone)]
pub struct WindowURI {
    /// 原始 URL / Original URL
    url: Url,
    /// 缓存的路径段 / Cached path segments
    windows: Vec<String>,
}

impl WindowURI {
    /// 创建新的 WindowURI / Create new WindowURI
    ///
    /// 仅校验 `window://` scheme + 非空 host + 路径段可解码；query 段被 WARN 丢弃（不报错）。
    /// Validates only the `window://` scheme + non-empty host + decodable path; any query is
    /// warned-and-dropped (no error).
    pub fn new(uri: &str) -> Result<Self, WindowURIError> {
        let url = Url::parse(uri)
            .map_err(|e| WindowURIError::InvalidURI(format!("Failed to parse URI: {}", e)))?;

        if url.scheme() != "window" {
            return Err(WindowURIError::InvalidScheme(url.scheme().to_string()));
        }

        if url.host().is_none() || url.host_str().unwrap().is_empty() {
            return Err(WindowURIError::MissingHost);
        }

        // v0.2 纯标识符化：URI 不再承载元数据。遇到 query 段 → WARN + 丢弃（兼容降级，不报错）。
        // Pure-identifier (v0.2): URIs no longer carry metadata. Any query → WARN + drop.
        if let Some(query) = url.query() {
            if !query.is_empty() {
                tracing::warn!(
                    uri = uri,
                    query = query,
                    "window:// URI query 已弃用并被丢弃（元数据下沉至 Resource.annotations/_meta）/ \
                     window:// URI query is deprecated and dropped (metadata moved to \
                     Resource.annotations/_meta)",
                );
            }
        }

        // 解析路径段 / Parse path segments
        let windows = url
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                percent_encoding::percent_decode_str(s)
                    .decode_utf8()
                    .map(|s| s.to_string())
                    .map_err(|e| {
                        WindowURIError::InvalidPath(format!("Failed to decode path segment: {}", e))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { url, windows })
    }

    /// 获取 MCP ID (host) / Get MCP ID (host)
    pub fn mcp_id(&self) -> &str {
        self.url.host_str().unwrap()
    }

    /// 获取窗口路径列表 / Get window path list
    pub fn windows(&self) -> &[String] {
        &self.windows
    }

    /// 构建 Window URI（纯标识符：host + path，无 query）/ Build a Window URI (pure
    /// identifier: host + path, no query)。
    ///
    /// v0.2 起不再拼接 `priority` / `fullscreen` query；元数据由生产方写入
    /// `Resource.annotations` / `_meta`。Since v0.2 no query metadata is appended.
    pub fn build(host: &str, windows: &[String]) -> Result<String, WindowURIError> {
        if host.is_empty() {
            return Err(WindowURIError::MissingHost);
        }

        let mut url = Url::parse(&format!("window://{}", host))
            .map_err(|e| WindowURIError::InvalidURI(format!("Failed to build URI: {}", e)))?;

        // 添加路径段 / Add path segments
        if !windows.is_empty() {
            let encoded_path: Vec<String> = windows
                .iter()
                .map(|w| {
                    percent_encoding::utf8_percent_encode(w, percent_encoding::NON_ALPHANUMERIC)
                        .to_string()
                })
                .collect();
            url.set_path(&encoded_path.join("/"));
        }

        Ok(url.to_string())
    }
}

impl std::fmt::Display for WindowURI {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.url)
    }
}

/// Window URI 错误 / Window URI errors
#[derive(Debug, thiserror::Error)]
pub enum WindowURIError {
    #[error("Invalid URI: {0}")]
    InvalidURI(String),

    #[error("Invalid scheme: {0}, expected 'window'")]
    InvalidScheme(String),

    #[error("Missing host (MCP ID)")]
    MissingHost,

    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

/// 检查是否为有效的 Window URI / Check if URI is a valid Window URI
pub fn is_window_uri(uri: &str) -> bool {
    WindowURI::new(uri).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal() {
        let uri = WindowURI::new("window://com.example.mcp").unwrap();
        assert_eq!(uri.mcp_id(), "com.example.mcp");
        assert!(uri.windows().is_empty());
    }

    #[test]
    fn test_parse_with_paths() {
        let uri = WindowURI::new("window://com.example.mcp/dashboard/main").unwrap();
        assert_eq!(uri.mcp_id(), "com.example.mcp");
        assert_eq!(uri.windows(), &["dashboard", "main"]);
    }

    #[test]
    fn test_query_is_dropped_not_errored() {
        // v0.2 纯标识符化：带 query 的旧式输入仍解析成功（host/path 保留），query 被丢弃（WARN）。
        // 元数据已下沉至 Resource.annotations/_meta，URI 不再承载 priority/fullscreen。
        let uri =
            WindowURI::new("window://com.example.mcp/page?priority=90&fullscreen=true").unwrap();
        assert_eq!(uri.mcp_id(), "com.example.mcp");
        assert_eq!(uri.windows(), &["page"]);
    }

    #[test]
    fn test_out_of_range_query_no_longer_errors() {
        // 旧版越界 priority 会报错；v0.2 query 被无条件丢弃，故一律 Ok。
        assert!(WindowURI::new("window://x?priority=999").is_ok());
        assert!(WindowURI::new("window://x?priority=-1").is_ok());
        assert!(WindowURI::new("window://x?fullscreen=maybe").is_ok());
    }

    #[test]
    fn test_build_uri_is_pure_identifier() {
        let uri = WindowURI::build(
            "com.example.mcp",
            &["dashboard".to_string(), "main".to_string()],
        )
        .unwrap();

        assert_eq!(uri, "window://com.example.mcp/dashboard/main");
        // 不再拼接 query 元数据 / no query metadata appended
        assert!(!uri.contains('?'));
        assert!(!uri.contains("priority"));
        assert!(!uri.contains("fullscreen"));
    }

    #[test]
    fn test_invalid_uri_is_rejected() {
        assert!(WindowURI::new("http://example.com").is_err()); // 错误 scheme
        assert!(WindowURI::new("window://").is_err()); // 缺 host
        assert!(WindowURI::new(":::not_a_uri").is_err()); // 非法 URI
    }

    #[test]
    fn test_is_window_uri() {
        assert!(is_window_uri("window://com.example.mcp"));
        // 带 query 的旧式输入仍视为合法 window URI（query 丢弃）
        assert!(is_window_uri("window://host/path?priority=50"));
        assert!(!is_window_uri("http://example.com"));
        assert!(!is_window_uri("window://"));
    }
}
