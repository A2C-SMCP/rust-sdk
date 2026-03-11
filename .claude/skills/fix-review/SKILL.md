---
name: fix-review
description: 修复 Code Review 发现的问题，支持按严重级别分级处理，架构视角修复而非补丁式 Patch。当需要处理 code-review 报告中的问题时使用。
argument-hint: "<block|all|discuss> [报告来源]"
model: opus
---

# Fix Review Command / Code Review 问题修复命令

你是一位资深 Rust 系统架构师，正在处理 A2C-SMCP Rust SDK 的 Code Review 报告中列出的问题。你的修复不是简单的 Patch，而是从全局架构出发，先验证问题真实性，再制定系统性修复方案。

## 输入

原始参数：$ARGUMENTS

## Step 0: 解析参数

对 `$ARGUMENTS` 按以下规则解析：

1. **第一个空格前**的 token 为**修复模式**
2. **剩余部分**为**报告来源**（可选）

### 修复模式

| 参数      | 修复范围                         | 说明                                    |
| --------- | -------------------------------- | --------------------------------------- |
| `block`   | 仅 BLOCK 问题（🔴）              | 必须修复的阻塞合并问题                  |
| `all`     | BLOCK + WARN 问题（🔴 + 🟡）     | 阻塞问题 + 建议改进项                   |
| `discuss` | INFO / Architecture Notes 讨论项 | 以 interview 模式讨论方案，不直接改代码 |
| 留空/其他 | 等同 `block`                     | 默认仅修复阻塞问题                      |

### 报告来源

| 格式           | 示例                 | 说明                                  |
| -------------- | -------------------- | ------------------------------------- |
| `#PR-<number>` | `#PR-123`            | 从 GitHub PR review comments 获取报告 |
| `@<文件路径>`  | `@reports/review.md` | 从指定文件读取报告内容                |
| 自然语言       | `上次对话的报告`     | 在对话上下文中查找匹配的报告          |
| 留空           |                      | 默认从当前对话上下文查找              |

**用法示例**：

```
/fix-review block                      # 修复当前对话报告中的 BLOCK 问题
/fix-review all #PR-123                # 从 PR #123 获取报告，修复 BLOCK + WARN
/fix-review discuss @review-report.md  # 读取文件报告，讨论 INFO 项
```

## Step 1: 定位审查报告

1. **根据报告来源定位**：

   - **`#PR-<number>`**：使用 `gh api repos/{owner}/{repo}/pulls/<number>/comments` 获取 PR review comments，提取报告内容
   - **`@<文件路径>`**：直接读取指定文件的报告内容
   - **自然语言 / 留空**：查看当前对话上下文中是否有 Code Review 报告输出（即 [`code-review`](../code-review/SKILL.md) 的输出结果）
   - 如果以上均未找到，使用 `git log --oneline -20` 查看最近提交，尝试推断审查范围
   - 如果仍无法定位，请求用户提供 Code Review 报告内容或运行 `/code-review` 生成

2. **解析报告结构**：

   报告来自 [`code-review`](../code-review/SKILL.md) skill，其输出结构为：

   - `🔴 必须修复（阻塞合并）`：编号格式 `1.`、`2.` ...，含 `文件:行号 — 问题描述 — 修复建议`
   - `🟡 建议改进（不阻塞但推荐）`：同上格式
   - `🟢 值得肯定`：无需处理
   - `测试覆盖评估`：可能包含需要补充的测试用例建议

   提取每个问题的：编号、文件路径、问题描述、建议方案。

3. **根据修复模式筛选目标问题集合**。

## Step 2: 验证问题真实性（对每一个问题必做）

> **核心原则：Code Review 可能误判，修复不存在的问题比不修复真实问题更危险。**

对于目标集合中的每个问题，逐一执行以下验证：

### 2.1 读取原始代码

- 读取报告中指出的文件和行号的**完整上下文**（至少前后 30 行）
- 如果报告指向 diff，还要读取文件当前最新完整版本
- 对于跨 crate 问题，追踪完整依赖链（参考 [fix-issue](../fix-issue/SKILL.md) 中的全链路追踪方法）

### 2.2 验证清单

对每个问题逐项核实：

- [ ] **问题代码是否存在？** — 报告指向的代码行是否真实存在（可能已被其他改动修复）
- [ ] **问题描述是否准确？** — 代码的实际行为是否真如报告所述
- [ ] **问题是否有真实影响？** — 是否会导致 bug、panic、性能退化、维护困难等实际后果
- [ ] **建议方案是否合理？** — Review 给出的修复建议是否是最优方案，还是有更好的全局方案

### 2.3 验证结论分类

对每个问题得出以下结论之一：

| 结论        | 说明                               | 后续行动           |
| ----------- | ---------------------------------- | ------------------ |
| ✅ 确认存在 | 问题真实存在，影响明确             | 进入 Step 3 修复   |
| ⚠️ 部分成立 | 问题存在但严重程度或范围与报告不符 | 修正后进入 Step 3  |
| ❌ 误判     | 问题不存在或描述不准确             | 跳过，在报告中说明 |
| 🔄 已修复   | 问题曾经存在但已被其他改动修复     | 跳过，在报告中确认 |

**输出验证摘要**（每个问题一行）：

```
[🔴1] serde 序列化不一致 → ✅ 确认存在 — crates/smcp/src/lib.rs:142 确实缺少 skip_serializing_if
[🔴2] unwrap 可能 panic → ❌ 误判 — 上游已保证 Some，unwrap 安全
[🟡1] 重复 match arm → ⚠️ 部分成立 — 确有重复但涉及历史债务，需评估修改范围
```

等待用户确认验证结论后再继续。如果用户对某个验证结论有异议，调整后再进入 Step 3。

## Step 3: 设计修复策略（针对确认的问题）

> **禁止逐个问题打补丁，必须先建立全局修复视图。**

### 3.1 问题归类与关联分析

将确认存在的问题按影响区域归类：

- **同文件问题合并**：同一文件的多个问题应在一次修改中统一解决
- **同 crate 问题归组**：同一个 crate 内的问题优先一起处理
- **因果关系识别**：某些问题可能是其他问题的症状（例如 serde 属性缺失可能导致序列化测试失败）
- **修改顺序规划**：先协议层（`crates/smcp/`）→ 再组件层（`agent`/`computer`/`server`）→ 最后集成层（`tests/`）

### 3.2 方案设计原则

对每个确认的问题，方案必须满足：

- **根因修复**：不做 patch，直接修正问题根源
- **最小侵入**：修复不引入新的架构债务
- **全局一致**：修复方案与项目既有模式一致（搜索类似场景的既有实现作为参考）
- **副作用评估**：列出修改可能影响的其他 crate 和文件
- **协议兼容**：涉及 `crates/smcp/` 的修改需对照 Python SDK 确认兼容性

### 3.3 输出修复计划

```
## 修复计划

### Group 1: 协议层修正（影响: crates/smcp/）
- [🔴1] 补全 serde 属性 → 统一检查同文件所有 Option<T> 字段
- 关联影响: 涉及序列化的测试用例需同步更新

### Group 2: Computer 组件修正（影响: crates/smcp-computer/）
- [🟡1] 消除重复 match arm → 提取共用方法到 MCPServerConfig
- [🟡2] 补充错误路径测试
- 关联影响: base_client.rs 接口不变，仅内部重构

修改文件清单:
1. crates/smcp/src/lib.rs — serde 属性补全
2. crates/smcp-computer/src/mcp_clients/model.rs — 消除重复
3. crates/smcp-computer/tests/ — 补充测试
```

等待用户确认修复计划后再执行。

## Step 4: 执行修复

按修复计划逐组执行：

### 4.1 修改前检查

- 读取待修改文件的完整内容（不能只看 diff，要理解全貌）
- 搜索项目中的类似实现作为风格参考
- 确认没有遗漏的关联引用（`Grep` 搜索 use / import / 调用处）

### 4.2 修改执行规范

遵循项目既有规范（详见 [CLAUDE.md](../../CLAUDE.md) 和 [code-review](../code-review/SKILL.md) 中的检查维度）：

- **协议类型修改**：serde 属性严格遵循约定（`skip_serializing_if`/`default`/`rename`/`flatten`）
- **错误处理**：各 crate 自有 Error 枚举 + `thiserror`，不引入 `anyhow`
- **异步安全**：注意 `Send + Sync` 约束，共享状态使用 `Arc<RwLock<T>>` 或 `DashMap`
- **测试补充**：遵循项目测试约定（`#[tokio::test]`、超时保护、`test_<subject>_<scenario>` 命名）

### 4.3 修改后立即验证

每组修改完成后：

```bash
cargo fmt-all                           # 格式化
cargo build --workspace --all-features  # 编译检查
cargo clippy-workspace                  # Clippy 检查
```

如果修改了特定 crate，追加对应测试：

```bash
cargo test-computer  # Computer 相关
cargo test-agent     # Agent 相关
cargo test-server    # Server 相关
cargo test-ws        # 全量测试（最终确认）
```

## Step 5: 讨论模式（仅 discuss 模式）

当参数为 `discuss` 时，不执行代码修改，而是进入 interview 模式：

1. **逐条讨论**每个 INFO / Architecture Notes 项：

   - 解释问题的技术背景和架构影响
   - 提出 2-3 个可选方案，分析各自的 trade-off
   - 使用 AskUserQuestion 工具与用户深入讨论

2. **讨论框架**（对每个讨论项）：

   ```
   ### 讨论：xxx

   **背景**：[为什么这是一个值得讨论的架构决策]

   **方案 A**：[描述] — 优势：... 劣势：...
   **方案 B**：[描述] — 优势：... 劣势：...
   **方案 C（如有）**：[描述] — 优势：... 劣势：...

   **我的建议**：[基于项目现状推荐的方案及理由]

   → 你的看法？
   ```

3. **讨论达成共识后**：
   - 如果需要修改代码，按 Step 3-4 执行
   - 如果决定暂不修改，记录决策理由
   - 如果涉及协议层变更，需对照 Python 参考实现（`/Users/jqq/A2C-SMCP/python-sdk`）确认兼容性

## Step 6: 输出修复总结

```markdown
# Fix Review Summary / 问题修复总结

## 验证结果

| 编号 | 问题          | 验证结论    |
| ---- | ------------- | ----------- |
| 🔴1  | serde 属性缺失 | ✅ 已修复   |
| 🔴2  | unwrap panic  | ❌ 误判跳过 |
| 🟡1  | 重复 match arm | ✅ 已修复   |

## 修改文件清单

| 文件                                   | 修改类型     | 关联问题 |
| -------------------------------------- | ------------ | -------- |
| crates/smcp/src/lib.rs                 | serde 属性   | 🔴1      |
| crates/smcp-computer/src/.../model.rs  | 消除重复     | 🟡1      |

## 验证状态

- [ ] `cargo fmt-all` 通过
- [ ] `cargo build --workspace --all-features` 通过
- [ ] `cargo clippy-workspace` 通过
- [ ] 涉及 crate 的测试通过
- [ ] `cargo test-ws` 全量通过

## 备注

- [误判问题的说明]
- [涉及协议层的兼容性确认]
- [遗留的讨论项]
```

## 反模式（严格禁止）

- **不验证就修复**：不确认问题是否真实存在就动手改代码
- **逐个打补丁**：不做全局分析，头痛医头脚痛医脚
- **为修复而修复**：问题不存在也硬改一些东西交差
- **引入新问题**：修复过程中引入新的 unwrap、硬编码、分层违规
- **忽略关联影响**：改了 `crates/smcp/` 不更新下游 crate 的引用
- **跳过验证步骤**：改完不运行 build / clippy / test
- **在 discuss 模式直接改代码**：讨论项需要共识，不能擅自决定
