# `marketplace/strict-false-conflict`
**Axis**: MK-STR-FALSE-CONFLICT — strict=false + plugin.json 声明组件 → 冲突降级。
- marketplace `strict-false-conflict`，plugin `audit`（strict:false），plugin.json.skills=["skills"]
- 派生：skills/greet→minimal-greet
- **期望**：add exit 0，skills=0（plugin 跳过），marketplace 仍添加，installedPlugins=[]
- **Rust 差异**：冲突降级时 stderr 无警告（Python 有）；断言基于 skills=0 等可观测结果。
