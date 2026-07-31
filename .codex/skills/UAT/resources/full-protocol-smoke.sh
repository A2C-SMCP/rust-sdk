#!/usr/bin/env bash
# 完整链路冒烟（F-01 连接·加入 + F-08 get_tools）。
#
# 用法：bash .codex/skills/UAT/resources/full-protocol-smoke.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"   # rust-sdk
SRV="$ROOT/target/debug/examples/uat_test_server"
COMP="$ROOT/target/debug/smcp-computer"
AGENT="$ROOT/target/debug/examples/e2e_test_agent"
U="$(mktemp -d -t a2c-uat-fp.XXXXXX)"
export A2C_SKILL_HOME="$U/skill-home" XDG_CONFIG_HOME="$U/config"
export no_proxy="127.0.0.1,localhost,::1" NO_PROXY="127.0.0.1,localhost,::1"
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"

# 前置：三个二进制需已编译
for b in "$SRV" "$COMP" "$AGENT"; do
  [[ -x "$b" ]] || { echo "missing binary: $b"; echo "先跑: cargo build -p smcp-server-hyper --example uat_test_server && cargo build -p smcp-computer --features cli && cargo build -p smcp-agent --example e2e_test_agent"; exit 1; }
done

cat > "$U/cfg.json" <<EOF
{"servers":{"echo":{"type":"stdio","disabled":false,"server_parameters":{"command":"node","args":["$ROOT/tests/echo-mcp-server/index.js"]}}}}
EOF
FIFO="$U/comp_in"; mkfifo "$FIFO"
cleanup(){ echo "quit" >&3 2>/dev/null; exec 3>&- 2>/dev/null; kill ${COMPPID:-} ${SRVPID:-} 2>/dev/null; wait 2>/dev/null; rm -rf "$U"; }
trap cleanup EXIT INT TERM

echo "== 1) server =="
"$SRV" "127.0.0.1:0" > "$U/server.log" 2>&1 & SRVPID=$!
PORT=""
for _ in $(seq 1 40); do
  PORT=$(grep -oiE "listening on 127.0.0.1:[0-9]+" "$U/server.log" | grep -oE "[0-9]+$" | head -1)
  [[ -n "$PORT" ]] && break
  sleep 0.3
done
[[ -n "$PORT" ]] || { echo "server failed"; cat "$U/server.log"; exit 1; }
echo "server up on $PORT"

echo "== 2) computer (FIFO 驱动 REPL；需 --approve-all-mcp 才初始化 MCP manager) =="
"$COMP" --url "http://127.0.0.1:$PORT" --approve-all-mcp run --mcp-config "$U/cfg.json" < "$FIFO" > "$U/computer.log" 2>&1 & COMPPID=$!
exec 3>"$FIFO"
sleep 4
echo "start all" >&3; sleep 2
echo "socket join proto-uat-office friday_hands" >&3; sleep 3
echo "-- computer.log tail --"; tail -8 "$U/computer.log"

echo "== 3) agent get_tools =="
RUST_LOG=info SMCP_SERVER_URL="http://127.0.0.1:$PORT" SMCP_OFFICE_ID=proto-uat-office SMCP_AGENT_ID=agent1 \
  SMCP_COMPUTER=friday_hands SMCP_TEST_MODE=get_tools \
  "$AGENT" > "$U/agent.log" 2>&1
AG_EXIT=$?
echo "-- agent.log --"; grep -iE "got .*tools|echo|error|timeout|joined" "$U/agent.log" | head

if [[ "$AG_EXIT" -eq 0 ]] && grep -q "UAT_RESULT: PASS get_tools" "$U/agent.log"; then
  echo "SMOKE: ✅ PASS"
else
  echo "---- server.log ----"; tail -20 "$U/server.log"
  echo "---- computer.log ----"; tail -20 "$U/computer.log"
  echo "---- agent.log ----"; tail -30 "$U/agent.log"
  echo "SMOKE: ❌ FAIL"
  exit 1
fi
