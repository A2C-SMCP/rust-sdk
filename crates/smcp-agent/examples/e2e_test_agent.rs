/*!
* 文件名: e2e_test_agent
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2026/06/12
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: 完整链路 UAT 的 env 驱动 Agent 驱动器 / env-driven Agent driver for full-protocol UAT。
*
*   按 `SMCP_TEST_MODE` 选择要驱动的协议面（可逗号分隔多选，或 `all`），每个用例在 Computer 上发起
*   对应 `client:*` / `server:*` 调用并打印 `UAT_RESULT: PASS|FAIL <mode> ...` 标记，供编排脚本
*   （full-protocol-uat.sh）grep 判定。覆盖 full-protocol 场景 F-02/F-08/F-09/F-10/F-11/F-12、
*   skill-discovery D-05（渐进披露），以及 #82 修复后新增的三场景端到端迁移：
*     - resource-discovery（mode `get_resources`）：R-01/R-02 透传+window://、R-03 4014、R-04 4015
*     - blob-transfer（mode `blob`）：B-01 inline、B-02/B-03/B-04 超内联预算二进制 sideband 透明 round-trip
*     - error-codes（mode `errors`）：E-01 4016、E-03 4014、E-04 4017、E-08 4018、E-11 404(#92 回归)
*   F-05（版本 4008）由编排脚本经 curl/HTTP 覆盖；F-07（get_config）当前 Agent SDK 未暴露方法，标 SKIP。
*
*   env 参数：
*     SMCP_SERVER_URL   服务端 URL（默认 http://127.0.0.1:8000）
*     SMCP_AGENT_ID     Agent 名（默认 e2e-test-agent）
*     SMCP_OFFICE_ID    office 名（默认 e2e-test-office）
*     SMCP_API_KEY      可选 API Key
*     SMCP_COMPUTER     目标 Computer 名（默认 test-computer）
*     SMCP_SKILL_NAME   D-05 目标 skill 名（默认 valid-skill-pkg）
*     SMCP_TEST_MODE    用例选择，逗号分隔或 `all`；兼容旧值 `tool_call`（= get_tools / F-08）
*/

use std::env;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{error, info, warn};

use base64::Engine as _;
use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig, SmcpAgentError};

/// 单用例调用的统一超时。
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// 打印 UAT 结果标记并返回是否 PASS（供汇总统计 / 退出码）。
fn pass(mode: &str, detail: &str) -> bool {
    info!("UAT_RESULT: PASS {mode} {detail}");
    true
}
fn fail(mode: &str, detail: &str) -> bool {
    error!("UAT_RESULT: FAIL {mode} {detail}");
    false
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server_url =
        env::var("SMCP_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8000".to_string());
    let agent_id = env::var("SMCP_AGENT_ID").unwrap_or_else(|_| "e2e-test-agent".to_string());
    let office_id = env::var("SMCP_OFFICE_ID").unwrap_or_else(|_| "e2e-test-office".to_string());
    let api_key = env::var("SMCP_API_KEY").ok();
    let computer = env::var("SMCP_COMPUTER").unwrap_or_else(|_| "test-computer".to_string());
    let skill_name = env::var("SMCP_SKILL_NAME").unwrap_or_else(|_| "valid-skill-pkg".to_string());
    // 兼容旧脚本：SMCP_TEST_MODE=tool_call 历史含义为 get_tools(F-08)。
    let raw_mode = env::var("SMCP_TEST_MODE").unwrap_or_else(|_| "get_tools".to_string());

    info!("Starting E2E Test Agent");
    info!("Server URL: {server_url}");
    info!("Agent ID: {agent_id} / Office: {office_id} / Computer: {computer}");
    info!("Test mode: {raw_mode}");

    // 解析要跑的用例集合。
    let modes: Vec<String> = if raw_mode == "all" {
        // leave_office 放最后（会断开 office），其余按协议面排序。
        [
            "get_tools",
            "call_tool",
            "get_desktop",
            "list_room",
            "get_resources",
            "blob",
            "skill_disclosure",
            "errors",
            "tool_call_cancel",
            "leave_office",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    } else {
        raw_mode
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    // 认证 + 配置 + 连接 + join。
    let auth = DefaultAuthProvider::new(agent_id.clone(), office_id.clone());
    let auth = if let Some(key) = api_key {
        auth.with_api_key(key)
    } else {
        auth
    };
    let config = SmcpAgentConfig::new()
        .with_default_timeout(10)
        .with_tool_call_timeout(10)
        .with_reconnect_interval(1000)
        .with_max_retries(3);
    let mut agent = AsyncSmcpAgent::new(auth, config);

    info!("Connecting to server...");
    agent.connect(&server_url).await?;
    info!("Connected; joining office...");
    agent.join_office(&agent_id).await?;
    info!("Joined office; waiting for room to settle...");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut all_pass = true;
    let mut ran = 0usize;
    for mode in &modes {
        ran += 1;
        let ok = run_mode(&agent, mode, &computer, &office_id, &agent_id, &skill_name).await;
        all_pass &= ok;
    }

    // leave_office 若不在用例集合里，收尾仍礼貌离开（不计入断言）。
    if !modes.iter().any(|m| m == "leave_office") {
        let _ = agent.leave_office().await;
    }

    info!("E2E Test Agent done: {ran} mode(s) ran, all_pass={all_pass}");
    if all_pass {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// 驱动单个用例，返回是否 PASS。
async fn run_mode(
    agent: &AsyncSmcpAgent,
    mode: &str,
    computer: &str,
    office_id: &str,
    agent_id: &str,
    skill_name: &str,
) -> bool {
    match mode {
        // ── F-08：get_tools（兼容旧值 tool_call）─────────────────────────────
        "get_tools" | "tool_call" => match timeout(CALL_TIMEOUT, agent.get_tools(computer)).await {
            Ok(Ok(tools)) => {
                let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
                if tools.is_empty() {
                    fail("get_tools", "工具列表为空")
                } else {
                    pass(
                        "get_tools",
                        &format!("got {} tools {:?}", tools.len(), names),
                    )
                }
            }
            Ok(Err(e)) => fail("get_tools", &format!("err: {e}")),
            Err(_) => fail("get_tools", "timeout"),
        },

        // ── F-02：tool_call 路由（echo 回显）─────────────────────────────────
        "call_tool" => {
            let probe = "hello-uat";
            match timeout(
                CALL_TIMEOUT,
                agent.tool_call(
                    computer,
                    "echo__echo",
                    serde_json::json!({ "message": probe }),
                ),
            )
            .await
            {
                Ok(Ok(result)) => {
                    let s = result.to_string();
                    if s.contains(probe) {
                        pass("call_tool", &format!("echo 回显含 '{probe}'"))
                    } else {
                        fail("call_tool", &format!("结果未含 '{probe}': {s}"))
                    }
                }
                Ok(Err(e)) => fail("call_tool", &format!("err: {e}")),
                Err(_) => fail("call_tool", "timeout"),
            }
        }

        // ── F-09：get_desktop（窗口资源）────────────────────────────────────
        "get_desktop" => {
            match timeout(CALL_TIMEOUT, agent.get_desktop(computer, None, None)).await {
                Ok(Ok(windows)) => {
                    if windows.is_empty() {
                        // 无 window 资源不算硬失败（取决于 MCP fixture），记 WARN。
                        warn!("get_desktop 返回空（fixture 可能无 window:// 资源）");
                        pass("get_desktop", "0 windows (fixture 无 window 资源)")
                    } else {
                        pass("get_desktop", &format!("{} windows", windows.len()))
                    }
                }
                Ok(Err(e)) => fail("get_desktop", &format!("err: {e}")),
                Err(_) => fail("get_desktop", "timeout"),
            }
        }

        // ── F-10：list_room（房间成员）──────────────────────────────────────
        "list_room" => match timeout(CALL_TIMEOUT, agent.list_room(office_id)).await {
            Ok(Ok(sessions)) => {
                let me = sessions.iter().any(|s| s.name == agent_id);
                if me {
                    pass(
                        "list_room",
                        &format!("{} sessions，含自身 {agent_id}", sessions.len()),
                    )
                } else {
                    fail(
                        "list_room",
                        &format!("{} sessions 但未含自身 {agent_id}", sessions.len()),
                    )
                }
            }
            Ok(Err(e)) => fail("list_room", &format!("err: {e}")),
            Err(_) => fail("list_room", "timeout"),
        },

        // ── F-12：tool_call_cancel（fire-and-forget 传输契约）────────────────
        // 在途 sleep 工具的 req_id 由 SDK 内部生成、不外露，故此处验证 server:tool_call_cancel 的
        // fire-and-forget emit 契约（无 ack、不报错即视为传输层成立）。结果级 a2c_cancelled 由
        // crate 级单测与 in-process 矩阵覆盖。
        "tool_call_cancel" => {
            match timeout(CALL_TIMEOUT, agent.tool_call_cancel("uat-cancel-probe")).await {
                Ok(Ok(())) => pass("tool_call_cancel", "fire-and-forget emit 成立（无 ack）"),
                Ok(Err(e)) => fail("tool_call_cancel", &format!("emit err: {e}")),
                Err(_) => fail("tool_call_cancel", "timeout"),
            }
        }

        // ── D-05：渐进披露 get_skills → get_skill（含 frontmatter 剥离 + 4 必选字段）──
        "skill_disclosure" => {
            // 1) get_skills → 引用列表 + 4 必选字段
            let skills = match timeout(CALL_TIMEOUT, agent.get_skills(computer)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return fail("skill_disclosure", &format!("get_skills err: {e}")),
                Err(_) => return fail("skill_disclosure", "get_skills timeout"),
            };
            let Some(skill) = skills.iter().find(|s| s.name == skill_name) else {
                return fail(
                    "skill_disclosure",
                    &format!(
                        "未发现目标 skill {skill_name}，实得 {:?}",
                        skills.iter().map(|s| &s.name).collect::<Vec<_>>()
                    ),
                );
            };
            // 4 必选字段：name / source / path / description 非空。
            if skill.name.is_empty()
                || skill.source.is_empty()
                || skill.path.is_empty()
                || skill.description.is_empty()
            {
                return fail(
                    "skill_disclosure",
                    &format!("A2CSkillRef 4 必选字段缺失: {skill:?}"),
                );
            }
            // 2) get_skill 入口（SKILL.md）→ frontmatter 剥离的 inline body
            let entry =
                match timeout(CALL_TIMEOUT, agent.get_skill(computer, skill_name, None)).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return fail("skill_disclosure", &format!("get_skill err: {e}")),
                    Err(_) => return fail("skill_disclosure", "get_skill timeout"),
                };
            let body = entry.body.unwrap_or_default();
            if body.is_empty() {
                return fail("skill_disclosure", "SKILL.md body 为空（应 inline 回正文）");
            }
            if body.starts_with("---") && body.contains("description:") {
                return fail("skill_disclosure", "frontmatter 未剥离（body 仍含 --- 头）");
            }
            pass(
                "skill_disclosure",
                &format!(
                    "skill={} source={} 4 字段齐全；SKILL.md inline {} 字节",
                    skill.name,
                    skill.source,
                    body.len()
                ),
            )
        }

        // ── resource-discovery：R-01/R-02 透传 + R-03 4014 + R-04 4015 ───────
        // MCP fixture：主 server 注册名 "echo"（= tests/v022-mcp-server，含 window:// 资源）；
        // 4015 用无 resources 能力的 server，注册名 "no-resources"（编排脚本挂载）。
        "get_resources" => {
            // R-01/R-02：成功透传，含 window:// 资源，mime_type 为 snake_case（经强类型 A2CResource）。
            let ret = match timeout(CALL_TIMEOUT, agent.get_resources(computer, "echo", None)).await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return fail("get_resources", &format!("R-01 err: {e}")),
                Err(_) => return fail("get_resources", "R-01 timeout"),
            };
            if ret.resources.is_empty() {
                return fail("get_resources", "R-01 资源列表为空");
            }
            let has_window = ret.resources.iter().any(|r| {
                r.uri
                    .as_deref()
                    .map(|u| u.starts_with("window://"))
                    .unwrap_or(false)
            });
            if !has_window {
                let uris: Vec<_> = ret.resources.iter().map(|r| r.uri.clone()).collect();
                return fail(
                    "get_resources",
                    &format!("R-02 未含 window:// 资源: {uris:?}"),
                );
            }
            // R-03：未知 MCP server → 4014（顶层 mcp_server 分流）。
            match timeout(
                CALL_TIMEOUT,
                agent.get_resources(computer, "nonexistent-server", None),
            )
            .await
            {
                Ok(Err(SmcpAgentError::Protocol(p))) if p.code == 4014 => {}
                other => {
                    return fail(
                        "get_resources",
                        &format!("R-03 期望 4014 Protocol，实得 {other:?}"),
                    )
                }
            }
            // R-04：目标 server 无 resources 能力 → 4015（顶层 capability="resources"）。
            match timeout(
                CALL_TIMEOUT,
                agent.get_resources(computer, "no-resources", None),
            )
            .await
            {
                Ok(Err(SmcpAgentError::Protocol(p))) if p.code == 4015 => pass(
                    "get_resources",
                    &format!(
                        "R-01 {} 资源含 window://；R-03 4014；R-04 4015 capability={:?}",
                        ret.resources.len(),
                        p.capability
                    ),
                ),
                other => fail(
                    "get_resources",
                    &format!("R-04 期望 4015 Protocol，实得 {other:?}"),
                ),
            }
        }

        // ── blob-transfer：B-01 inline + B-02/B-03/B-04 二进制 sideband 透明 round-trip ──
        "blob" => {
            // B-01：小文本 SKILL 资源 inline 回 body（< inline_budget，无 blob_handle）。
            match timeout(
                CALL_TIMEOUT,
                agent.get_skill(computer, skill_name, Some("references/usage.md")),
            )
            .await
            {
                Ok(Ok(r)) => {
                    if r.body.as_deref().unwrap_or("").is_empty() {
                        return fail("blob", "B-01 inline body 为空（应直接回正文）");
                    }
                }
                Ok(Err(e)) => return fail("blob", &format!("B-01 get_skill err: {e}")),
                Err(_) => return fail("blob", "B-01 timeout"),
            }
            // B-02/B-03/B-04：gen_image 产 40000B 二进制，base64 后 ~53KB > 32KB inline_budget →
            // Computer 必经 `_meta.a2c_blob_handle` 旁路下发；高层 tool_call 透明 drain 回填字节
            // （AGT-04 #41）。若 sideband 链路断裂，超预算二进制会静默变空——故「字节齐全且确定性
            // 模式逐字节一致」即端到端自证 mint+drain+完整性（B-04 + B-03 SHA 级一致性的更强形式）。
            const N: usize = 40000;
            let result = match timeout(
                CALL_TIMEOUT,
                agent.tool_call(
                    computer,
                    "echo__gen_image",
                    serde_json::json!({ "bytes": N }),
                ),
            )
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return fail("blob", &format!("B-04 tool_call err: {e}")),
                Err(_) => return fail("blob", "B-04 timeout"),
            };
            let data_b64 = result
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find_map(|it| it.get("data").and_then(|d| d.as_str()))
                });
            let Some(b64) = data_b64 else {
                return fail(
                    "blob",
                    &format!("B-04 结果无 image data（sideband drain 失败?）: {result}"),
                );
            };
            let bytes = match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                Ok(b) => b,
                Err(e) => return fail("blob", &format!("B-04 base64 解码失败: {e}")),
            };
            if bytes.len() != N {
                return fail(
                    "blob",
                    &format!("B-04 字节数 {} != {N}（截断/损坏）", bytes.len()),
                );
            }
            // 确定性字节模式（与 fixture deterministicImage 同式 byte[i]=(i*31+7)&0xff）逐字节自证。
            for (i, b) in bytes.iter().enumerate() {
                let want = (i.wrapping_mul(31).wrapping_add(7) & 0xff) as u8;
                if *b != want {
                    return fail("blob", &format!("B-03 第 {i} 字节确定性模式不符"));
                }
            }
            pass(
                "blob",
                &format!("B-01 inline body 非空；B-02/03/04 {N}B 二进制经 sideband 透明 round-trip，逐字节一致"),
            )
        }

        // ── error-codes：E-01 4016 / E-03 4014 / E-04 4017 / E-08 4018 / E-11 404 ──
        "errors" => {
            // E-01：SKILL name 路径穿越格式非法 → 4016。
            match timeout(
                CALL_TIMEOUT,
                agent.get_skill(computer, "../etc/passwd", None),
            )
            .await
            {
                Ok(Err(SmcpAgentError::Protocol(p))) if p.code == 4016 => {}
                other => {
                    return fail(
                        "errors",
                        &format!("E-01 期望 4016 Protocol，实得 {other:?}"),
                    )
                }
            }
            // E-03：SKILL name 合法但不存在 → 4014（复用 MCP_SERVER_NOT_FOUND 语义）。
            match timeout(
                CALL_TIMEOUT,
                agent.get_skill(computer, "nonexistent-skill", None),
            )
            .await
            {
                Ok(Err(SmcpAgentError::Protocol(p))) if p.code == 4014 => {}
                other => {
                    return fail(
                        "errors",
                        &format!("E-03 期望 4014 Protocol，实得 {other:?}"),
                    )
                }
            }
            // E-04：rel_path 路径穿越 → 4017（details.reason=traversal）。
            match timeout(
                CALL_TIMEOUT,
                agent.get_skill(computer, skill_name, Some("../../etc/passwd")),
            )
            .await
            {
                Ok(Err(SmcpAgentError::Protocol(p))) if p.code == 4017 => {}
                other => {
                    return fail(
                        "errors",
                        &format!("E-04 期望 4017 Protocol，实得 {other:?}"),
                    )
                }
            }
            // E-08：无效 blob 句柄 → 4018。
            match timeout(
                CALL_TIMEOUT,
                agent.get_blob(computer, "a2c:invalid:totally-fake-handle", None, None),
            )
            .await
            {
                Ok(Err(SmcpAgentError::Protocol(p))) if p.code == 4018 => {}
                other => {
                    return fail(
                        "errors",
                        &format!("E-08 期望 4018 Protocol，实得 {other:?}"),
                    )
                }
            }
            // E-11：目标 Computer 不存在 → flat ErrorPayload 404（#92 回归：MUST NOT 退化为超时/挂起）。
            match timeout(
                CALL_TIMEOUT,
                agent.get_skill("ghost-computer-999", "any-skill", None),
            )
            .await
            {
                Ok(Err(SmcpAgentError::Protocol(p))) if p.code == 404 => pass(
                    "errors",
                    "E-01 4016 / E-03 4014 / E-04 4017 / E-08 4018 / E-11 404 全部命中",
                ),
                Ok(Err(SmcpAgentError::Timeout)) | Err(_) => {
                    fail("errors", "E-11 退化为超时（#92 回归失败：应回 flat 404）")
                }
                other => fail("errors", &format!("E-11 期望 404 Protocol，实得 {other:?}")),
            }
        }

        // ── F-11：leave_office ──────────────────────────────────────────────
        "leave_office" => match timeout(CALL_TIMEOUT, agent.leave_office()).await {
            Ok(Ok(())) => pass("leave_office", "已离开 office"),
            Ok(Err(e)) => fail("leave_office", &format!("err: {e}")),
            Err(_) => fail("leave_office", "timeout"),
        },

        other => {
            warn!("UAT_RESULT: SKIP {other} (未知用例 mode)");
            true
        }
    }
}
