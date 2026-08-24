# 场景：blob-transfer

## 测试目标

验证 SKILL 资源 inline/blob 阈值切换、tool_call 二进制 sideband blob 传输与端到端字节/SHA 一致性。
对标 python-sdk 同名场景 B-01~B-04。

## 类型

完整链路（Agent → Server → Computer 三真实进程）。

## 状态

✅ **端到端通过**（#82 ack 拆封 + [#83](https://github.com/A2C-SMCP/rust-sdk/issues/83) `run` 补 `boot_up()` 修复后）。
由 `full-protocol-uat.sh` 的 `blob` mode 驱动：B-01 inline body 非空；B-02/03/04 的 40000B 二进制经
sideband 透明 round-trip，逐字节一致。

> 历史：#83 修复前，`smcp-computer run` 从不调用 `Computer::boot_up()`，blob 子系统未装配、user 源
> SKILL 未发现，本场景曾 known-blocked。修复（`prepare_handler` 在 connect 前 `boot_up()`）后自动转绿。

## 测试用例

### B-01: 小资源 inline（< inline_budget 直接返回 body）

- **优先级**: P0
- **步骤**: `get_skill(computer, "valid-skill-pkg", Some("references/usage.md"))`
- **预期**: `GetSkillRet.body` 非空（inline 文本，无 blob_handle）；`total_size <= 32768`。

### B-02/B-03/B-04: 超内联预算二进制经 sideband 透明 round-trip

- **优先级**: P0/P1
- **步骤**: `tool_call(computer, "gen_image", {"bytes": 40000})`
  （40000B base64 后 ~53KB > 32KB inline_budget → Computer 必经 `_meta.a2c_blob_handle` 旁路；
  高层 `tool_call` 透明 drain 回填字节，AGT-04 #41）
- **预期**:
  - 结果 content 含 image data（sideband drain 成功，未静默变空）
  - base64 解码字节数 == 40000（B-04 无截断/损坏）
  - 逐字节满足确定性模式 `byte[i] == (i*31+7) & 0xff`（B-03 SHA 级一致性的逐字节更强形式，
    与 fixture `deterministicImage` 同式）

> **观测说明**：高层 `tool_call` 自动 drain 二进制 sideband，故裸 `a2c_blob_handle` 不外露；
> 「>inline budget 的二进制透明回正确字节」即端到端自证 mint+drain+完整性。裸句柄分块重组
> （offset/eof/sha256/total_size）由矩阵 `tool_call_binary_blob_roundtrip` 在底层 emit 覆盖。

## 驱动器实现

`crates/smcp-agent/examples/e2e_test_agent.rs` 的 `blob` mode。
