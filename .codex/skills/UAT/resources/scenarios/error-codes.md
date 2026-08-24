# 场景：error-codes

## 测试目标

验证 A2C-SMCP 协议错误码负面路径端到端：SKILL 名称格式非法（4016）、SKILL 名合法但不存在（4014）、
SKILL 资源路径穿越（4017）、Blob 句柄无效（4018），以及 Server 路由层目标 Computer 不存在时回
flat ErrorPayload 404（#92 回归守卫）。对标 python-sdk 同名场景 E-01~E-16 的代表性 P0 子集。

## 类型

完整链路（Agent → Server → Computer 三真实进程）。

## 状态

✅ **端到端通过**（#82 ack 拆封 + [#83](https://github.com/A2C-SMCP/rust-sdk/issues/83) `run` 补 `boot_up()` 修复后）。
`errors` mode 单次运行连测 5 码全命中：E-01 4016 / E-03 4014 / E-04 4017 / E-08 4018 / E-11 404。

> 历史：#83 修复前，依赖 SKILL 子系统的 E-04（期望 4017）先撞 `4014 SKILL not found`（skill 未 staged）。
> `run` 补 `boot_up()` 后 user 源 SKILL 可发现，沙箱解析到达，E-04 正确回 4017 traversal。错误码表达力
> （`SmcpProtocolError.code/details/reason/capability/mcp_server_name`）另由 crate 级单测（`response.rs`）覆盖。

## 测试用例（代表性 P0 子集）

### E-01: SKILL name 路径穿越格式非法 → 4016

- **步骤**: `get_skill(computer, "../etc/passwd", None)`
- **预期**: `Err(Protocol)`，`code == 4016`（SKILL_NAME_INVALID）。

### E-03: SKILL name 合法但不存在 → 4014

- **步骤**: `get_skill(computer, "nonexistent-skill", None)`
- **预期**: `code == 4014`（复用 MCP_SERVER_NOT_FOUND 语义）。区分力依赖 #83 已修（已发现的合法 skill
  返回正常、不存在名才回 4014）。

### E-04: SKILL rel_path 路径穿越 → 4017

- **步骤**: `get_skill(computer, "valid-skill-pkg", Some("../../etc/passwd"))`
- **预期**: `code == 4017`（SKILL_RESOURCE_NOT_ACCESSIBLE），`details.reason == "traversal"`。

### E-08: Blob 句柄无效 → 4018

- **步骤**: `get_blob(computer, "a2c:invalid:totally-fake-handle", None, None)`
- **预期**: `code == 4018`（BLOB_NOT_ACCESSIBLE），`details.reason ∈ {invalid_handle, gone}`。
- **不依赖 #83**（句柄校验在 blob resolver 之前的格式判定）。

### E-11: 目标 Computer 不存在 → flat ErrorPayload 404（#92 回归）

- **步骤**: `get_skill("ghost-computer-999", "any-skill", None)`
- **预期**: `Err(Protocol)`，`code == 404`（Server 路由层 `build_computer_not_found_error`）；
  **MUST NOT** 退化为超时/挂起。
- **不依赖 #83**（Server 路由层先于 Computer 拦截）。

## 驱动器实现

`crates/smcp-agent/examples/e2e_test_agent.rs` 的 `errors` mode：5 码顺序连测，任一不符即 FAIL。

## 与 Python 的差异

- Rust Server 对 computer-not-found 统一回 **404**（`ErrorCode::NotFound`，不泄露存在性、不新增码），
  对应 python #92 的 computer-not-found 语义。
- E-02/E-05/E-06/E-07/E-09/E-10/E-12~E-16 为同族扩展，未逐一迁移（代表性 P0 已覆盖各错误码闭集）。
