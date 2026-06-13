#!/usr/bin/env bash
# 完整链路 UAT 编排 / full-protocol UAT orchestration。
#
# 三进程（Server + Computer + Agent）真实 socket.io 链路，驱动：
#   - full-protocol：F-02/F-05/F-08/F-09/F-10/F-11/F-12
#   - skill-discovery：D-05（渐进披露）
#   - resource-discovery：R-01/R-02/R-03(4014)/R-04(4015)        [#82 修复后迁入]
#   - blob-transfer：B-01 inline / B-02/B-03/B-04 二进制 sideband  [#82 修复后迁入]
#   - error-codes：E-01(4016)/E-03(4014)/E-04(4017)/E-08(4018)/E-11(404 #92)  [#82 修复后迁入]
# Agent 驱动器为 `crates/smcp-agent/examples/e2e_test_agent.rs`（env `SMCP_TEST_MODE=all`）。
#
# 依赖 #80 修复（版本握手只 gate 开局握手，放行带 sid 的后续 polling）——否则 polling-first
# 客户端无法连接，本脚本会在 join/agent 步超时。
#
# 用法：bash .claude/skills/UAT/resources/full-protocol-uat.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"   # rust-sdk
# UAT 放行鉴权 server（example，非生产二进制）——等价 python `_local_sync_server.py`，
# 让三端无需共享密钥即可真实端到端连接。生产 `smcp-server-hyper` 二进制 reject-all 默认、
# 无配 secret 入口，不能直接用于 UAT 编排；见 examples/uat_test_server.rs。
SRV="$ROOT/target/debug/examples/uat_test_server"
COMP="$ROOT/target/debug/smcp-computer"
AGENT="$ROOT/target/debug/examples/e2e_test_agent"
MCP="$ROOT/tests/v022-mcp-server/index.js"            # echo + sleep + gen_image + window 资源
MCP_NORES="$ROOT/tests/no-resources-mcp-server/index.js"  # 无 resources 能力（R-04 4015 用）
SEED="$ROOT/.claude/skills/UAT/resources/seeds/_common/valid-skill-pkg"
OFFICE="proto-uat-office"
# Computer 在 office 内的注册名。`smcp-computer run` 无 --name flag，注册名恒为
# REPL 内置默认 "friday_hands"（cli/mod.rs:539，REPL `socket join` 的 name 参数对注册名无效）。
# Agent 必须用此真实注册名路由 client:* 请求，否则 server 返回 404 computer-not-found。
COMPUTER_NAME="friday_hands"
SKILL_NAME="valid-skill-pkg"

U="$(mktemp -d -t a2c-uat-fp.XXXXXX)"
export A2C_SKILL_HOME="$U/skill-home" XDG_CONFIG_HOME="$U/config"
mkdir -p "$A2C_SKILL_HOME/user/$SKILL_NAME" "$XDG_CONFIG_HOME"

# ⚠️ 本地 HTTP 代理旁路：Agent/Computer 的 reqwest engine.io 客户端会遵循 http_proxy，
# 把 127.0.0.1 的连接经本地代理转发 → EngineIO Error。对 loopback 强制 no_proxy。
export no_proxy="127.0.0.1,localhost,::1" NO_PROXY="127.0.0.1,localhost,::1"

# 前置：三个二进制 + MCP fixture
for b in "$SRV" "$COMP" "$AGENT"; do
  [[ -x "$b" ]] || { echo "missing binary: $b"; echo "build: cargo build -p smcp-server-hyper --example uat_test_server && cargo build -p smcp-computer --features cli && cargo build -p smcp-agent --example e2e_test_agent"; exit 1; }
done
[[ -f "$MCP" ]] || { echo "missing MCP fixture: $MCP"; exit 1; }
[[ -f "$MCP_NORES" ]] || { echo "missing MCP fixture: $MCP_NORES"; exit 1; }

# D-05 种子：user 源 skill（Computer boot_up 就地发现 <home>/user/<name>/SKILL.md）
cp -R "$SEED"/. "$A2C_SKILL_HOME/user/$SKILL_NAME/"

# MCP 配置（run --config 期望顶层 "servers" 数组）。两个 stdio server：
#   - "echo"        : v022 fixture（echo/sleep/gen_image + window:// 资源）—— get_resources R-01/R-02、blob
#   - "no-resources": 仅 tools 能力 —— get_resources R-04（4015 能力预检）
cat > "$U/cfg.json" <<EOF
{"servers":[
  {"type":"stdio","name":"echo","disabled":false,"server_parameters":{"command":"node","args":["$MCP"]}},
  {"type":"stdio","name":"no-resources","disabled":false,"server_parameters":{"command":"node","args":["$MCP_NORES"]}}
]}
EOF

FIFO="$U/comp_in"; mkfifo "$FIFO"
cleanup(){ echo "quit" >&3 2>/dev/null; exec 3>&- 2>/dev/null; kill ${COMPPID:-} ${SRVPID:-} 2>/dev/null; wait 2>/dev/null; rm -rf "$U"; }
trap cleanup EXIT INT TERM

# ── 1) Server（取临时端口，从日志解析实际端口）─────────────────────────────
echo "== 1) server =="
"$SRV" "127.0.0.1:0" > "$U/server.log" 2>&1 & SRVPID=$!
PORT=""
for _ in $(seq 1 40); do
  PORT=$(grep -oiE "listening on 127.0.0.1:[0-9]+" "$U/server.log" | grep -oE "[0-9]+$" | head -1)
  [[ -n "$PORT" ]] && break
  sleep 0.3
done
[[ -n "$PORT" ]] || { echo "server failed to report port"; cat "$U/server.log"; exit 1; }
echo "server up on $PORT"

# ── 2) Computer（--url 自动连；REPL 经 FIFO 驱动 start + join）────────────────
echo "== 2) computer =="
"$COMP" --url "http://127.0.0.1:$PORT" --approve-all-mcp run --config "$U/cfg.json" < "$FIFO" > "$U/computer.log" 2>&1 & COMPPID=$!
exec 3>"$FIFO"
sleep 4
echo "start all" >&3; sleep 2
echo "socket join $OFFICE $COMPUTER_NAME" >&3; sleep 3
if grep -qiE "join" "$U/computer.log"; then echo "computer joined office"; else echo "WARN: computer join 未在日志确认（继续，由 agent 调用兜底判定）"; fi

# ── 3) F-05：版本不兼容 4008（curl，独立于 agent 驱动）───────────────────────
echo "== 3) F-05 版本握手 4008 (curl) =="
F05=$(curl -s -i "http://127.0.0.1:$PORT/socket.io/?EIO=4&transport=polling&a2c_version=0.1.0" | head -1)
if echo "$F05" | grep -q " 400"; then echo "UAT_RESULT: PASS F-05 ($F05)"; else echo "UAT_RESULT: FAIL F-05 ($F05)"; fi

# ── 4) Agent 全用例（F-02/08/09/10/11/12 + D-05）──────────────────────────────
echo "== 4) agent: all modes =="
RUST_LOG=info SMCP_SERVER_URL="http://127.0.0.1:$PORT" SMCP_OFFICE_ID="$OFFICE" \
  SMCP_AGENT_ID="agent1" SMCP_COMPUTER="$COMPUTER_NAME" SMCP_SKILL_NAME="$SKILL_NAME" \
  SMCP_TEST_MODE="all" "$AGENT" > "$U/agent.log" 2>&1
AG_EXIT=$?

# ── 5) 结果汇总 ─────────────────────────────────────────────────────────────
# 注：#83（smcp-computer run 缺 boot_up）修复后，skill_disclosure/blob/errors 三场景已端到端转绿，
# 故恢复严格门控（任一 FAIL 即硬失败）——保留豁免会掩盖未来 boot_up/子系统回归。
echo "== 结果汇总 =="
grep -E "UAT_RESULT:" "$U/agent.log" || true
echo "UAT_RESULT: F-05 (见上 curl)"
PASS=$(grep -cE "UAT_RESULT: PASS" "$U/agent.log")
FAILN=$(grep -cE "UAT_RESULT: FAIL" "$U/agent.log")
echo "agent exit=$AG_EXIT  agent-PASS=$PASS  agent-FAIL=$FAILN"

if echo "$F05" | grep -q " 400"; then :; else FAILN=$((FAILN+1)); fi

if [[ "$FAILN" -gt 0 || "$AG_EXIT" -ne 0 ]]; then
  echo "---- server.log (tail) ----"; tail -30 "$U/server.log"
  echo "---- computer.log (tail) ----"; tail -30 "$U/computer.log"
  echo "---- agent.log (tail) ----"; tail -40 "$U/agent.log"
  echo "FULL-PROTOCOL UAT: ❌ FAIL"; exit 1
fi
echo "FULL-PROTOCOL UAT: ✅ PASS"
