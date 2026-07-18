//! #140：input 环境变量命名（ENV_SEGMENT）**跨 SDK 一致性对拍**（P0 硬门槛）/ ENV_SEGMENT conformance.
//!
//! [`fixtures/env_segment_conformance_vectors.json`] 是**跨语言契约**——python-sdk 对拍同一份文件
//! （python 首版提供，rust 逐字节 vendored；协议仓收口后改为 vendored 副本，同 bundle_id 方式同步）。
//!
//! 含义：任一 SDK 的 `env_var_name(input_id, bundle_id, tool_name)` MUST 对每条向量产出
//! `expected_env_var_name` 方为合规（PROTO-5 / Discussion #23 F4-F5）。

use smcp::utils::env_segment::env_var_name;

const VECTORS: &str = include_str!("fixtures/env_segment_conformance_vectors.json");

#[test]
fn env_segment_conformance_vectors_byte_identical() {
    let doc: serde_json::Value = serde_json::from_str(VECTORS).expect("valid conformance JSON");
    // 夹具完整性：16 条向量 + algorithm 规范性声明（守 vendored 副本未截断）。
    let algo = &doc["algorithm"];
    assert!(
        algo["prefix"]
            .as_str()
            .unwrap_or_default()
            .contains("A2C_SMCP_"),
        "algorithm.prefix 须声明 A2C_SMCP_"
    );
    assert!(
        algo.get("no_folding").is_some(),
        "algorithm.no_folding 须在"
    );
    assert!(algo.get("collision").is_some(), "algorithm.collision 须在");
    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert_eq!(vectors.len(), 16, "16 条向量");

    for v in vectors {
        let id = v["id"].as_str().unwrap();
        let input_id = v["input_id"].as_str().unwrap();
        let bundle_id = v["bundle_id"].as_str();
        let tool_name = v["tool_name"].as_str();
        let expected = v["expected_env_var_name"].as_str().unwrap();
        let got = env_var_name(input_id, bundle_id, tool_name);
        assert_eq!(
            got,
            expected,
            "[{id}] {}: got={got} expected={expected}",
            v["desc"].as_str().unwrap_or_default()
        );
    }
}

/// 对照对显式断言：MyServer / myserver 两个合法共存 bundle_id **不**坍缩（#155 验收 ③）。
#[test]
fn case_preserved_pair_does_not_collapse() {
    assert_ne!(
        env_var_name("token", Some("MyServer"), None),
        env_var_name("token", Some("myserver"), None),
        "保留大小写失效：MyServer/myserver 坍缩"
    );
}
