//! #245 / protocol#65：`canonical_path` 词法规范化**跨 SDK 一致性对拍**（数据向量）。
//!
//! [`fixtures/canonical_path_conformance_vectors.json`] 的唯一源是协议仓
//! `docs/specification/fixtures/canonical_path_conformance_vectors.json`（protocol#65）。本文件是
//! **vendored 逐字节副本**（`include_str!` 编译期嵌入），同步靠人手 `git show`——没有任何构建期校验；
//! 漂移由下方向量条数 / 平台分布 / `algorithm` 块完整性门禁兜底，重新同步时须整文件覆盖。
//!
//! 含义：任一 SDK 的 `normalize_path(input, platform)` MUST 对每条向量产出 `expected`，或在向量给出
//! `error` 时以 `invalid_path` 失败；Windows 形态在任意 OS 上以纯函数测试（conformance-tests §2.8）。

use smcp::utils::hash::sha256_hex;
use smcp::utils::path_norm::{normalize_path, PathPlatform};

const VECTORS: &str = include_str!("fixtures/canonical_path_conformance_vectors.json");

/// vendored 副本的 sha256（协议 develop `ce39f63`，PR#72 版本）。协议更新 vendored 文件时**有意**改此常量。
const FIXTURE_SHA256: &str = "176582498b4b09b6603a418f352eecd552e23efc44fea4b552d3576fa02d4cb7";

#[test]
fn fixture_bytes_are_pinned() {
    assert_eq!(
        sha256_hex(VECTORS.as_bytes()),
        FIXTURE_SHA256,
        "vendored fixture 漂移：从协议 develop 重新 `git show` 整文件覆盖，并有意更新本常量"
    );
}

#[test]
fn canonical_path_conformance_vectors() {
    let doc: serde_json::Value = serde_json::from_str(VECTORS).expect("valid conformance JSON");

    // 夹具完整性：algorithm 四段（守 vendored 副本未截断/未改写）。
    let algorithm = doc["algorithm"].as_object().expect("algorithm object");
    for key in ["common", "posix", "windows", "full_pipeline"] {
        assert!(algorithm.contains_key(key), "algorithm.{key} 须在");
    }

    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert_eq!(vectors.len(), 49, "49 条向量");
    let posix = vectors.iter().filter(|v| v["platform"] == "posix").count();
    let windows = vectors
        .iter()
        .filter(|v| v["platform"] == "windows")
        .count();
    assert_eq!((posix, windows), (18, 31), "POSIX 18 + Windows 31");
    let invalid = vectors
        .iter()
        .filter(|v| v["error"] == "invalid_path")
        .count();
    assert_eq!(invalid, 21, "21 条 invalid_path");

    for vector in vectors {
        let id = vector["id"].as_str().expect("id");
        let platform = match vector["platform"].as_str().expect("platform") {
            "posix" => PathPlatform::Posix,
            "windows" => PathPlatform::Windows,
            other => panic!("[{id}] unknown platform: {other}"),
        };
        let input = vector["input"].as_str().expect("input");
        match (vector.get("expected"), vector.get("error")) {
            (Some(serde_json::Value::String(expected)), None) => {
                let got = normalize_path(input, platform)
                    .unwrap_or_else(|err| panic!("[{id}] unexpected error: {err}"));
                assert_eq!(got.as_str(), expected.as_str(), "[{id}] input={input:?}");
            }
            (None, Some(serde_json::Value::String(error))) => {
                assert_eq!(error, "invalid_path", "[{id}] only invalid_path is defined");
                let got = normalize_path(input, platform);
                assert!(
                    got.is_err(),
                    "[{id}] expected invalid_path, got {got:?} input={input:?}"
                );
                assert_eq!(got.unwrap_err().reason(), "invalid_path", "[{id}]");
            }
            (expected, error) => panic!(
                "[{id}] vector must carry either expected or error, got expected={expected:?} error={error:?}"
            ),
        }
    }
}
