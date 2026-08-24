#!/usr/bin/env bash
# Acceptance for seeds/marketplace/valid-single-plugin/ (Rust SDK)
# Axis: MK-VAL-01 — happy single-plugin marketplace registers expected SKILL.
#
# 驱动 Rust CLI `smcp-computer marketplace add`（非交互 + --json），验证：
#   1) 退出码 0
#   2) 物化包根 SKILL.md 存在
#   3) JSON 输出含 name=uat-seed-mp / trusted=true
#
# 用法：
#   A2C_BIN=/path/to/target/debug/smcp-computer bash acceptance.sh
# 缺省 A2C_BIN 时回退到 `cargo run -q -p smcp-computer --features cli --`。
set -Eeuo pipefail

SEED_DIR="$(cd "$(dirname "$0")" && pwd)"
SEEDS_ROOT="$SEED_DIR/.."
SEED_NAME="$(basename "$SEED_DIR")"
REPO_ROOT="$(cd "$SEED_DIR/../../../../../.." && pwd)"   # → rust-sdk 项目根
TMPDIR="$(mktemp -d -t "a2c-mp-${SEED_NAME}.XXXXXX")"
WORK="$TMPDIR/work"
BARE="$TMPDIR/${SEED_NAME}.git"
HOME_DIR="$TMPDIR/skill-home"
LOG="$TMPDIR/run.log"

cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT INT TERM

fail() {
  echo "FAIL: $*" >&2
  echo "---- last 60 log lines ----" >&2
  tail -60 "$LOG" >&2 || true
  exit 1
}

# A2C CLI 调用方式：优先用预编译二进制（快），否则 cargo run。
if [[ -n "${A2C_BIN:-}" && -x "${A2C_BIN:-}" ]]; then
  A2C=("$A2C_BIN")
else
  A2C=(cargo run -q --manifest-path "$REPO_ROOT/Cargo.toml" -p smcp-computer --features cli --)
fi

# 1. Build worktree (materialize _seeds.manifest) + bare repo
bash "$SEEDS_ROOT/_helpers/init_bare_repo.sh" "$SEED_DIR" "$WORK" "$BARE" > "$LOG" 2>&1 \
  || fail "init_bare_repo failed"

CONFIG_DIR="$TMPDIR/config"   # 隔离 settings.json（trust 决策落盘处）
mkdir -p "$HOME_DIR" "$CONFIG_DIR"

# 2. Drive `marketplace add`（双隔离：A2C_SKILL_HOME + XDG_CONFIG_HOME）
A2C_SKILL_HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_DIR" "${A2C[@]}" \
  marketplace add "file://$BARE" --name uat-seed-mp --trust --json \
  >> "$LOG" 2>&1 \
  || fail "marketplace add exited non-zero"

# 3. PASS 判据
SKILL_MD="$HOME_DIR/marketplace/uat-seed-mp/plugins/foo/skills/valid-skill-pkg/SKILL.md"
[[ -f "$SKILL_MD" ]] || fail "staged SKILL.md not found at $SKILL_MD"
grep -q "valid-skill-pkg" "$SKILL_MD" || fail "staged SKILL.md missing expected name token"

# `marketplace add` 返回精简 JSON：{"added": "uat-seed-mp", "skills": 1, "url": ...}
grep -q '"added"' "$LOG" && grep -q "uat-seed-mp" "$LOG" \
  || fail "expected {\"added\": \"uat-seed-mp\", ...} in JSON output"
grep -q '"skills": 1' "$LOG" \
  || fail "expected skills=1 (one SKILL discovered) in JSON output"

echo "PASS: marketplace seed ${SEED_NAME}"
