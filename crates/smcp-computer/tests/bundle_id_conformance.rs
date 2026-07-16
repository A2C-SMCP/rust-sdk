//! BundleID 缺省生成一致性测试向量断言（协议 0.3.0，rust-sdk#117）。
//!
//! 断言 `resolve_bundle_id(name, config)` 对每条跨 SDK 向量产出 `expected_bundle_id`。向量文件
//! [`fixtures/bundle_id_conformance_vectors.json`] 是**跨语言契约**——python-sdk 对拍同一份文件（随协议落库）。
//! 向量以**扁平连接身份形式**表达（`type`/`command`/`args`/`env` 或 `url`/`headers`），与各 SDK 的 serde 形态解耦。

use std::collections::HashMap;

use smcp_computer::mcp_clients::bundle_id::{resolve_bundle_id, BundleId};
use smcp_computer::mcp_clients::model::{
    HttpServerConfig, HttpServerParameters, MCPServerConfig, SseServerConfig, SseServerParameters,
    StdioServerConfig, StdioServerParameters,
};

const VECTORS: &str = include_str!("fixtures/bundle_id_conformance_vectors.json");

/// JSON `{k: v}`（保证字符串值）→ `HashMap<String,String>`。
fn json_string_map(v: &serde_json::Value) -> HashMap<String, String> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, val)| (k.clone(), val.as_str().expect("string value").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn json_string_list(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|s| s.as_str().expect("string arg").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// 从扁平向量形式构造 `MCPServerConfig`（经公开构造器；显式 `bundle_id` 直填字段）。
fn build_config(name: &str, cfg: &serde_json::Value) -> MCPServerConfig {
    let ty = cfg["type"].as_str().expect("type");
    // #130：向量里的显式 bundle_id 经 `BundleId` 构造校验——夹具取值非法即在此 panic（跨 SDK 向量
    // 本就只应含合法值；若未来新增"非法值"向量，须改走 `try_from` 的 Err 分支断言，而非静默成 String）。
    let explicit_bundle_id = cfg
        .get("bundle_id")
        .and_then(|v| v.as_str())
        .map(|v| BundleId::try_from(v).expect("向量中的显式 bundle_id 必须合法"));

    match ty {
        "stdio" => {
            let mut c = StdioServerConfig::new(
                name,
                StdioServerParameters {
                    command: cfg["command"].as_str().expect("command").to_string(),
                    args: json_string_list(&cfg["args"]),
                    env: json_string_map(&cfg["env"]),
                    cwd: None,
                },
            );
            c.bundle_id = explicit_bundle_id;
            MCPServerConfig::Stdio(c)
        }
        "streamable" | "http" => {
            let mut c = HttpServerConfig::new(
                name,
                HttpServerParameters {
                    url: cfg["url"].as_str().expect("url").to_string(),
                    headers: json_string_map(&cfg["headers"]),
                },
            );
            c.bundle_id = explicit_bundle_id;
            MCPServerConfig::Http(c)
        }
        "sse" => {
            let mut c = SseServerConfig::new(
                name,
                SseServerParameters {
                    url: cfg["url"].as_str().expect("url").to_string(),
                    headers: json_string_map(&cfg["headers"]),
                },
            );
            c.bundle_id = explicit_bundle_id;
            MCPServerConfig::Sse(c)
        }
        other => panic!("unknown transport type in vector: {other}"),
    }
}

#[test]
fn bundle_id_conformance_vectors_pass() {
    let doc: serde_json::Value = serde_json::from_str(VECTORS).expect("valid vectors JSON");
    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert!(
        vectors.len() >= 16,
        "向量应覆盖全部分叉点（含 raw 占位 2 条），实得 {}",
        vectors.len()
    );

    let mut checked = 0usize;
    for v in vectors {
        let name = v["name"].as_str().expect("name");
        let expected = v["expected_bundle_id"].as_str().expect("expected");
        let desc = v.get("desc").and_then(|d| d.as_str()).unwrap_or("");
        let config = build_config(name, &v["config"]);

        let got = resolve_bundle_id(&config);
        assert_eq!(
            got, expected,
            "conformance 向量失配 [{desc}] name={name:?}: 期望 {expected}, 实得 {got}"
        );
        checked += 1;
    }
    assert_eq!(checked, vectors.len());
}
