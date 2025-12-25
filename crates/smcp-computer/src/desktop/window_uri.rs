/*!
* 文件名: window_uri
* 作者: JQQ
* 创建日期: 2025/12/18
* 最后修改日期: 2025/12/18
* 版权: 2023 JQQ. All rights reserved.
* 依赖: url, serde
* 描述: Window URI 解析与处理，对应 Python 侧的 window_uri.py
*/

use std::collections::HashMap;
use url::Url;

/// Window URI 解析器 / Window URI parser
/// 对应 Python 侧的 WindowURI 类
#[derive(Debug, Clone)]
pub struct WindowURI {
    /// 原始 URL / Original URL
    url: Url,
    /// 缓存的路径段 / Cached path segments
    windows: Vec<String>,
    /// 缓存的查询参数 / Cached query parameters
    params: HashMap<String, String>,
}

impl WindowURI {
    /// 创建新的 WindowURI / Create new WindowURI
    pub fn new(uri: &str) -> Result<Self, WindowURIError> {
        let url = Url::parse(uri)
            .map_err(|e| WindowURIError::InvalidURI(format!("Failed to parse URI: {}", e)))?;

        if url.scheme() != "window" {
            return Err(WindowURIError::InvalidScheme(url.scheme().to_string()));
        }

        if url.host().is_none() || url.host_str().unwrap().is_empty() {
            return Err(WindowURIError::MissingHost);
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

        // 解析查询参数 / Parse query parameters
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();

        // 验证查询参数 / Validate query parameters
        let uri = Self {
            url,
            windows,
            params,
        };

        // 验证 priority / Validate priority
        if let Some(priority) = uri.priority() {
            if !(0..=100).contains(&priority) {
                return Err(WindowURIError::InvalidPriority(priority));
            }
        }

        // 验证 fullscreen / Validate fullscreen
        let _ = uri.fullscreen(); // Will error if invalid

        Ok(uri)
    }

    /// 获取 MCP ID (host) / Get MCP ID (host)
    pub fn mcp_id(&self) -> &str {
        self.url.host_str().unwrap()
    }

    /// 获取窗口路径列表 / Get window path list
    pub fn windows(&self) -> &[String] {
        &self.windows
    }

    /// 获取优先级 / Get priority (0-100)
    pub fn priority(&self) -> Option<i32> {
        self.params.get("priority").and_then(|s| s.parse().ok())
    }

    /// 获取全屏标志 / Get fullscreen flag
    pub fn fullscreen(&self) -> Option<bool> {
        self.params
            .get("fullscreen")
            .and_then(|s| match s.to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            })
    }

    /// 构建 Window URI / Build Window URI
    pub fn build(
        host: &str,
        windows: &[String],
        priority: Option<i32>,
        fullscreen: Option<bool>,
    ) -> Result<String, WindowURIError> {
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

        // 添加查询参数 / Add query parameters
        let mut query_pairs = Vec::new();

        if let Some(p) = priority {
            if !(0..=100).contains(&p) {
                return Err(WindowURIError::InvalidPriority(p));
            }
            query_pairs.push(("priority", p.to_string()));
        }

        if let Some(f) = fullscreen {
            query_pairs.push(("fullscreen", if f { "true" } else { "false" }.to_string()));
        }

        if !query_pairs.is_empty() {
            url.set_query(Some(
                &query_pairs
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join("&"),
            ));
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

    #[error("Invalid priority: {0}, must be between 0 and 100")]
    InvalidPriority(i32),

    #[error("Invalid fullscreen value")]
    InvalidFullscreen,
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
        assert_eq!(uri.priority(), None);
        assert_eq!(uri.fullscreen(), None);
    }

    #[test]
    fn test_parse_with_paths() {
        let uri = WindowURI::new("window://com.example.mcp/dashboard/main").unwrap();
        assert_eq!(uri.mcp_id(), "com.example.mcp");
        assert_eq!(uri.windows(), &["dashboard", "main"]);
    }

    #[test]
    fn test_parse_with_query_params() {
        let uri =
            WindowURI::new("window://com.example.mcp/page?priority=90&fullscreen=true").unwrap();
        assert_eq!(uri.windows(), &["page"]);
        assert_eq!(uri.priority(), Some(90));
        assert_eq!(uri.fullscreen(), Some(true));
    }

    #[test]
    fn test_priority_bounds() {
        assert!(WindowURI::new("window://x?priority=0").is_ok());
        assert!(WindowURI::new("window://x?priority=100").is_ok());
        assert!(WindowURI::new("window://x?priority=-1").is_err());
        assert!(WindowURI::new("window://x?priority=101").is_err());
    }

    #[test]
    fn test_fullscreen_variants() {
        let test_cases = vec![
            ("true", true),
            ("1", true),
            ("yes", true),
            ("on", true),
            ("false", false),
            ("0", false),
            ("no", false),
            ("off", false),
        ];

        for (val, expected) in test_cases {
            let uri = WindowURI::new(&format!("window://x?fullscreen={}", val)).unwrap();
            assert_eq!(uri.fullscreen(), Some(expected));
        }
    }

    #[test]
    fn test_build_uri() {
        let uri = WindowURI::build(
            "com.example.mcp",
            &["dashboard".to_string(), "main".to_string()],
            Some(80),
            Some(false),
        )
        .unwrap();

        assert!(uri.starts_with("window://com.example.mcp/dashboard/main"));
        assert!(uri.contains("priority=80"));
        assert!(uri.contains("fullscreen=false"));
    }

    #[test]
    fn test_is_window_uri() {
        assert!(is_window_uri("window://com.example.mcp"));
        assert!(is_window_uri("window://host/path?priority=50"));
        assert!(!is_window_uri("http://example.com"));
        assert!(!is_window_uri("window://"));
    }
}
