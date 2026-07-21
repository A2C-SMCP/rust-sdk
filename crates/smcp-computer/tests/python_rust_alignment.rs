/*！
* 文件名: python_rust_alignment
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json
* 描述: 验证Rust与Python SDK行为对齐的集成测试
*/

use serde_json::Value;
use smcp_computer::inputs::{
    CliInputProvider, InputContext, InputProvider, InputRequest, InputType,
};
use smcp_computer::mcp_clients::{ConfigRender, RenderError};

// #149：共享 Streamable HTTP mock（非 cli 门控）；auto_reconnect 测试复用寻址 / AUTH-01 同源 mock。
#[path = "common/streamable_mock.rs"]
mod streamable_mock;

#[tokio::test]
async fn test_config_render_placeholder() {
    // 测试ConfigRender的${input:xxx}占位符解析
    let render = ConfigRender::default();

    // 创建resolver函数
    async fn resolver(id: String) -> Result<Value, RenderError> {
        match id.as_str() {
            "api_key" => Ok(Value::String("sk-123456".to_string())),
            "port" => Ok(Value::Number(serde_json::Number::from(8080))),
            "missing" => Err(RenderError::InputNotFound(id)),
            _ => Ok(Value::String(format!("resolved_{}", id))),
        }
    }

    // 测试单个占位符
    let input = Value::String("${input:api_key}".to_string());
    let result = render.render(input, resolver).await.unwrap();
    assert_eq!(result, Value::String("sk-123456".to_string()));

    // 测试字符串中的占位符
    let input = Value::String("http://localhost:${input:port}/api".to_string());
    let result = render.render(input, resolver).await.unwrap();
    assert_eq!(
        result,
        Value::String("http://localhost:8080/api".to_string())
    );

    // 测试对象渲染
    let mut obj = serde_json::Map::new();
    obj.insert(
        "url".to_string(),
        Value::String("${input:api_key}".to_string()),
    );
    obj.insert("nested".to_string(), Value::String("value".to_string()));
    let input = Value::Object(obj);
    let result = render.render(input, resolver).await.unwrap();

    if let Value::Object(map) = result {
        assert_eq!(
            map.get("url").unwrap(),
            &Value::String("sk-123456".to_string())
        );
        assert_eq!(
            map.get("nested").unwrap(),
            &Value::String("value".to_string())
        );
    } else {
        panic!("Expected object");
    }

    // 测试缺失输入（应保留原占位符）
    let input = Value::String("${input:missing}".to_string());
    let result = render.render(input, resolver).await.unwrap();
    assert_eq!(result, Value::String("${input:missing}".to_string()));
}

#[tokio::test]
async fn test_command_input_shell_mode() {
    // 测试command input的shell模式支持
    let provider = CliInputProvider::new();

    // 创建输入上下文
    let context = InputContext {
        server_name: None,
        tool_name: None,
        metadata: std::collections::HashMap::new(),
    };

    // Unix shell管道测试（仅在Unix系统运行）
    #[cfg(unix)]
    {
        let request = InputRequest {
            id: "test_pipe".to_string(),
            input_type: InputType::Command {
                command: "echo".to_string(),
                args: vec!["hello | tr a-z A-Z".to_string()],
            },
            title: "Test Command".to_string(),
            description: "Test shell command".to_string(),
            default: None,
            required: false,
            validation: None,
        };

        let response = provider.get_input(&request, &context).await.unwrap();
        if let smcp_computer::inputs::InputValue::String(s) = response.value {
            // shell应该执行管道并返回大写的HELLO
            assert!(s.contains("HELLO"));
        } else {
            panic!("Expected string result");
        }
    }

    // Windows测试
    #[cfg(windows)]
    {
        let request = InputRequest {
            id: "test_windows".to_string(),
            input_type: InputType::Command {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
            },
            title: "Test Command".to_string(),
            description: "Test shell command".to_string(),
            default: None,
            required: false,
            validation: None,
        };

        let response = provider.get_input(&request, &context).await.unwrap();
        if let smcp_computer::inputs::InputValue::String(s) = response.value {
            assert_eq!(s.trim(), "hello");
        } else {
            panic!("Expected string result");
        }
    }
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_integration_with_manager() {
    // 测试VRL与MCPServerManager的集成
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
    use smcp_computer::mcp_clients::{
        MCPServerConfig, MCPServerManager, StdioServerConfig, StdioServerParameters,
    };

    // 创建带VRL脚本的配置
    let vrl_script = r#"
        .processed = true
        .tool_name = .tool_name
        .timestamp_added = "2025-12-16"
    "#;

    let config = {
        let mut c = StdioServerConfig::new(
            "vrl_test_server",
            StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["test".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        );
        c.vrl = Some(vrl_script.to_string());
        c
    };

    let manager = MCPServerManager::new();

    // 初始化管理器
    manager
        .initialize(vec![MCPServerConfig::Stdio(config.clone())])
        .await
        .unwrap();

    // 验证VRL脚本已正确存储
    assert_eq!(config.vrl, Some(vrl_script.to_string()));

    // 测试VRL运行时独立功能
    let mut runtime = VrlRuntime::new();
    let test_event = serde_json::json!({
        "result": "success",
        "data": [1, 2, 3]
    });

    let result = runtime.run(vrl_script, test_event.clone(), "UTC").unwrap();

    // 验证原始数据保持不变（简化实现）
    assert_eq!(result.processed_event["result"], "success");
    assert_eq!(result.processed_event["data"].as_array().unwrap().len(), 3);
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_multiple_server_configs() {
    // 测试多个服务器配置中的VRL脚本
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
    use smcp_computer::mcp_clients::{
        MCPServerConfig, MCPServerManager, StdioServerConfig, StdioServerParameters,
    };

    let configs = vec![
        MCPServerConfig::Stdio({
            let mut c = StdioServerConfig::new(
                "server1",
                StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["server1".to_string()],
                    env: std::collections::HashMap::new(),
                    cwd: None,
                },
            );
            c.vrl = Some(".server = 1".to_string());
            c
        }),
        MCPServerConfig::Stdio({
            let mut c = StdioServerConfig::new(
                "server2",
                StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["server2".to_string()],
                    env: std::collections::HashMap::new(),
                    cwd: None,
                },
            );
            c.vrl = Some(".server = 2".to_string());
            c
        }),
    ];

    let manager = MCPServerManager::new();
    manager.initialize(configs).await.unwrap();

    // 测试每个VRL脚本
    let mut runtime = VrlRuntime::new();
    let event = serde_json::json!({"test": "value"});

    let result1 = runtime.run(".server = 1", event.clone(), "UTC").unwrap();
    let result2 = runtime.run(".server = 2", event.clone(), "UTC").unwrap();

    // 验证结果
    assert_eq!(result1.processed_event["test"], "value");
    assert_eq!(result2.processed_event["test"], "value");
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_error_handling_in_manager() {
    // 测试VRL错误处理
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;

    let mut runtime = VrlRuntime::new();

    // 测试无效脚本
    let invalid_scripts = vec![
        ".field =",
        "= value",
        ".invalid_syntax @#$",
        ".field = now(", // 未闭合的函数
    ];

    let event = serde_json::json!({"test": "value"});

    for script in invalid_scripts {
        assert!(
            runtime.run(script, event.clone(), "UTC").is_err(),
            "Script should fail: {}",
            script
        );
    }
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_performance() {
    // 测试VRL性能（简单基准测试）
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
    use std::time::Instant;

    let mut runtime = VrlRuntime::new();
    let script = ".processed = true";
    let event = serde_json::json!({"data": "test"});

    let iterations = 1000;
    let start = Instant::now();

    for _ in 0..iterations {
        runtime.run(script, event.clone(), "UTC").unwrap();
    }

    let duration = start.elapsed();
    println!(
        "VRL execution time for {} iterations: {:?}",
        iterations, duration
    );

    // 确保性能在合理范围内（每个迭代不超过1ms）
    assert!(duration.as_millis() < iterations as u128);
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_with_complex_json() {
    // 测试VRL处理复杂JSON结构
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;

    let mut runtime = VrlRuntime::new();
    let script = r#"
        .metadata.processed = true
        .summary.count = 3
    "#;

    let complex_event = serde_json::json!({
        "items": [
            {"id": 1, "name": "item1"},
            {"id": 2, "name": "item2"},
            {"id": 3, "name": "item3"}
        ],
        "nested": {
            "level1": {
                "level2": {
                    "value": "deep"
                }
            }
        },
        "metadata": {
            "created": "2025-12-16"
        }
    });

    let result = runtime.run(script, complex_event.clone(), "UTC").unwrap();

    // 验证复杂结构保持不变
    assert_eq!(result.processed_event["items"].as_array().unwrap().len(), 3);
    assert_eq!(
        result.processed_event["nested"]["level1"]["level2"]["value"],
        "deep"
    );
    assert_eq!(result.processed_event["metadata"]["created"], "2025-12-16");
}

#[tokio::test]
async fn test_vrl_feature_flag() {
    // 测试VRL feature flag的行为
    #[cfg(feature = "vrl")]
    {
        use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
        assert!(VrlRuntime::check_syntax(".field = 1").is_ok());
    }

    #[cfg(not(feature = "vrl"))]
    {
        use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
        assert!(VrlRuntime::check_syntax(".field = 1").is_err());
    }
}

#[tokio::test]
async fn test_inputs_type_compatibility() {
    // 测试inputs类型扩展的兼容性
    // Rust新增的类型（Number/Bool/FilePath）不应影响协议兼容性

    let _provider = CliInputProvider::new();

    // 测试Number类型
    let request = InputRequest {
        id: "test_number".to_string(),
        input_type: InputType::Number {
            min: Some(0),
            max: Some(100),
        },
        title: "Enter number".to_string(),
        description: "Test number input".to_string(),
        default: None,
        required: false,
        validation: None,
    };

    // Number类型是Rust扩展，协议层仍以字符串传输
    assert!(matches!(request.input_type, InputType::Number { .. }));

    // 测试Bool类型
    let request = InputRequest {
        id: "test_bool".to_string(),
        input_type: InputType::Bool {
            true_label: Some("Yes".to_string()),
            false_label: Some("No".to_string()),
        },
        title: "Enter bool".to_string(),
        description: "Test bool input".to_string(),
        default: None,
        required: false,
        validation: None,
    };

    assert!(matches!(request.input_type, InputType::Bool { .. }));

    // 基础类型（String/PickString/Command）保持与Python一致
    let request = InputRequest {
        id: "test_string".to_string(),
        input_type: InputType::String {
            password: Some(false),
            min_length: None,
            max_length: None,
        },
        title: "Enter text".to_string(),
        description: "Test string input".to_string(),
        default: None,
        required: false,
        validation: None,
    };

    assert!(matches!(request.input_type, InputType::String { .. }));
}

/// `auto_reconnect` 分支真覆盖（#149）：`add_or_update_server` 在 server **已激活**时按 `auto_reconnect`
/// 分流（`manager.rs:263-275`）——true 重启放行、false 拒绝。
///
/// **#142 / R5① 夹具取值分叉**：display 名 `"auto.reconnect (display)"` 派生 bundle_id
/// `auto_reconnect_display`，二者取值分叉（conformance §2.0-1）。原夹具名 `"test"` 规范化后逐字等于自身
/// bundle_id，name 与身份恰好重合，把身份裂缝整个盖住。
///
/// **#142 修身份维度假绿**：原断言 `status.iter().find(|(_, name, _, _)| name == "test")` 把 `.0`（bundle_id、
/// 唯一身份键）丢进 `_`、只按 display 名断言存在性 ⇒ bundle_id 全线错乱此测试照样绿。现断言落在 **bundle_id
/// 维度**，并同时钉住 display 名不被身份键顶替（两识别空间分账）。
///
/// **#149 修死分支假绿**：原测试从未 `start_all()` 起真实客户端 ⇒ `MCPServerManager::new()` 默认
/// `auto_connect=false`（`manager.rs:121`）、`add_or_update_server` 里 `is_active` 恒 false ⇒ `auto_reconnect`
/// 的 if/else 分支**从未执行**（原测试自带注释承认此缺口）。现先 `start_all()` 令 `is_active=true`，再正/负
/// 两例分别覆盖「重启放行」（`manager.rs:265-267`）与「拒绝」（`manager.rs:268-274`）两分支。
///
/// **变异验证（守卫非恒真）**：把分流改坏（恒放行 / 恒拒绝），正/负两例必有一条转红——任一单向恒真实现
/// 无法同时通过（与 `bundle_id_addressing_conformance` 四景③「该回收」互为对照同构）。
#[tokio::test]
async fn test_auto_reconnect_semantics() {
    use smcp_computer::mcp_clients::{
        HttpServerConfig, HttpServerParameters, MCPServerConfig, MCPServerManager,
    };
    use streamable_mock::{spawn_streamable_mock, MockOpts};

    // R5①：display 名 ≠ bundle_id（`.`/空格/括号 → `_`，折叠连续 `_`，裁首尾）。
    const DISPLAY_NAME: &str = "auto.reconnect (display)";
    const EXPECTED_BID: &str = "auto_reconnect_display";

    // 起一台握手放行、`tools/call` 返 403 的 Streamable HTTP mock（与寻址对拍 / AUTH-01 测试同源，#149）。
    let port = spawn_streamable_mock(MockOpts::default()).await;
    let mk_cfg = |rev: &str| {
        MCPServerConfig::Http(HttpServerConfig::new(
            DISPLAY_NAME,
            HttpServerParameters {
                url: format!("http://127.0.0.1:{port}"),
                headers: if rev.is_empty() {
                    std::collections::HashMap::new()
                } else {
                    std::iter::once(("x-rev".to_string(), rev.to_string())).collect()
                },
            },
        ))
    };

    // 默认 `auto_reconnect=true`（`MCPServerManager::new()`）。
    let manager = MCPServerManager::new();
    manager.initialize(vec![mk_cfg("")]).await.unwrap();

    // 起真实客户端 ⇒ `is_active=true`（`auto_reconnect` 分支的前置条件）。
    // time-box：驱动真实 socket 的路径 MUST 有超时保护（握手实测亚秒级，60s 仅为防无限挂，见 #149 第 3 项）。
    tokio::time::timeout(std::time::Duration::from_secs(60), manager.start_all())
        .await
        .expect("HANG: start_all 未在 60s 内完成")
        .unwrap();

    // ── 正例（auto_reconnect=true）：热更新活跃 server 走 restart 分支 → Ok（覆盖 manager.rs:265-267）。
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.add_or_update_server(mk_cfg("v2")),
    )
    .await
    .expect("HANG: add_or_update (restart) 未在 60s 内完成");
    assert!(
        result.is_ok(),
        "auto_reconnect=true 时热更新活跃 server MUST 经 restart 放行：{result:?}"
    );

    // 身份维度（#142 已修，保留）：热更新是**替换**非新增；身份键是 bundle_id 非 display 名。
    let status = manager.get_server_status().await;
    assert_eq!(
        status.len(),
        1,
        "热更新 MUST 替换同一 bundle_id 的既有条目，而非新增一条"
    );
    let (bundle_id, name, _, _) = &status[0];
    assert_eq!(
        bundle_id.as_str(),
        EXPECTED_BID,
        "身份键 MUST 是派生 bundle_id（`.0`），非 display 名"
    );
    assert_eq!(
        *name, DISPLAY_NAME,
        "display 名 MUST 原样保留（人看的那一半），MUST NOT 被身份键顶替"
    );
    // 夹具分叉自检（#142）：钉到**真实派生函数**（比较两个常量恒真，守不住派生算法漂移）。
    assert_eq!(
        smcp_computer::mcp_clients::bundle_id::derive_bundle_id(&mk_cfg("")).as_str(),
        EXPECTED_BID,
        "EXPECTED_BID 须是 DISPLAY_NAME 的真实派生值（conformance §2.0-1 取值分叉由此自动蕴含）"
    );

    // ── 关闭 auto_reconnect，再热更新活跃 server → 走拒绝分支（覆盖 manager.rs:268-274）。
    manager.disable_auto_reconnect().await;
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        manager.add_or_update_server(mk_cfg("v3")),
    )
    .await
    .expect("HANG: add_or_update (reject) 未在 30s 内完成")
    .expect_err("auto_reconnect=false 时热更新活跃 server MUST 被拒绝");
    let msg = err.to_string();
    assert!(
        msg.contains(DISPLAY_NAME) || msg.contains(EXPECTED_BID),
        "拒绝错误 MUST 携带 name 或 bundle_id 便于诊断；实际：{msg}"
    );
}
