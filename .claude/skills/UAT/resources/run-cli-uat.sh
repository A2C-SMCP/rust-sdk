#!/usr/bin/env bash
# 一键运行 A2C-SMCP Rust SDK 的 CLI-only UAT（4 个已跑通场景 + 全部种子 acceptance）。
#
# 用法：
#   bash .claude/skills/UAT/resources/run-cli-uat.sh
# 可选：A2C_BIN=/abs/target/debug/smcp-computer（缺省自动编译 + 用 target/debug）
#
# 覆盖：marketplace-ops / settings-scope / plugin-management / strict-mode / skill-discovery(CLI 部分)
# 完整链路场景（full-protocol 等）不在此脚本内，需多进程编排（见 test-env-setup.md）。
set -uo pipefail

SKILL_DIR="$(cd "$(dirname "$0")/.." && pwd)"          # .claude/skills/UAT
REPO_ROOT="$(cd "$SKILL_DIR/../../.." && pwd)"          # rust-sdk
SEEDS="$SKILL_DIR/resources/seeds"

# 0. 编译/定位二进制
if [[ -z "${A2C_BIN:-}" ]]; then
  echo "==> building smcp-computer (cli)…"
  ( cd "$REPO_ROOT" && cargo build -q -p smcp-computer --features cli ) || { echo "build failed"; exit 1; }
  A2C_BIN="$REPO_ROOT/target/debug/smcp-computer"
fi
[[ -x "$A2C_BIN" ]] || { echo "A2C_BIN not executable: $A2C_BIN"; exit 1; }
A2C="$A2C_BIN"
echo "==> using A2C=$A2C"

PASS=0; FAIL=0; FAILED=()
ck(){ local n="$1" ee="$2" ae="$3" pat="$4" out="$5"
  if [[ "$ae" == "$ee" ]] && echo "$out" | grep -q "$pat"; then PASS=$((PASS+1)); printf '  ✅ %s\n' "$n"
  else FAIL=$((FAIL+1)); FAILED+=("$n"); printf '  ❌ %s (exit exp=%s got=%s; want /%s/)\n' "$n" "$ee" "$ae" "$pat"; fi; }

iso(){ U="$(mktemp -d -t a2c-uat.XXXXXX)"; export A2C_SKILL_HOME="$U/skill-home" XDG_CONFIG_HOME="$U/config"; mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"; }
mpadd(){ bash "$SEEDS/marketplace/_helpers/init_bare_repo.sh" "$SEEDS/marketplace/$1" "$U/work" "$U/bare.git" >/dev/null 2>&1; "$A2C" marketplace add "file://$U/bare.git" --name "$2" --trust --json >/dev/null 2>&1; }
done_iso(){ rm -rf "$U"; }

echo; echo "### marketplace-ops"
iso; o=$("$A2C" marketplace add "file://$(bash "$SEEDS/marketplace/_helpers/init_bare_repo.sh" "$SEEDS/marketplace/valid-single-plugin" "$U/work" "$U/bare.git" 2>/dev/null)" --name uat-seed-mp --trust --json 2>&1); ck M-01 0 $? '"added": "uat-seed-mp"' "$o"
o=$("$A2C" marketplace list --json 2>&1); ck M-02 0 $? '"trusted": true' "$o"
o=$("$A2C" marketplace info uat-seed-mp --json 2>&1); ck M-03 0 $? '"installLocation"' "$o"
o=$("$A2C" marketplace refresh uat-seed-mp --json 2>&1); ck M-04 0 $? '"unchanged"' "$o"
o=$("$A2C" marketplace set uat-seed-mp auto-update=true --json 2>&1); ck M-05 0 $? '"autoUpdate": true' "$o"
o=$("$A2C" marketplace add "file://$U/bare.git" --name uat-seed-mp --trust --json 2>&1); ck M-08 1 $? 'already exists' "$o"
o=$("$A2C" marketplace remove uat-seed-mp --json 2>&1); ck M-07 0 $? '"removed": "uat-seed-mp"' "$o"
done_iso

echo; echo "### settings-scope"
iso
o=$("$A2C" settings show --json 2>&1); ck G-01 0 $? '{}' "$o"
o=$("$A2C" settings set strictKnownMarketplaces true --scope user --json 2>&1); ck G-04 0 $? '"value": true' "$o"
o=$("$A2C" settings get strictKnownMarketplaces --json 2>&1); ck G-03 0 $? 'strictKnownMarketplaces": true' "$o"
o=$("$A2C" settings set x y --scope project --json 2>&1); ck G-05 1 $? 'active workdir' "$o"
o=$("$A2C" settings set x y --scope policy --json 2>&1); ck G-07 1 $? 'read-only' "$o"
done_iso

echo; echo "### plugin-management"
iso; mpadd plugin-with-bundled-mcp mp-bundled-mcp
o=$("$A2C" plugin install foo@mp-bundled-mcp --json 2>&1); ck P-01 0 $? 'figma-mcp' "$o"
o=$("$A2C" plugin list --json 2>&1); ck P-02 0 $? '"enabled": true' "$o"
o=$("$A2C" plugin disable foo@mp-bundled-mcp --json 2>&1); ck P-04 0 $? '"disabled"' "$o"
o=$("$A2C" plugin enable foo@mp-bundled-mcp --json 2>&1); ck P-05 0 $? '"enabled": "foo' "$o"
o=$("$A2C" plugin uninstall foo@mp-bundled-mcp --keep-servers --json 2>&1); ck P-09 0 $? '"keptServers": true' "$o"
done_iso

echo; echo "### strict-mode"
iso; mpadd strict-true-merge strict-true-merge; o=$("$A2C" skill list --source mp --json 2>&1); ck S-01 0 $? 'audit:scan' "$o"; done_iso
iso; mpadd strict-false-clean strict-false-clean; o=$("$A2C" skill list --source mp --json 2>&1); ck S-02 0 $? 'audit:review' "$o"; done_iso
iso; B=$(bash "$SEEDS/marketplace/_helpers/init_bare_repo.sh" "$SEEDS/marketplace/strict-false-conflict" "$U/work" "$U/bare.git" 2>/dev/null); o=$("$A2C" marketplace add "file://$B" --name strict-false-conflict --trust --json 2>/dev/null); ck S-03 0 $? '"skills": 0' "$o"; done_iso

echo; echo "### skill-discovery (CLI part)"
iso; mpadd valid-single-plugin uat-seed-mp
mkdir -p "$A2C_SKILL_HOME/user/valid-skill-pkg"; cp -R "$SEEDS/_common/valid-skill-pkg"/. "$A2C_SKILL_HOME/user/valid-skill-pkg/"
o=$("$A2C" skill list --source mp --json 2>&1); ck D-01 0 $? 'foo:valid-skill-pkg' "$o"
o=$("$A2C" skill list --source user --json 2>&1); ck D-02 0 $? '"source": "user"' "$o"
o=$("$A2C" skill info foo:valid-skill-pkg --json 2>&1); ck D-04 0 $? '"license": "MIT"' "$o"
done_iso

echo; echo "### seed acceptances"
for acc in \
  marketplace/valid-single-plugin/acceptance.sh \
  marketplace/plugin-with-bundled-mcp/acceptance.sh \
  marketplace/strict-true-merge/acceptance.sh \
  marketplace/strict-false-clean/acceptance.sh \
  marketplace/strict-false-conflict/acceptance.sh \
  user/home-user-basic/acceptance.sh ; do
  name="seed:$(dirname "$acc" | xargs basename)"
  if A2C_BIN="$A2C" bash "$SEEDS/$acc" >/dev/null 2>&1; then PASS=$((PASS+1)); printf '  ✅ %s\n' "$name"
  else FAIL=$((FAIL+1)); FAILED+=("$name"); printf '  ❌ %s\n' "$name"; fi
done

echo; echo "================ UAT SUMMARY ================"
echo "PASS=$PASS  FAIL=$FAIL"
if (( FAIL > 0 )); then printf 'FAILED: %s\n' "${FAILED[*]}"; exit 1; fi
echo "ALL GREEN ✅"
