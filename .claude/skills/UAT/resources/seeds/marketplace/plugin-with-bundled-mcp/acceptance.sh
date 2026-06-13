#!/usr/bin/env bash
# Acceptance for seeds/marketplace/plugin-with-bundled-mcp/ (Rust SDK)
# Axis: MK-BMC-01 — plugin with bundled MCP server installs skill + server.
#
# 驱动 Rust CLI：marketplace add → plugin install，验证 bundledMcpServers 含 figma-mcp。
# 用法：A2C_BIN=/path/to/target/debug/smcp-computer bash acceptance.sh
set -Eeuo pipefail

SEED_DIR="$(cd "$(dirname "$0")" && pwd)"
SEEDS_ROOT="$SEED_DIR/.."
SEED_NAME="$(basename "$SEED_DIR")"
REPO_ROOT="$(cd "$SEED_DIR/../../../../../.." && pwd)"
TMPDIR="$(mktemp -d -t "a2c-mp-${SEED_NAME}.XXXXXX")"
WORK="$TMPDIR/work"; BARE="$TMPDIR/${SEED_NAME}.git"
HOME_DIR="$TMPDIR/skill-home"; CONFIG_DIR="$TMPDIR/config"
LOG="$TMPDIR/run.log"

cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT INT TERM
fail() { echo "FAIL: $*" >&2; echo "---- last 60 log lines ----" >&2; tail -60 "$LOG" >&2 || true; exit 1; }

if [[ -n "${A2C_BIN:-}" && -x "${A2C_BIN:-}" ]]; then A2C=("$A2C_BIN")
else A2C=(cargo run -q --manifest-path "$REPO_ROOT/Cargo.toml" -p smcp-computer --features cli --); fi

mkdir -p "$HOME_DIR" "$CONFIG_DIR"
export A2C_SKILL_HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_DIR"

# 1. worktree + bare repo
bash "$SEEDS_ROOT/_helpers/init_bare_repo.sh" "$SEED_DIR" "$WORK" "$BARE" > "$LOG" 2>&1 \
  || fail "init_bare_repo failed"

# 2. 捆绑 MCP server 配置须含 type 字段（Rust schema 要求）
[[ -f "$WORK/plugins/foo/mcp-servers/figma-mcp.json" ]] || fail "bundled MCP config missing in worktree"
grep -q '"type"' "$WORK/plugins/foo/mcp-servers/figma-mcp.json" \
  || fail "bundled MCP config missing 'type' field (Rust requires it)"

# 3. marketplace add + plugin install
"${A2C[@]}" marketplace add "file://$BARE" --name mp-bundled-mcp --trust --json >> "$LOG" 2>&1 \
  || fail "marketplace add failed"
"${A2C[@]}" plugin install foo@mp-bundled-mcp --json >> "$LOG" 2>&1 \
  || fail "plugin install failed"

# 4. PASS 判据：install 输出含 figma-mcp 捆绑
grep -q '"installed"' "$LOG" && grep -q "foo@mp-bundled-mcp" "$LOG" \
  || fail "expected installed foo@mp-bundled-mcp"
grep -q "figma-mcp" "$LOG" \
  || fail "expected bundledMcpServers to include figma-mcp"

echo "PASS: marketplace seed ${SEED_NAME}"
