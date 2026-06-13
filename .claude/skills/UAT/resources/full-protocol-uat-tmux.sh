#!/usr/bin/env bash
# 完整链路 UAT —— **tmux 正式编排** / full-protocol UAT, official tmux orchestration.
#
# 与 full-protocol-uat.sh 同覆盖（F-02/05/08/09/10/11/12 + D-05 + resource-discovery R-* +
# blob-transfer B-* + error-codes E-*），但用**真实 tmux 多 window**协调三进程，对齐 UAT
# skill 文档「完整链路场景用 tmux 终端自动化」。三个 window 全程存活，可随时 attach 观察：
#
#   tmux attach -t a2c-uat-fp      # 切 window：Ctrl-b 0/1/2（server/computer/agent）
#
# 设计：tmux send-keys 发命令 + 轮询 capture-pane 等标记（非定长 sleep）；端口锚定 "listening on"
# 行解析（避免误取客户端临时端口）；Agent 输出 tee 到文件供 grep 判定。失败保留 session 供调试，
# 成功自动 kill。用法：bash .claude/skills/UAT/resources/full-protocol-uat-tmux.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
SRV="$ROOT/target/debug/examples/uat_test_server"
COMP="$ROOT/target/debug/smcp-computer"
AGENT="$ROOT/target/debug/examples/e2e_test_agent"
MCP="$ROOT/tests/v022-mcp-server/index.js"
MCP_NORES="$ROOT/tests/no-resources-mcp-server/index.js"
SEED="$ROOT/.claude/skills/UAT/resources/seeds/_common/valid-skill-pkg"
OFFICE="proto-uat-office"; COMPUTER_NAME="friday_hands"; SKILL_NAME="valid-skill-pkg"
SESSION="a2c-uat-fp"

command -v tmux >/dev/null || { echo "missing tmux"; exit 1; }
for b in "$SRV" "$COMP" "$AGENT"; do
  [[ -x "$b" ]] || { echo "missing binary: $b"; echo "build: cargo build -p smcp-server-hyper --example uat_test_server && cargo build -p smcp-computer --features cli && cargo build -p smcp-agent --example e2e_test_agent"; exit 1; }
done
[[ -f "$MCP" && -f "$MCP_NORES" ]] || { echo "missing MCP fixture"; exit 1; }

U="$(mktemp -d -t a2c-uat-tmux.XXXXXX)"
export A2C_SKILL_HOME="$U/skill-home" XDG_CONFIG_HOME="$U/config"
export no_proxy="127.0.0.1,localhost,::1" NO_PROXY="127.0.0.1,localhost,::1"
mkdir -p "$A2C_SKILL_HOME/user/$SKILL_NAME" "$XDG_CONFIG_HOME"
cp -R "$SEED"/. "$A2C_SKILL_HOME/user/$SKILL_NAME/"
printf '%s\n' "{\"servers\":[{\"type\":\"stdio\",\"name\":\"echo\",\"disabled\":false,\"server_parameters\":{\"command\":\"node\",\"args\":[\"$MCP\"]}},{\"type\":\"stdio\",\"name\":\"no-resources\",\"disabled\":false,\"server_parameters\":{\"command\":\"node\",\"args\":[\"$MCP_NORES\"]}}]}" > "$U/cfg.json"
# 各 window 共用 env（tmux 子 shell 不继承本进程 export）。
printf '%s\n' "export A2C_SKILL_HOME=$A2C_SKILL_HOME" "export XDG_CONFIG_HOME=$XDG_CONFIG_HOME" "export no_proxy=127.0.0.1,localhost,::1 NO_PROXY=127.0.0.1,localhost,::1" > "$U/env.sh"

KEEP_ON_FAIL="${KEEP_ON_FAIL:-1}"
cleanup_ok(){ tmux kill-session -t "$SESSION" 2>/dev/null; rm -rf "$U"; }
fail_exit(){
  echo "---- server pane ----"; tmux capture-pane -p -t "$SESSION:server" 2>/dev/null | tail -20
  echo "---- computer pane ----"; tmux capture-pane -p -t "$SESSION:computer" 2>/dev/null | tail -20
  echo "---- agent pane ----"; tmux capture-pane -p -t "$SESSION:agent" 2>/dev/null | tail -30
  if [[ "$KEEP_ON_FAIL" == "1" ]]; then echo "FULL-PROTOCOL UAT (tmux): ❌ FAIL —— session 保留供调试: tmux attach -t $SESSION"; else cleanup_ok; echo "FULL-PROTOCOL UAT (tmux): ❌ FAIL"; fi
  exit 1
}
# 轮询 tee 的**日志文件**直到出现正则（或超时秒数）。
# ⚠️ 不读 capture-pane：80 列 pane 会把 "listening on 127.0.0.1:PORT" 折行，正则匹配不到。
# 日志文件无折行，是可靠真值源。
wait_log(){ local f="$1" re="$2" to="${3:-20}" i; for ((i=0;i<to*2;i++)); do [[ -f "$U/$f" ]] && grep -qE "$re" "$U/$f" && return 0; sleep 0.5; done; return 1; }

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -n server

# ── 1) Server window ─────────────────────────────────────────────────────────
echo "== 1) server window =="
tmux send-keys -t "$SESSION:server" "source $U/env.sh && '$SRV' 127.0.0.1:0 2>&1 | tee $U/server.log" Enter
wait_log server.log "listening on 127.0.0.1:[0-9]+" 20 || { echo "server 未报端口"; fail_exit; }
PORT="$(grep -oiE 'listening on 127.0.0.1:[0-9]+' "$U/server.log" | grep -oE '[0-9]+$' | head -1)"
echo "server up on $PORT"

# ── 2) Computer window（REPL：start all + socket join）────────────────────────
echo "== 2) computer window =="
tmux new-window -t "$SESSION" -n computer
tmux send-keys -t "$SESSION:computer" "source $U/env.sh && '$COMP' --url http://127.0.0.1:$PORT --approve-all-mcp run --config $U/cfg.json 2>&1 | tee $U/computer.log" Enter
wait_log computer.log "进入交互模式|Enter interactive mode" 20 || { echo "computer REPL 未就绪"; fail_exit; }
tmux send-keys -t "$SESSION:computer" "start all" Enter
wait_log computer.log "所有服务器启动完成|All servers started" 20 || { echo "MCP 未启动"; fail_exit; }
tmux send-keys -t "$SESSION:computer" "socket join $OFFICE $COMPUTER_NAME" Enter
wait_log computer.log "已加入房间|Joined office" 15 || { echo "computer 未加入 office"; fail_exit; }
echo "computer joined office"

# ── 3) F-05：版本不兼容 4008（curl）──────────────────────────────────────────
echo "== 3) F-05 版本握手 4008 (curl) =="
F05=$(curl -s -i "http://127.0.0.1:$PORT/socket.io/?EIO=4&transport=polling&a2c_version=0.1.0" | head -1)
if echo "$F05" | grep -q " 400"; then echo "UAT_RESULT: PASS F-05 ($F05)"; F05_OK=1; else echo "UAT_RESULT: FAIL F-05 ($F05)"; F05_OK=0; fi

# ── 4) Agent window（全用例）─────────────────────────────────────────────────
echo "== 4) agent window: all modes =="
tmux new-window -t "$SESSION" -n agent
tmux send-keys -t "$SESSION:agent" "source $U/env.sh && RUST_LOG=info SMCP_SERVER_URL=http://127.0.0.1:$PORT SMCP_OFFICE_ID=$OFFICE SMCP_AGENT_ID=agent1 SMCP_COMPUTER=$COMPUTER_NAME SMCP_SKILL_NAME=$SKILL_NAME SMCP_TEST_MODE=all '$AGENT' 2>&1 | tee $U/agent.log" Enter
wait_log agent.log "E2E Test Agent done:" 40 || { echo "agent 未在时限内结束"; fail_exit; }

# ── 5) 结果汇总（严格门控：任一 FAIL 即失败）─────────────────────────────────
echo "== 结果汇总 =="
grep -E "UAT_RESULT:" "$U/agent.log" || true
echo "UAT_RESULT: F-05 (见上 curl)"
PASS=$(grep -cE "UAT_RESULT: PASS" "$U/agent.log")
FAILN=$(grep -cE "UAT_RESULT: FAIL" "$U/agent.log")
[[ "$F05_OK" == "1" ]] || FAILN=$((FAILN+1))
echo "agent-PASS=$PASS  agent-FAIL=$FAILN  F-05=$([[ $F05_OK == 1 ]] && echo PASS || echo FAIL)"
[[ "$FAILN" -gt 0 ]] && fail_exit

cleanup_ok
echo "FULL-PROTOCOL UAT (tmux): ✅ PASS（agent $PASS modes + F-05，真三进程 tmux 三 window）"
