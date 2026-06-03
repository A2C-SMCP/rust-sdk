/*!
* 文件名: response.rs
* 作者: JQQ
* 创建日期: 2026/06/02
* 最后修改日期: 2026/06/02
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json
* 描述: Agent 响应校验共享 helper / Shared response-validation helpers
*/

//! Agent 端响应校验共享 helper（与 `request_builders` 对称：一个构造请求、一个校验响应）。
//!
//! 当前承载 `client:*` ack 的 `req_id` 回显校验——`get_tools` / `get_desktop` / `list_room` /
//! SKILL 消费（`skill_consume`）均经此单点收敛，避免「错误处理逻辑重复」（DRY）。

use serde_json::Value;

use crate::error::{Result, SmcpAgentError};

/// 校验响应回显的 `req_id` 与请求一致 / validate the echoed `req_id` matches the request。
///
/// 缺失 → [`SmcpAgentError::internal`]；不一致 → [`SmcpAgentError::ReqIdMismatch`]。语义对标
/// Python 各消费方法的 `if response.get("req_id") != req["req_id"]: raise`，为全 crate 单一真源。
pub(crate) fn ensure_req_id(response: &Value, expected: &str) -> Result<()> {
    let actual = response
        .get("req_id")
        .and_then(Value::as_str)
        .ok_or_else(|| SmcpAgentError::internal("Missing req_id in response"))?;
    if actual != expected {
        return Err(SmcpAgentError::ReqIdMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_ensure_req_id_ok() {
        assert!(ensure_req_id(&json!({ "req_id": "R1", "x": 1 }), "R1").is_ok());
    }

    #[test]
    fn test_ensure_req_id_missing_is_internal_error() {
        let err = ensure_req_id(&json!({ "x": 1 }), "R1").unwrap_err();
        assert!(matches!(err, SmcpAgentError::Internal(_)), "got {err:?}");
    }

    #[test]
    fn test_ensure_req_id_mismatch() {
        let err = ensure_req_id(&json!({ "req_id": "OTHER" }), "R1").unwrap_err();
        match err {
            SmcpAgentError::ReqIdMismatch { expected, actual } => {
                assert_eq!(expected, "R1");
                assert_eq!(actual, "OTHER");
            }
            other => panic!("expected ReqIdMismatch, got {other:?}"),
        }
    }
}
