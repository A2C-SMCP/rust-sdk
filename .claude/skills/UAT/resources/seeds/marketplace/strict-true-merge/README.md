# `marketplace/strict-true-merge`
**Axis**: MK-STR-TRUE — strict=true（默认）entry.skills + plugin.json.skills 追加合并。
- marketplace `strict-true-merge`，plugin `audit`
- entry.skills=["extra-skills"]，plugin.json.skills=["more-skills"]
- 派生：skills/greet→minimal-greet, extra-skills/review→minimal-review, more-skills/scan→minimal-scan
- **期望**：`marketplace add` → skills=3；skill list 显示 audit:greet/review/scan
