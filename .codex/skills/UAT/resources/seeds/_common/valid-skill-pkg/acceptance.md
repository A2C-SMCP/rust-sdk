# Acceptance: `_common/valid-skill-pkg`

**Axis**: CM-01

**校验项**:

- [ ] `SKILL.md` 存在
- [ ] YAML frontmatter 可解析
- [ ] `name` = "valid-skill-pkg"
- [ ] `description` 非空且为字符串
- [ ] 目录结构包含 `SKILL.md` / `scripts/run.py` / `references/usage.md`

## 自动化脚本（语言无关，仅校验包本体结构）

```bash
#!/usr/bin/env bash
set -Eeuo pipefail
SEED_DIR="$(cd "$(dirname "$0")" && pwd)"

skill_md="$SEED_DIR/SKILL.md"
[[ -f "$skill_md" ]] || { echo "FAIL: SKILL.md missing in $SEED_DIR" >&2; exit 1; }
head -1 "$skill_md" | grep -q '^---' || { echo "FAIL: frontmatter fence missing" >&2; exit 1; }
grep -q '^name: valid-skill-pkg' "$skill_md" || { echo "FAIL: unexpected name" >&2; exit 1; }
grep -q '^description:' "$skill_md" || { echo "FAIL: description missing" >&2; exit 1; }
[[ -f "$SEED_DIR/scripts/run.py" ]] || { echo "FAIL: scripts/run.py missing" >&2; exit 1; }
[[ -f "$SEED_DIR/references/usage.md" ]] || { echo "FAIL: references/usage.md missing" >&2; exit 1; }

echo "PASS: _common/valid-skill-pkg"
```
