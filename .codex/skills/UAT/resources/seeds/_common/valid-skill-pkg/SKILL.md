---
name: valid-skill-pkg
description: "Well-formed minimal SKILL used as the canonical happy-path source for UAT seed derivation across mcp / marketplace / user sources."
license: MIT
version: 1.0.0
allowed-tools:
  - Read
compatibility: "a2c-smcp>=0.2.1"
metadata:
  axis: CM-01
  derived-from: __self__
---

# valid-skill-pkg

This SKILL is the canonical well-formed package consumed by UAT seeds. Its body
content is intentionally minimal — only the frontmatter and the package layout
matter for fixture purposes.

## When to invoke

LLM should never see this in production; it is used by UAT acceptance scripts to
verify that marketplace / mcp / user staging accept a happy-path SKILL package.

## Layout exhibited

- `SKILL.md` (this file)
- `scripts/run.py` (placeholder executable script)
- `references/usage.md` (placeholder documentation reference)
