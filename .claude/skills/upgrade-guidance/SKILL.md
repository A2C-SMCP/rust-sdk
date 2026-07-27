---
name: upgrade-guidance
description: 为下游兄弟项目（如 tfrobot-client）生成 rust-sdk 升级指导报告，并以 Issue 交付到对方仓库。输入兄弟项目当前使用的 commit hash 或版本 Tag，目标版本缺省为当前工作空间分支的 HEAD。当需要通知下游"升级会打到什么"、评估某个区间的破坏面、或兄弟项目询问能否升级时使用。
argument-hint: "<兄弟当前 commit hash | 版本 Tag> [目标 commit/Tag，缺省=当前分支 HEAD] [兄弟项目名，缺省=tfrobot-client]"
---

# Upgrade Guidance — 向下游兄弟生成升级指导报告

rust-sdk 是 `tfrobot-client` 等下游项目的依赖。快速迭代期（0.3.0-dev 未发布、develop 持续演进）下游无法自行判断"这个区间会打到我什么"，本 Skill 产出定向、已验证、可执行的升级指导，并落为对方仓库的 Issue。

**Golden 示例（本 Skill 的第一次真实运行，建议先读）**：

- 报告全文：[`resources/example-tfrobot-client-91b5758-to-98d75cc.md`](resources/example-tfrobot-client-91b5758-to-98d75cc.md)
- 交付产物：[tfrobot-client#32](https://github.com/A2C-SMCP/tfrobot-client/issues/32)

## 为什么需要这套方法（四条实证反直觉）

这些不是理论，是第一次真跑（`91b5758..98d75cc`，7 提交）实测出来的。**它们决定了下面每一步为什么长这样。**

### 1. `!` 破坏性标记与真实影响面几乎不相关

| 提交 | `!` 标记 | 对下游生产代码的真实影响 |
| --- | --- | --- |
| `98d75cc` #136 | **有** | **零影响**（改的是 wire 类型 `SMCPTool`，下游用的是 rmcp `Tool`，两个类型） |
| `937f0bb` #132 | **无**（自述"纯改名/零行为"） | **唯一的编译期破坏**（`ComputerError` 字段改名） |

⇒ **不能按 `!` 筛选，必须逐提交对代码验证。** commit message 是作者视角（"我改了什么"），不是消费者视角（"谁会疼"）。

### 2. 版本号可能是假信号

`91b5758` 与 `98d75cc` 两端 workspace 版本**都是 `0.3.0-dev.0`**，期间发生 3 次破坏性变更。0.3.0 未发布 ⇒ [`CHANGELOG.md`](../../../CHANGELOG.md) 也不覆盖（git-cliff 只生成已发布区间，规则见 [`cliff.toml`](../../../cliff.toml)）。

⇒ **dev 期只认 commit hash。** 输入的"版本 Tag"要先解析成 hash 再用。

### 3. 兄弟的「API 依赖清单」是过滤器，不是真相

没有清单：7 个提交全报一遍，对方淹死在噪音里。有清单：只报命中面 —— 本次 7 提交收敛成 1 个 P0 + 2 个 P1。

但清单**本身可能与代码现状矛盾**。本次挖出 3 处（清单说 client 从 `Tool.meta.server_name` 读工具归属，而 SDK 从不写这个 key ⇒ 一处死读取）。

⇒ **清单定义「什么值得说」，代码定义「事实是什么」。两者冲突时，冲突本身就是报告里最有价值的部分**——那是对方不知道自己有的 bug。

### 4. 最危险的是「签名没变、行为变了」

编译器抓不到，测试也未必抓得到（本次下游那两个 alias 测试升级后**继续绿**，但它们不断言工具数量，捕捉不到真正的变化）。#134、#143 都属这类。

⇒ 每个 diff hunk 都要问一遍：**签名没动，但语义动了吗？**

---

## Step 1 — 参数解析与基线核准

```
/upgrade-guidance <兄弟当前 commit|Tag> [目标，缺省=HEAD] [兄弟项目名，缺省=tfrobot-client]
```

缺参数时用 `AskUserQuestion` 问。目标缺省 = 当前分支 HEAD（`git rev-parse HEAD`）。

必做的核准（**顺序不能反**）：

```bash
git cat-file -t <baseline>        # 基线在本仓存在吗？（Tag 要先解析成 hash）
git log --oneline <baseline>..<target>
grep -A3 '\[workspace.package\]' Cargo.toml   # 两端版本号 —— 若相同，报告里必须显式警告"版本号是假信号"
```

**输出**：区间提交清单 + 两端版本号。若区间为空 ⇒ 直接告诉用户无需升级，结束。

## Step 2 — 核实「声称的基线」vs 兄弟仓的实际 pin

**这一步最容易被跳过，也最容易翻车。** 本次实测：清单声称基线 `91b5758`，但它**只存在于一个 feature 分支**，main/develop 钉的是完全不同的 crates.io 依赖：

| 分支 | 实际依赖 |
| --- | --- |
| `main` | `smcp-computer = "0.1.14"` |
| `develop` | `smcp-computer = "0.2.3"` |
| `feature/split_runtime_config` | `a2c-smcp` git rev `91b5758` ← 唯一匹配 |

```bash
gh api repos/A2C-SMCP/<兄弟>/branches --jq '.[] | {name, sha: .commit.sha[0:8]}'
# 逐分支读 pin：
gh api "repos/A2C-SMCP/<兄弟>/contents/<manifest 路径>?ref=<branch>" --jq '.content' | base64 -d | grep -i -A2 "a2c-smcp\|smcp-computer"
# 分支活跃度：
gh api "repos/A2C-SMCP/<兄弟>/commits?sha=<branch>&per_page=1" --jq '.[0].commit.committer.date'
```

**输出**：一张分支→pin 表 + 明确结论「本报告只对 `<分支>` 有效」。若多分支分歧，在报告开头显式声明收件对象，并说明其他分支属于另一个迁移话题。

## Step 3 — 取兄弟的 API 依赖清单

清单由兄弟维护、**不一定在他们仓库里**（本次不在，是人肉传递的）。优先级：用户提供 → 兄弟仓 `docs/` → 无清单则退化为"扫兄弟源码的 `use` 面"。

落到 scratchpad 供后续 agent 读（本次即 `client-api-inventory.md`）。

**没有清单就不要硬跑**——报告会退化成 changelog 复述，对方读不动。

## Step 4 — 逐提交并行核实（**不信 commit message**）

按提交派并行 subagent（本次 4 个：3 个 `!` 提交各一个 + 非 `!` 提交合一个）。每个 agent 的 prompt 必须包含：

1. **清单路径**（判断"是否命中"的唯一依据）
2. **兄弟的 feature 集**（本次 `features = ["computer"]` ⇒ agent 侧代码根本不编译，这一条直接让 #136 的影响面塌缩到零）
3. **你的怀疑假设 + 明确授权推翻它**
4. **强制 `file:line` 证据、禁止猜测、不确定必须明说**
5. **固定输出格式**：破坏性分级（编译期破坏 / 运行期静默变更 / wire 变更 / 无影响）+ 命中清单章节 + 事实 + 触发条件 + 影响 + 建议动作

> **第 3 点是关键。** 本次我怀疑 #143 命中 `resolve_settings`/`reconcile_governance`，agent 证伪了——真正命中的是 `validate_config`/`migrate_config`/`import_config`。**假设被推翻是这一步在正常工作的标志**，不是失败。

派 agent 的 prompt 结构可直接抄本次的四份（见 golden 示例报告的「附：本报告的验证方法」）。

## Step 5 — 反向查兄弟的 open issues + 替对方 grep

**这一步把报告从「合格」变成「有用」。**

**5a. 反向查 issue** —— 升级不只"打坏什么"，也"修好什么"：

```bash
gh issue list -R A2C-SMCP/<兄弟> --state open --limit 20
```

本次发现下游 #31「填写别名前缀后 MCP 启动失败」正是 #134 修的东西，**且定性需要修正**（原报的硬报错在他们当前基线上已被 #117 消掉，残留的是静默丢工具）。顺着 issue 里引用的 SDK issue 往上追（`#101 → #116 → #134/#117`），才能给出"你等的那个能力不会以你想的形态到来"这种结论。

**5b. 替对方 grep** —— 把"需自查"降级为"已定位到行号"：

```bash
git clone --depth 1 --branch <活跃分支> git@github.com:A2C-SMCP/<兄弟>.git <scratchpad>/client
```

成本极低、价值极高。本次把 agent 报告里所有「无法确定，需 client 自查」逐条查实，命中 2 处真问题（`contract_test.rs:304` 编译破坏、`debug.rs:450` 死读取），并排除 1 处（`SMCPTool` 零命中）。

## Step 6 — 合成报告

结构照 [golden 示例](resources/example-tfrobot-client-91b5758-to-98d75cc.md) 抄。要点：

- **按严重度排**，不按提交顺序：🔴 P0 编译期 → 🟡 P1 静默行为 → ✅ 已核实无影响 → 🎁 顺带发现的既存缺陷 → 📅 前瞻预警
- **每条给全**：旧 → 新、首次受影响 commit、触发条件、before/after 具体例子、是否需数据迁移、建议动作
- **「无影响」也要写**，并给理由 —— 它和"有影响"同等重要，避免对方过度反应
- **"顺带发现"单独成节**并标注非本次引入 —— 别混进升级影响面
- **前瞻预警**：扫一遍 open 的 `breaking-change` issue，提前告诉对方下一颗雷。本次预警 [#147](https://github.com/A2C-SMCP/rust-sdk/issues/147) 会给 `ProvenanceScope` 加变体，而对方的穷尽 match 无 fallback ⇒ 建议现在就加 `_ =>` 兜底，把未来的硬中断降级成软退化。

## Step 7 — 交付 Issue

```bash
gh issue create -R A2C-SMCP/<兄弟> \
  --title "[SDK 升级指导] a2c-smcp <base短hash> → <target短hash>（<分支>）：<一句话净结论>" \
  --body-file <报告路径> --label enhancement
```

标题必须带**两端 hash + 收件分支 + 净结论**（如"1 处编译期破坏 + 2 处静默行为变更"）——对方从通知列表就能判断优先级。

先确认权限：`gh api repos/A2C-SMCP/<兄弟> --jq '.permissions'`。

## Step 8 — 回填

把本次报告存进 `resources/` 作为新的示例（若比现有示例更典型则替换），并在 `MEMORY.md` 记一条。**清单基线的更新是兄弟的职责**（他们清单 §16 已有规则），不要替他们改。

---

## 常见陷阱

| 陷阱 | 后果 | 对策 |
| --- | --- | --- |
| 按 `!` 标记筛提交 | 漏掉真破坏（#132）、误报假破坏（#136） | 逐提交验代码 |
| 用版本号判断兼容性 | dev 期版本号不动，3 次破坏全漏 | 只认 commit hash |
| 信 commit message 的"零行为变化" | #132 自述纯改名，实含编译破坏 | 剔除注释行后看剩余 diff |
| 不核实兄弟实际 pin | 报告发错分支 | Step 2 |
| 无清单硬跑 | 退化成 changelog 复述 | Step 3 门禁 |
| 只报"坏消息" | 漏掉"你的 #31 被修好了" | Step 5a |
| 留一堆"需你自查" | 对方还得再干一遍活 | Step 5b 替他 grep |
| 把既存缺陷混进升级影响面 | 对方误以为是升级引入的 | 单独成节 + 标注 |
