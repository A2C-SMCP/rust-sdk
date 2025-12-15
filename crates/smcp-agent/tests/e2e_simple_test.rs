/*!
* 文件名: e2e_simple_test
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: 简单的端到端测试 / Simple end-to-end tests
*/

#[cfg(test)]
mod e2e_tests {
    use std::env;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    #[test]
    #[ignore] // 需要手动运行，需要Python Server
    fn test_rust_agent_basic_connection() {
        // 这个测试需要手动启动Python Server
        // 1. cd examples/python
        // 2. python -m a2c_smcp.server
        // 3. 运行此测试

        let server_url = "http://127.0.0.1:8000";

        // 启动Rust Agent进程
        let mut child = Command::new("cargo")
            .args([
                "run",
                "--example",
                "e2e_test_agent",
                "--",
                "--server-url",
                server_url,
                "--agent-id",
                "test-agent",
                "--office-id",
                "test-office",
            ])
            .current_dir(env::var("CARGO_MANIFEST_DIR").unwrap())
            .spawn()
            .expect("Failed to start agent");

        // 等待5秒
        thread::sleep(Duration::from_secs(5));

        // 检查进程是否还在运行（成功连接）
        let status = child.try_wait().unwrap();
        assert!(status.is_none(), "Agent should still be running");

        // 终止进程
        child.kill().unwrap();
    }

    #[test]
    fn test_protocol_compatibility() {
        // 验证协议兼容性的单元测试
        use smcp::{EnterOfficeReq, ReqId, Role};

        // 测试ReqId序列化
        let req_id = ReqId::new();
        let json = serde_json::to_string(&req_id).unwrap();
        assert!(json.starts_with('"'));
        assert!(json.ends_with('"'));
        assert_eq!(json.len(), 34); // 32 chars + quotes

        // 测试Role序列化
        assert_eq!(serde_json::to_string(&Role::Agent).unwrap(), "\"agent\"");
        assert_eq!(
            serde_json::to_string(&Role::Computer).unwrap(),
            "\"computer\""
        );

        // 测试EnterOfficeReq结构
        let req = EnterOfficeReq {
            role: Role::Agent,
            name: "Test".to_string(),
            office_id: "Office1".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["role"], "agent");
        assert_eq!(parsed["name"], "Test");
        assert_eq!(parsed["office_id"], "Office1");
    }
}
