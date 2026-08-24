#!/usr/bin/env bash
# Acceptance for seeds/user/home-user-basic/ (Rust SDK)
# Axis: US-VAL-01 — user drop-in skill discovered via `skill list --source user`.
# 用法：A2C_BIN=/path/to/target/debug/smcp-computer bash acceptance.sh
set -Eeuo pipefail
SEED_DIR="$(cd "$(dirname "$0")" && pwd)"
SEEDS_ROOT="$(cd "$SEED_DIR/../.." && pwd)"   # seeds/user/<name>/ → seeds/
REPO_ROOT="$(cd "$SEED_DIR/../../../../../.." && pwd)"
TMPDIR="$(mktemp -d -t "a2c-user-home-user-basic.XXXXXX")"
LOG="$TMPDIR/run.log"
cleanup(){ rm -rf "$TMPDIR"; }; trap cleanup EXIT INT TERM
fail(){ echo "FAIL: $*" >&2; tail -40 "$LOG" >&2 || true; exit 1; }
if [[ -n "${A2C_BIN:-}" && -x "${A2C_BIN:-}" ]]; then A2C=("$A2C_BIN")
else A2C=(cargo run -q --manifest-path "$REPO_ROOT/Cargo.toml" -p smcp-computer --features cli --); fi
export A2C_SKILL_HOME="$TMPDIR/skill-home" XDG_CONFIG_HOME="$TMPDIR/config"
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"

# 1. drop _common/valid-skill-pkg into $HOME/user/valid-skill-pkg (basename = frontmatter name)
src=$(awk '/^source:/{print $2}' "$SEED_DIR/_seeds.manifest")
case "$src" in _common/*) ;; *) fail "unsupported _seeds.manifest source: $src" ;; esac
mkdir -p "$A2C_SKILL_HOME/user/valid-skill-pkg"
cp -R "$SEEDS_ROOT/$src"/. "$A2C_SKILL_HOME/user/valid-skill-pkg/"

# 2. drive `skill list --source user`
"${A2C[@]}" skill list --source user --json > "$LOG" 2>&1 || fail "skill list failed"

# 3. PASS 判据
grep -q '"name": "valid-skill-pkg"' "$LOG" || fail "expected user skill valid-skill-pkg"
grep -q '"source": "user"' "$LOG" || fail "expected source=user"
# SKILL 就地未被拷走
[[ -f "$A2C_SKILL_HOME/user/valid-skill-pkg/SKILL.md" ]] || fail "SKILL.md should remain in place"

echo "PASS: user seed home-user-basic"
