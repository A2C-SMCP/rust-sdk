#!/usr/bin/env bash
# 完整链路冒烟（F-01 连接·加入 + F-08 get_tools）骨架脚本。
#
# ⚠️ 状态：⏳ 尚未跑通——见文末「已知阻塞点」。脚本结构正确，可作为后续完整链路
#    UAT 的起点；阻塞点解决后即可作为 full-protocol 的自动化入口。
#
# 用法：bash .claude/skills/UAT/resources/full-protocol-smoke.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"   # rust-sdk
SRV="$ROOT/target/debug/smcp-server-hyper"
COMP="$ROOT/target/debug/smcp-computer"
AGENT="$ROOT/target/debug/examples/e2e_test_agent"
PORT="${PORT:-18931}"
U="$(mktemp -d -t a2c-uat-fp.XXXXXX)"
export A2C_SKILL_HOME="$U/skill-home" XDG_CONFIG_HOME="$U/config"
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"

# 前置：三个二进制需已编译
for b in "$SRV" "$COMP" "$AGENT"; do
  [[ -x "$b" ]] || { echo "missing binary: $b"; echo "先跑: cargo build -p smcp-server-hyper && cargo build -p smcp-computer --features cli && cargo build -p smcp-agent --example e2e_test_agent"; exit 1; }
done

cat > "$U/cfg.json" <<EOF
{"type":"stdio","name":"echo","disabled":false,"server_parameters":{"command":"node","args":["$ROOT/tests/echo-mcp-server/index.js"]}}
EOF
FIFO="$U/comp_in"; mkfifo "$FIFO"
cleanup(){ echo "quit" >&3 2>/dev/null; exec 3>&- 2>/dev/null; kill ${COMPPID:-} ${SRVPID:-} 2>/dev/null; wait 2>/dev/null; rm -rf "$U"; }
trap cleanup EXIT INT TERM

echo "== 1) server =="
"$SRV" "127.0.0.1:$PORT" > "$U/server.log" 2>&1 & SRVPID=$!
sleep 1.5; grep -qi "listening" "$U/server.log" && echo "server up" || { echo "server failed"; cat "$U/server.log"; exit 1; }

echo "== 2) computer (FIFO 驱动 REPL；需 --approve-all-mcp 才初始化 MCP manager) =="
"$COMP" --url "http://127.0.0.1:$PORT" --approve-all-mcp run --config "$U/cfg.json" < "$FIFO" > "$U/computer.log" 2>&1 & COMPPID=$!
exec 3>"$FIFO"
sleep 4
echo "start all" >&3; sleep 2
echo "socket join proto-uat-office test-computer" >&3; sleep 3
echo "-- computer.log tail --"; tail -8 "$U/computer.log"

echo "== 3) agent get_tools =="
SMCP_SERVER_URL="http://127.0.0.1:$PORT" SMCP_OFFICE_ID=proto-uat-office SMCP_AGENT_ID=agent1 SMCP_TEST_MODE=tool_call \
  "$AGENT" > "$U/agent.log" 2>&1
echo "-- agent.log --"; grep -iE "got .*tools|echo|error|timeout|joined" "$U/agent.log" | head

if grep -qi "Got .* tools" "$U/agent.log"; then echo "SMOKE: ✅ PASS"; else echo "SMOKE: ❌ (见已知阻塞点)"; fi

# ── 已知阻塞点（2026-06-11，develop-v0.2.2）────────────────────────────────────
#  1) Computer/Agent 经独立进程连 server 二进制时，socket.io EngineIO 握手失败：
#       computer.log: "Socket.IO client not connected"
#       agent.log:    Connection("Failed to connect: EngineIO Error")
#     现有 e2e 集成测试是【同进程】(TcpListener + run_server + AsyncSmcpAgent) 覆盖协议，
#     未经独立进程的 engine.io 握手；二进制级握手需排查 transport/namespace(/smcp)/path。
#  2) MCP Manager 仅在 --approve-all-mcp（或逐项批准）后初始化；否则 `start all` 报
#     "MCP Manager not initialized"。脚本已加 --approve-all-mcp。
#  3) e2e_test_agent example 当前只做 connect/join/get_tools(F-08)；F-02/03/05/06/07/
#     09/10/11/12 需扩展该 example 或跨语言复用 python-sdk agent_protocol_driver.py。
