# 设计：#107 — SDK-owned Config CRUD / Schema 与 Runtime 边界（Rust）

- **状态**：Draft（评估 + 拆解，未开工）
- **关联**：GitHub #107（收敛自 #105）；协议 `a2c-smcp-protocol` `computer-management/runtime-contract.md`（规范性）+ `sdk-api-guidance.md`（非规范）
- **根因动机**：`rust-sdk#96`（活跃集只在内存、重启即丢）；本设计补的是"runtime 改动不落盘 + 无统一 config 入口"两个洞
- **本文范围**：设计与子任务拆解；**不含实现码**

---

## 0. 一句话结论

#107 约 90% 不是新需求，而是**把协议已批准的 Runtime Contract 落到 Rust**。真正新增、真正难、协议又**故意不标准化**的，只有一件事：**运行时对 config 的增删改，落到哪个文件 / 哪个 scope**（#107 的 point #2）。协议 `sdk-api-guidance §9 非目标`明确写了 settings 文件名 / home 布局 / scope 写策略**都不标准化** → 这块是纯 SDK-local 决策，**不需要动协议**。

---

## 1. 决策（已与需求方确认）

| # | 决策点 | 结论 |
|---|---|---|
| D1 | **Inputs 归属** | **折中，对齐协议**：input **定义**留 SDK-owned config（协议 §2.1 把 `inputs` 列为 `ComputerConfig` 字段族）；仅 resolved **值 / secret** 走 client resolver，**SDK 不落盘明文**（拆掉 `inputs/value_store.rs` 的明文持久化）。需**订正 #107 验收项**，**不动协议**。 |
| D2 | **写目标消解** | **对称消解纯函数 + asset-class-aware**：`remove` → 删 origin scope，命中只读 scope（policy/flag）→ 返结构化错；`disable` → 写负向 override 到**固定 writable scope**（默认 local，复用现有"批准→local" §9.2）。 |
| D3 | **本轮产出** | 本设计文档（结构化设计 + 子任务拆解 + 依赖序），不写实现码。 |

---

## 2. 现状盘点（为什么要做）

### 2.1 runtime 改动根本不落盘

`computer.rs:1407 add_or_update_server` / `:1445 remove_server` 只改内存 `self.mcp_servers` + MCP manager，**不写任何 mcp.json**。重启即丢。这就是 `#96` 根因在 MCP 维度的翻版。

### 2.2 落盘另走一路（割裂）

CLI / `settings/store.rs` / `settings/installer.rs` 直接写物化文件与 settings.json。于是：**既没有"client 经 SDK 写"，也没有"runtime 改动持久化"这个接缝**——这才是 #107 要补的洞，而非重构现有写路径。

### 2.3 config 不是一个目录，是**四个根**

#107 的 `load_config(config_dir) -> snapshot` 暗含"一个目录 = 一份配置"，但真实是**多根多 scope 的 reconcile 投影**：

| scope | 根 | 文件 | 可写? |
|---|---|---|---|
| user | XDG `a2c/`（`scope.rs:resolve_user_config_dir`） | `settings.json` / `mcp.json` | ✅ |
| project | `<cwd>/.tfrobot/`（#98 进程 cwd 锚定） | `settings.json` / `mcp.json` | ✅ |
| local | `<cwd>/.tfrobot/` | `settings.local.json` / `mcp.local.json` | ✅ |
| flag | CLI `--flag` 注入 | （无文件） | ❌ 只读 |
| policy | per-platform managed dir（`policy.rs`） | `managed-*.json` | ❌ 只读 |
| （物化/意图） | SKILL Home（`store.rs:resolve_home`） | `known_marketplaces.json` / `installed_plugins.json` / `installed_plugins_intent.json` | ⚙️ 仅 reconcile 写、不可手编 |

> **关键结论**：`ComputerConfigSnapshot` **必须是 reconcile 投影**，不是单文件。CRUD 的读是 many→one（合并投影），写是 one→many（经 §5 写目标消解器 fan-out）。`config_dir` 参数只指定 **project-scope 锚点**（client 唯一合法拥有的那个目录），user/policy/home 为 env-resolved ambient——这样既守住"SDK 不维护 Computer registry"（只吃你递进来的目录），又保住 scope 模型。

---

## 3. 协议对齐地图（#107 ↔ 已有 contract）

| #107 提法 | 协议依据 | 落 Rust 现状 |
|---|---|---|
| `ComputerConfig{mcp,skills,marketplace,plugins,runtime}` | runtime-contract §2.1 字段族 | 分散在 mcp.json / settings.json / 物化文件；**无统一快照类型** |
| Config CRUD 10 API | sdk-api-guidance §1 能力族 | **无统一入口**（scope.rs/store.rs/installer.rs 各写各的） |
| `validate_config` 只 schema | sdk-api-guidance §3 + runtime-contract §4.1 | ✅ 现 `schema.rs` 已字段级容错、不探测环境 |
| install ⊥ enable 三态 | runtime-contract §2.4 | ✅ 已落地（`262605d`，见 memory `feature-102`） |
| disable = 保 config + 移出投影 | runtime-contract §2.4 / §3 / §5 | ⚙️ 元数据齐、写目标未统一（见 §6） |
| runtime handle（start/stop/status/list/execute…） | runtime-contract §4 + sdk-api-guidance §4 | ✅ **大半已在 `Computer` 上**（见 §7） |
| MCP env/secret 只存引用 | runtime-contract §5.9 | 部分（占位符定义在 mcp_config，明文值在 value_store/secret_store） |

**推论**：真正 greenfield 只有两块——(a) `ComputerConfig` 快照 + Config-CRUD；(b) §5 写目标消解器。其余是"包一层 + 补落盘 + 补 revision snapshot"。

---

## 4. `ComputerConfig` 快照 schema

**原则**：快照是**带 provenance 的 reconcile 投影**（每个实体携带 `origin` scope），使 client 能看清来源、且让 update/remove/disable 能消解写目标。

```
ComputerConfigSnapshot {
    version,
    revision,                    // 单调递增；供 client 判断是否需刷 UI / 同步 robot capability
    mcp:     McpConfigView,      // 合并 mcp.json 多 scope，每 server 带 origin（mcp_config.rs 已有 origin:142）
    inputs:  InputDefsView,      // ⚠️ D1：只 input **定义**，无明文值/secret（值走 resolver）
    skills:  SkillConfigView,    // skill home / discovery / enablement
    marketplace: MarketplaceGovView,  // known marketplaces + trust/strict policy
    plugins: PluginConfigView,   // installedPlugins 意图 + enabledPlugins(per-scope) + bundled 归属（派生自账本）
    runtime: RuntimeDefaults,    // timeout / cache / capability-revision 策略
    provenance: Map<EntityKey, SettingsScope>,  // 每实体的 origin，写目标消解输入
}
```

### 4.1 D1 落地：inputs 边界订正

- **留 SDK config**：`InputDef`（id / type / description / options / plugin-scope 前缀），进 `inputs` 字段族，可 CRUD、可 import/export（协议 §2.1）。
- **移出 SDK**：resolved 明文值、secret 明文——改由 `RuntimeOptions.input_resolver` / `secret_resolver` 注入（协议 §2.2 / §6）。
- **代码影响**：`inputs/value_store.rs`（明文落盘 XDG state）→ **退役其持久化**；`secret_store.rs`（keyring）保留为 resolver 的一种实现，但不属 SDK-owned config。
- **#107 验收项订正**：把"SDK config 中不包含 inputs" 改为"SDK config 含 input **定义**、不含 input **值 / secret 明文**"。

> **✅ S5 已落地（#112，`inputs/runtime_resolver.rs` + `computer.rs`）**：新增 D1 运行期注入契约
> `InputValueResolver` / `SecretValueResolver`（= `RuntimeOptions.input_resolver` / `secret_resolver`），经
> `Computer::with_input_resolver` / `with_secret_resolver` 注入；`KeyringSecretResolver` 把 keyring 降级为 **opt-in**
> secret resolver 实现（不再属 SDK-owned config）。`render_server_config` 全接入实时解析路径，解析序 **client
> resolver → env `A2C_INPUT_<ID>` → session（默认 / 自定义交互 / Command 执行）→ 定义默认值 → 结构化
> `ComputerError::InputResolution`**（复用暂存-仅引用才上抛的容忍语义，替换旧「解析失败→debug 日志→静默空串」反
> 模式；新增 `RenderError::InputUnresolved` 与 `InputNotFound`「保留原样」刻意区分）。**明文 value store 硬退役**：
> `inputs/value_store.rs` 已删除，非密钥值不再落盘（仅会话缓存）。**迁移**：旧 `input-values.json` 残留被**孤儿化**
> （不再读取）——用户升级后须经 resolver / env `A2C_INPUT_*` 重新提供该值（见 §12 R3；「无损迁移」验收项按用户决策
> 显式豁免，换取代码零残留 + 立即停止明文依赖）。

---

## 5. 写目标消解器（本设计的核心）

### 5.1 为什么不能"自动反推"

Reconcile 是 many→one **有损投影**，其逆**天然不唯一**。所以"与 Reconcile 对称"≠"自动推唯一文件"，而是：**一条确定性、纯函数、由 origin/scope 代码逻辑决定（不靠 per-entity 元数据）的写目标策略**。种子已在：`scope.rs` 的读合并/写回两套对称 customizer + `mcp_config.rs:142` 的 `origin`。

### 5.2 签名（纯函数、无 I/O）

```
enum WriteScope { User, Project, Local }          // 可写子集；Flag/Policy 只读
enum EditIntent { Upsert(Value), Remove, Disable, Enable }

fn resolve_write_target(
    entity:   &ConfigEntity,          // McpServer / Plugin / Marketplace / InputDef / GovernanceField ...
    intent:   EditIntent,
    snapshot: &ComputerConfigSnapshot,// 带 provenance(origin)
    anchors:  &ScopeAnchors,          // user dir / project dir / policy dir / home（env-resolved）
) -> Result<WritePlan, WriteTargetError>

struct WritePlan { file: PathBuf, scope: SettingsScope, op: WriteValue }  // 复用 scope.rs::WriteValue
enum WriteTargetError {
    ReadOnlyOrigin { entity, origin },   // remove/edit 命中 policy/flag
    Synthesized     { entity },          // plugin-bundled server：无可编辑文件
    // 多写 scope 声明 → 不报错，按 §5.3 规则确定性 fan-out
}
```

### 5.3 消解规则（asset-class-aware，对称语义表）

| intent | 语义 | 写目标（纯函数） | 只读 origin（policy/flag） |
|---|---|---|---|
| **Upsert** 新实体 | 声明它 | `config_dir` 对应 scope（默认 project）；caller 可显式覆盖 | — |
| **Upsert** 改已有 | 就地改声明 | origin scope（若可写） | `ReadOnlyOrigin`（policy 恒最高、改不动） |
| **Remove** | 让它不再被声明 | 删**所有可写 scope**中该实体的条目（真"删干净"） | `ReadOnlyOrigin`（origin=policy/flag 硬错，不 partial；#109 已拍板） |
| **Disable** | 我这层盖掉它 | 写负向 override 到**固定 disable-scope**（默认 local），**不动 origin** | 合法：override 高盖低 |
| **Enable** | 撤销压制 | 删除/翻正 disable-scope 的 override | 合法 |

> **disable ≠ remove** 是全设计最重要的一刀：remove 动"声明"（origin），disable 动"override"（固定 writable scope）。这让 disable 天然可逆、不碰声明，正对齐协议 §2.4（disable 保留 `installedPlugins` + 物化）。

---

## 6. Disable 模型（三 asset class 落三文件）

协议已把 disable 的**行为语义 + 元数据**定死，SDK 只需"用对开关、写对文件"。**无需改协议**。

| asset class | disable 写什么 | 文件 | scope | 协议 |
|---|---|---|---|---|
| **plugin** | `enabledPlugins[<plugin>@<mp>]=false` | settings(.local).json | disable-scope（默认 local） | §2.4 |
| **独立 MCP server** | 加入 `disabledMcpjsonServers`（信任门）**或**翻 server 自带 `disabled` 字段（`model.rs:199`） | settings(.local).json **或** mcp.json | disable-scope | §3 / §5.10 |
| **plugin-bundled MCP server** | 由**属主 plugin 的 enablement** 决定，**MUST NOT 走 project 信任门** | （无可编辑文件，经 plugin） | — | §5.10 |

> 协议 §5.10 两套正交开关：project 信任门（`enabledMcpjsonServers`/`disabledMcpjsonServers`/`enableAllProjectMcpServers`）vs 通用禁用（server 自带 `disabled` 字段，对 bundled 亦逻辑生效）。**bundled server 启停不得走信任门**——这正是消解器必须 asset-class-aware 的证据。

---

## 7. Runtime handle = 演进 `Computer`，不造平行类型

`Computer` 已实现 #107 runtime handle 的大半（`sdk-api-guidance §8` 建议演进 + compat wrapper，别开第二类型）：

| #107 runtime API | 现有 `Computer` 方法 | 缺口 |
|---|---|---|
| `list_mcp_servers` / `get_mcp_server` | `list_mcp_servers` / `list_mcp_servers_with_metadata`（`:1952/:1982`） | 补 runtime status 汇总 + revision |
| `start/stop_mcp_server` | `start/stop_mcp_client`（`:2042/:2058`） | — |
| `install/enable/disable/uninstall_plugin` | 同名 | ✅ **S6 已落地（#113）**：enable/disable scope 由**安装记录消解**（`resolve_plugin_install_scope`，非恒定 user）+ 成功后 bump config revision + emit |
| `add/refresh/remove marketplace` | 同名（`:699–:736`） | — |
| `execute_tool` / `cancel` | `execute_tool[_cancellable]` / `acancel_tool`（`:1690–:1918`） | — |
| `connect/disconnect/join/leave` | 同名（`:2143–:2226`） | — |
| `status()` / `subscribe_events()` | ✅ **S7 已落地（#114）**：`status.rs` 补 `ComputerStatusSnapshot` + `broadcast` event stream + 分离单调 revision | — |
| `add_or_update_server`/`remove_server` | 同名 | ✅ **S6 已落地（#113）**：经 S2 消解器 + S3 执行器落盘到 project scope → 内容真变才 bump config revision → 运行期物化 |

**核心接线**：所有 mutate 方法从"只改内存"改为"经 Config-CRUD + 写目标消解器落盘 → reload 投影 → bump revision → emit update"。

> **✅ S7 已落地（#114，`status.rs`）**：新增 [`crate::status`] 模块——`RuntimeStatus`（`Arc` 跨 clone 共享）持
> ① `LifecycleState`（协议 §3 状态的 Rust 映射，`AtomicU8` 无锁，serde snake_case 对齐协议用词）；② **分离**的两个
> 单调 revision（`config_revision` ⊥ `capability_revision`，`AtomicU64`，§12 R2）；③ 公开诊断（`last_error` /
> `degraded_reason`）；④ `tokio::sync::broadcast` 事件通道（`ComputerEvent`：LifecycleChanged / ConfigRevisionBumped /
> CapabilityRevisionBumped）。`Computer` 新增 `status()`（cheap 非阻塞快照：状态/revision/诊断 + 内存投影计数 MCP/工具/
> SKILL，**不做 ledger IO**——plugin/marketplace 明细仍走 `list_mcp_servers_with_metadata`）、`subscribe_events()`、
> `config_revision()`/`capability_revision()`/`lifecycle_state()`。**生命周期迁移接线**：boot（Starting→Started，
> marketplace 部分失败→Degraded+诊断）/ connect（Connected）/ join（JoinedOffice）/ leave（Connected）/ disconnect
> （Started）/ shutdown（Shutdown）；**capability revision bump** 于 boot 与 start/stop MCP（工具投影变化，§12 R2）。
> **shutdown 闸门**（契约 §4.7）：`enter_shutdown` 发唯一终态事件后闸断——此后不再发 stale 事件、bump 降 no-op。
> `config_revision` 的 mutate-bump 入口 `bump_config_revision()` 已备（S6 落盘成功后调用；S7 落地时暂无生产调用者）。

> **✅ S6 已落地（#113，`computer.rs`）**：runtime mutate 落盘接线（补 #96 洞）。**核心=区隔两类写**：抽
> `pub(crate) mount_server`/`mount_rendered`/`unmount_server`（**仅运行期物化**：render+manager+内存投影+capability
> bump+emit，**不落盘**）——**治理物化**路径（`CliMcpHooks::register_server`/`remove_server`、`approval::mount`、
> `ReplTeardown`、installer 级联）改指向之，使 ledger 拥有的 bundled server **不被写进 project `mcp.json`**（否则卸载
> 后孤儿化 / 每次 boot remount 重写用户配置）。公开 `add_or_update_server`/`remove_server` = 经 **S2 消解器 + S3 执行器**
> （`update_config`）落盘到 **project scope**（新 server；改已有落 origin scope）→ **内容真变才** bump config revision
> （§12 R2；no-op/幂等 mutate 不虚假 bump）→ 运行期物化。**D1 安全**：落盘**原始** `server`（保留 `${input:*}` 引用），
> **绝不**落渲染后明文/secret；render **仅一次**（`mount_rendered` 复用，避免 resolver 副作用放大）。**跨 SDK 保真**：
> 落盘前 `canonicalize_persist_body` 剥内嵌 `name`（map key 即身份）+ `type` 判别符归协议 §9.1 规范小写
> （`Stdio/Sse/Http`→`stdio/sse/streamable`，对齐 Python `Literal`；读端加 `alias="streamable"` 保往返）。**config_dir
> seam**：新增 `with_config_dir`（缺省进程 cwd，#98 project 锚点）。**plugin scope**：`enable/disable_plugin` scope 缺省
> 时从 ledger 安装记录 `record.scope` 消解（`resolve_plugin_install_scope`，非恒定 user；installer 层刻意不回查）+ 成功后
> bump config revision + emit。新增 `ComputerError::ConfigPersist`（400，无 secret）。

---

## 8. `validate_config` 边界（= 协议 §4.1）

只做 **schema validation**（现 `schema.rs` 已是此姿态）：version 受支持 / 必填 section / ID 唯一合法 / enum 合法 / 引用**语法**合法。**不**做：secret 可解析、文件存在、marketplace 可达、plugin 可下载、MCP 可启动——这些归 runtime preflight / diagnostics / start。

> **✅ S4 已落地（#111，`config/{validate,portability}.rs`）**：`validate_config(&ProjectConfigDoc)` 复用运行期
> 校验器（`schema::validate_settings` + `mcp_config::{validate_server,validate_input}`，后二者提为 `pub(crate)`）
> 逐文件产报告，**零环境探测**；唯一刻意更严=同文件重复 input id（§8「ID 唯一」，loader 静默去重）。
> `migrate_config` = **幂等形态规范化**（意图层 versionless，**不发明 version 字段**）：settings 采 loader cleaned 形
> （有损：移除 loader 本就忽略的畸形条目，运行行为无损）、mcp body 逐字保留、只写内容真变的文件。
> `export_config`/`import_config` 作用于 `ProjectConfigDoc`，**双管脱敏**：① 丢 client-owned 面（`*.local.json` 整层
> + server `envFile` 机器本地路径）；② **分段脱敏** mcp secret 面（stdio `env` / sse·http `headers` 值 + `url`
> 内联 userinfo + password 输入 `default`）——逐字保留合法闭合可识别的 `${input:*}`/`${env:*}` 引用、其余每段字面
> 抹为 `${REDACTED}`（**欠脱敏是危险方向**：整值含任何非引用字面即抹，不因串里出现 `${` 就整值放行）。best-effort：
> `command`/`args` 密码 flag、url 敏感 query 属未覆盖面（文档如实声明）。import 先脱敏后校验后落盘。

---

## 9. 子任务拆解与依赖序（DAG）

```
S1 ComputerConfig 快照 schema + provenance ─┐
                                            ├─> S3 Config-CRUD(init/load/save/update/delete/duplicate)
S2 写目标消解器(纯函数+单测) ───────────────┘        │
                                                     ├─> S4 validate/migrate/import-export(schema-only；不导 secret/client 字段)
S1 ──> S5 inputs 边界订正(D1：退役明文 value_store 持久化；def 进快照；值走 resolver)
S3+S7 ──> S6 runtime 落盘接线(add_or_update/remove_server 等 mutate → 经消解器落盘 → reload → bump revision)
S1 ──> S7 ComputerStatusSnapshot + revision + event stream(补 status/subscribe)
S6 ──> S8 连接态 → robot capability 同步(revision 驱动 server:update_*)
```

**建议顺序**：`S1 → S2 →（S3, S5 并行）→ S4 → S7 → S6 → S8`。**全部子任务已落地（#108/#109/#110/#111/#112/#113/#114/#115）**。

> **S8（#115）落地要点**（`tests/config_runtime_regression.rs`，纯集成回归 + 吸收 S6 审查遗留 R1）：
> - **集成回归**（11 例）逐条守护 #107 验收：CRUD roundtrip（runtime mutate→落盘→fresh reload）、migration 幂等、
>   `validate_config` schema-only（不探测环境）、D1 inputs 边界 + import/export 脱敏（引用留/明文去/丢 local 层）、
>   **disable≠remove**（override 落 `*.local.json`、声明不动、不 bump config revision）、只读 policy origin→`ReadOnlyOrigin`
>   整批零落盘、runtime mutate→`config_revision ⊥ capability_revision` + `ComputerEvent` 广播、**Http→`streamable`
>   全链路往返**（S6 审查 R2）、enable/disable→resolved-scope 落盘（非恒定 user）、lifecycle 不变量（boot/shutdown
>   终态 + 未连接 gate）、跨-SDK 快照 schema 桩（python 未实现前守顶层形态漂移）。
> - **R1（S6 审查遗留，方案 A）落地**：enable/disable_plugin 从**无条件 bump** 改为**内容真变才 bump**——installer
>   `apply_enabled_plugin_write`/`write_enabled_plugin`/`enable_plugin`/`disable_plugin` 改返 `Result<bool>`（据实际写盘
>   结果），Computer wrapper 仅 `changed` 时 bump config revision + `emit_update_config`。**false-negative 安全**（写了即真
>   变、no-op 跳写盘），对齐 add/remove 的对称语义。隔离审查（Step7）无 🔴，两 🟡（回归测试自身隔离硬化 env 注入 /
>   补 enable wrapper 幂等断言）+ 一 🟢（rustdoc 同步）全修。
**先落地价值最高**：S1 + S2（地基）与 S6（补 `#96/#102` 同源的"改动落盘"洞）。

| 子任务 | 对应 #107 建议拆分 | 依赖 | 是否 greenfield |
|---|---|---|---|
| S1 快照 schema + provenance | 1 | — | 是 |
| S2 写目标消解器 | （新增，#107 未显式列） | — | **是（核心）** |
| S3 Config CRUD | 2 | S1,S2 | 是 |
| S4 validate/migrate/import/export ✅#111 | 3,4 | S3 | 半（validate 已有底子）|
| S5 inputs 边界订正 ✅#112 | （D1 派生） | S1 | 改造 |
| S6 runtime 落盘接线 ✅#113 | 5,7 | S3,S7 ✅ | 接线（已落地）|
| S7 status snapshot + events ✅#114 | 7 | S1 | 半（已落地）|
| S8 集成回归守护 ✅#115 | 8 | S4,S5,S6,S7 | 纯测试（末端汇聚）|

---

## 10. 对 #107 的订正建议

1. **inputs（验收项）**：SDK config 含 input **定义**，不含 input **值 / secret 明文**（D1）；import/export 同理只带定义。
2. **`config_dir` 语义**：显式说明它是 **project-scope 锚点**，snapshot 是**跨 user/project/local/policy/home 的 reconcile 投影**（非单文件），否则实现会误建单文件 config、砸掉 scope 模型（§2.3）。
3. **补一条写目标消解验收项**：runtime edit/remove/disable 的落盘 scope 由**纯函数**（origin + scope 规则）确定；命中只读 scope 返结构化错；disable 写 override 而非改声明（§5）。
4. **runtime handle**：明确"演进 `Computer`"而非新类型，避免双实现漂移。

---

## 11. 协议影响

**无需改协议。** 已选 D1「折中对齐协议」，inputs 定义本就在 contract §2.1；写目标 / 文件名 / home 布局是 `sdk-api-guidance §9` 明列的**非目标**。唯一会触发"协议先行"（`add-feature` 门）的是"严格按 #107 把 input 定义也逐出 config"——**该分支已被 D1 否掉**。

---

## 12. 风险 / 待议

- **R1 多 scope remove 策略**：✅ **已拍板（#109）**=删**所有可写 scope**（真删干净）；origin=policy/flag → `ReadOnlyOrigin` 硬错（非 partial）。✅ **执行器已落地（#110，`config/executor.rs`）**：no-change 判定用**精确语义比对**（`is_no_change`/`strip_fresh_scaffold`——只剥「本次写新物化、且在 existing 缺失/非对象」的空对象脚手架，**不**对称剥两侧，故既不凭空建 `{"servers":{}}`、也不误跳磁盘上空对象值 server 的真实删除）。多文件 fan-out 落盘前加 pre-flight 只读探测（corrupt/IO），收窄半落盘窗口。
- **R2 revision 语义**：✅ **已拍板并落地（#114 S7）**=**分离**两个独立单调计数（`config_revision` ⊥ `capability_revision`），因 config 改不一定改 capability（如 disable 一个本未激活的 server）。`status.rs` 的 `RuntimeStatus` 各持一个 `AtomicU64`；capability 于 boot / MCP start·stop bump，config 于 S6 mutate 落盘 bump。
- **R3 value_store 退役的兼容**：✅ **已拍板（#112，用户决策=硬退役）**=删 `inputs/value_store.rs` 读写，旧 `input-values.json` 残留**孤儿化**（不再读取）。「无损迁移」验收项**显式豁免**（换取代码零残留 + 立即停止明文依赖）；升级迁移路径=用户经 `RuntimeOptions.input_resolver` 或 env `A2C_INPUT_<ID>` 重新提供该值。`secret_store.rs`（keyring，加密非明文）**保留**为 `KeyringSecretResolver`（opt-in secret resolver），非退役目标。
- **R4 duplicate/import 跨机**：协议 §5.8「install path 非权威、boot 重校验」——duplicate 到新 `config_dir` 后物化账本须重建，不可照搬 installPath。
