# 本地测试与性能基线（#212）

本仓库 workspace 有 **80 个测试二进制**（`crates/*/tests` + 根 `tests/` + 各 crate lib 单测），
每条都是完整链接 rmcp / tokio / libdbus 等大依赖链的独立产物。全量跑一次在 40 分钟内不能
出现在日常循环里，因此 #212 落地了如下分级方案：**日常用子集 + 基线档全量 + 工具链加速**。

## 命令速查

| 场景 | 命令 | 说明 |
|------|------|------|
| 全量（快，默认） | `cargo test-ws` | **cargo-nextest**：并行执行 80 个测试二进制 + per-test 计时 |
| 全量（cargo 原生，CI 对齐） | `cargo test-ws-cargo` | 等价原 `cargo test --workspace` |
| 全量 + 全部 features | `cargo test-all` | 原生 cargo（变体慢，仅发版/CI 前跑） |
| e2e（ignored，需 e2e feature） | `cargo test-e2e` | 原生 cargo `-- --ignored` |
| 单 crate（治理 CLI 套件） | `cargo test-computer` | `smcp-computer --features cli` |
| 单 crate | `cargo test-agent` / `cargo test-server` | `smcp-agent` / `smcp-server-core` |
| 单套件/单用例 | `cargo nextest run -p smcp-computer -E 'test(foo)'` | nextest filter 语法 |
| 只编译不跑 | `cargo nextest run --workspace --no-run` | 检查编译 |
| 慢用例/疑似并行冲突 | `cargo nextest run -p smcp-server-core --test-threads 1` | 见下文「并行注意」 |

> **前置**：`cargo test-ws` 依赖 `cargo-nextest`（`cargo install cargo-nextest --locked`）。
> 未安装时用 `cargo test-ws-cargo` 获得原始行为。

## 优化项（#212 已落地）

1. **cargo-nextest 接入**（`test-ws` 别名指向它）：
   - 各测试二进制并行执行（默认 `test-threads = num-cpus`；本机 24 核 → 24 路并发，原生
     `cargo test` 是顺序跑二进制）。
   - per-test 计时：结尾 Summary 自带「最慢用例」列表，慢点可见。
   - nextest 默认不 fail-fast（与 cargo test 相反），一次跑完全量表。
2. **rust-analyzer 构建隔离**：`.vscode/settings.json` 设置 `rust-analyzer.cargo.targetDir =
   target/rust-analyzer`（相对 workspace 根）——IDE 常驻检查（本机曾 16 个进程）不再与
   命令行 cargo 争抢 `target/` 构建锁与磁盘 I/O（此前 `cargo test` 可因此卡 10+ 分钟）。
   代价：RA 需要独立首次全量检查（一次性）；产物仍在 target/ 内，`cargo clean` 一并清理。
   该配置仅对 VS Code 生效；RustRover / neovim 等编辑器（含 rust-analyzer 用户）需按同名
   设置项自行配置。
3. **测试剖面瘦身**：`Cargo.toml [profile.test] debug = "line-tables-only"` —— 测试二进制
   保留函数名/行号/栈回溯，去掉局部变量调试信息；编译与链接时间、产物体积显著下降。
   `cargo build`/`run` 的 dev 完整 debuginfo 不受影响（已提交 `aac90ee`）。
4. **链接加速（机器级，可选）**：
   - `brew install lld` 的 lld 与 brew 的 llvm 存在**版本错位**（本机实测 lld 20.1.8 vs
     llvm 21.1.5 → 链接 build script 直接崩溃），**仓库不提交** `-fuse-ld=lld`。
   - 推荐用 rustup 自带的 rust-lld（自洽 LLVM，支持 Mach-O，实测可用），在
     `~/.cargo/config.toml` 追加（本机已配置）：

     ```toml
     [target.aarch64-apple-darwin]
     linker = "<sysroot>/lib/rustlib/aarch64-apple-darwin/bin/rust-lld"
     # sysroot = $(rustc --print sysroot)，路径随工具链升级变化
     ```

5. **sccache**（机器级，可选）：`~/.cargo/config.toml [build] rustc-wrapper = "sccache"`。
   缓存的是「非增量」的依赖编译（多变体/多 feature 组合复用），与 incremental 互补；
   本机已接线，首次全量后命中率逐步上升。
6. **target/ 卫生**：APFS 上小文件爆炸（实测 1.34M 文件 / 190GB）会让 du/find/链接/fsync
   全链路退化——这是「40 分钟跑不完」的隐性元凶之一。**本机已自动化**：launchd 代理
   `com.jqq.cargo-sweep`（`~/Library/LaunchAgents/com.jqq.cargo-sweep.plist`）每周一 03:30
   对 `~/RustroverProjects` 全部 Cargo 项目执行
   `cargo-sweep sweep --maxsize 40GB --recursive <path>`（各项目 target/ 超出 40GB 时按最旧
   开始删，直到收缩）；日志 `~/Library/Logs/cargo-sweep.log`，手动验证：
   `launchctl kickstart -k gui/$(id -u)/com.jqq.cargo-sweep`，卸载：
   `launchctl bootout gui/$(id -u)/com.jqq.cargo-sweep`。其他机器手动清理：
   `cargo-sweep sweep --maxsize 40GB <project>` 或不得已时 `cargo clean`（全量冷启动后：
   基线 ≤ 60 分钟）。

## 并行注意

smcp-server-core 的 Socket.IO 集成测试对固定端口有依赖（CI 侧就是
`-- --test-threads=1` 防冲突）。nextest 默认 24 路并发下若出现 Bind 错误/flaky，
用 `cargo nextest run -p smcp-server-core --test-threads 1` 或按套件收敛。

## 基线（2026-08-27，`develop` @3b7e4bb，24 核/192GB macOS，target 清空后首次冷启动）

| 指标 | 值 |
|------|-----|
| 总墙钟 | **260.3s（4 分 20 秒）**：编译+链接 ≈ 190s，测试运行 67.8s |
| 用例 | 1686 run / 1686 pass / 40 skipped（e2e `--ignored`）/ 0 failed |
| 测试阶段并发 | 各二进制 24 路并行（默认 `test-threads = num-cpus`），无端口冲突 |
| 慢点（>5s） | `governance_snapshot` 60.2s/30.2s/30.2s、`test_mcp_integration` 30.1s/20.5s、`get_tools_test` 10.7s/5.5s、`socketio_interop` 8.1s/5.1s、`auth_dict_injection_test` 7.0s、`get_config_complete_test` 5.5s |

主要二进制测试耗和 Top 6（各二进制并行执行，故总墙钟远小于和；其余二进制单套件耗和
均在 1s 量级，未逐行列——漏列套件数合计 74，含根 `tests/` 与各 crate 集成套件）：

| 二进制 | 用例数 | 测试耗和 |
|--------|-------|----------|
| smcp-computer | 209 | 317.8s |
| smcp-server-core | 61 | 64.7s |
| smcp-server-hyper | 10 | 8.3s |
| a2c-smcp | 32 | 3.0s |
| smcp-agent | 72 | 2.3s |
| smcp | 7 | 0.1s |

> 测量条件：无并发 IDE 编译（残留进程已清）；依赖 rust-lld + sccache（冷） +
> `[profile.test] line-tables-only`。40+ 分钟旧态与本次差异的主因是 target/ 内
> **1.34M 小文件爆炸**（APFS 上 du/find/链接/fsync 全链路退化）已被清理，
> 其二是变异体缓存各自重编（sccache 接线后缓解）。

## 日常工作法

- 改小范围 → 单 crate/单套件 nextest，不跑全量。
- 改动跨多 crate → `cargo test-ws`（nextest，目标 < 15 分钟）。
- 发版/CI 前 → `cargo test-all` + `cargo test-e2e`（原生 cargo 形式与 CI 一致；CI 实跑
  `--features agent,computer,server`，与 test-all 变体近似）。
- 迭代中别让 IDE 的 rust-analyzer 与 cargo 同时打同一个 target/：RA 已隔离，
  命令行随意跑。
