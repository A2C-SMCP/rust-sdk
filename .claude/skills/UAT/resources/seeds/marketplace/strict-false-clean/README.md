# `marketplace/strict-false-clean`
**Axis**: MK-STR-FALSE-CLEAN — strict=false + plugin.json 无组件 → 仅取 entry.skills。
- marketplace `strict-false-clean`，plugin `audit`（strict:false），entry.skills=["extra-skills"]，plugin.json={}
- 派生：skills/greet→minimal-greet, extra-skills/review→minimal-review
- **期望**：skills=2；skill list 显示 audit:greet/review（无 scan）
