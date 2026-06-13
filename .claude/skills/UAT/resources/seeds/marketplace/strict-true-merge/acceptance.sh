#!/usr/bin/env bash
# Acceptance for seeds/marketplace/strict-true-merge/ (Rust SDK)
# Axis: MK-STR-TRUE — assert 'marketplace add' registers 3 skill(s).
# 用法：A2C_BIN=/path/to/target/debug/smcp-computer bash acceptance.sh
set -Eeuo pipefail
SEED_DIR="$(cd "$(dirname "$0")" && pwd)"
SEEDS_ROOT="$SEED_DIR/.."
SEED_NAME="$(basename "$SEED_DIR")"
REPO_ROOT="$(cd "$SEED_DIR/../../../../../.." && pwd)"
TMPDIR="$(mktemp -d -t "a2c-mp-${SEED_NAME}.XXXXXX")"
LOG="$TMPDIR/run.log"
cleanup(){ rm -rf "$TMPDIR"; }; trap cleanup EXIT INT TERM
fail(){ echo "FAIL: $*" >&2; tail -40 "$LOG" >&2 || true; exit 1; }
if [[ -n "${A2C_BIN:-}" && -x "${A2C_BIN:-}" ]]; then A2C=("$A2C_BIN")
else A2C=(cargo run -q --manifest-path "$REPO_ROOT/Cargo.toml" -p smcp-computer --features cli --); fi
export A2C_SKILL_HOME="$TMPDIR/skill-home" XDG_CONFIG_HOME="$TMPDIR/config"
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"
bash "$SEEDS_ROOT/_helpers/init_bare_repo.sh" "$SEED_DIR" "$TMPDIR/work" "$TMPDIR/bare.git" > "$LOG" 2>&1 || fail "init_bare_repo failed"
"${A2C[@]}" marketplace add "file://$TMPDIR/bare.git" --name "strict-true-merge" --trust --json >> "$LOG" 2>&1 || fail "marketplace add failed"
grep -q '"skills": 3' "$LOG" || fail "expected skills=3, got: $(grep '\"skills\"' "$LOG" || true)"
echo "PASS: marketplace seed ${SEED_NAME} (skills=3)"
