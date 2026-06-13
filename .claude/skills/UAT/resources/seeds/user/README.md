# `user/` — DropIn 就地种子

> 每条种子在 acceptance 时把 `_common/<x>` 拷进 `$A2C_SKILL_HOME/user/<skill>/`，
> 通过 `skill list --source user` 验证发现。user 源就地发现，SKILL 不被拷走到别处。

## 子目录结构

```
user/
└── <name>/
    ├── _seeds.manifest   ← 派生 → _common/<x>
    ├── README.md
    └── acceptance.sh     ← drop 进 $HOME/user/ + skill list --source user
```

## 索引

| name | axis | acceptance | 引用 scenarios |
|---|---|---|---|
| home-user-basic | US-VAL-01 | user/home-user-basic/acceptance.sh | skill-discovery (D-02) |
