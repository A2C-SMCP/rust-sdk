---
name: create-skill
description: Creates new Claude Skills with proper structure, SKILL.md templates, and directory organization. 创建符合规范的新 Claude Skill，包含完整结构、模板和目录组织。当用户需要创建新 skill、为 Claude Code 添加功能或搭建 skill 脚手架时使用。
---

# Create Skill

帮助创建符合 Claude Code 规范的新 Skill。

## Quick start

创建最简 Skill 结构（30 秒）：

```bash
# 在项目根目录执行
mkdir -p .claude/skills/your-skill
cat > .claude/skills/your-skill/SKILL.md << 'EOF'
---
name: your-skill
description: 简要描述这个 skill 的功能和适用场景（最多 1024 字符）
---

# Your Skill Name

## Quick start
\```bash
# 最常见的用例代码示例
\```
EOF
```

## Basic usage

### 标准 Skill 模板

使用完整模板创建 Skill：

```bash
# 1. 创建目录结构
mkdir -p .claude/skills/my-skill/resources

# 2. 创建 SKILL.md
cat > .claude/skills/my-skill/SKILL.md << 'EOF'
---
name: my-skill
description: 清晰描述这个 skill 的功能以及何时调用它。提及关键触发条件和典型使用场景。
---

# Skill Title

## Quick start
最常见的用例（20% 代码解决 80% 问题）：

\```python
# 示例代码
def main():
    print("Hello from skill")
\```

## Basic usage
标准用法和常见模式：

\```python
# 更详细的示例
\```

## Advanced usage
复杂场景和高级特性：

\```python
# 高级用法
\```

## Resources
- `{baseDir}/resources/reference.md`
- `{baseDir}/resources/examples.md`
EOF
```

### 命名规范

```bash
# ✅ 推荐的命名
data-processing      # 小写 + 连字符
csv-analyzer        # 清晰的功能描述
file-converter      # 动名词形式

# ❌ 避免
DataProcessing      # 不要大写
data_processing     # 不要下划线
skill-data-process  # 不要前缀 "skill-"
```

## Advanced usage

### 添加资源文件

```bash
# 创建参考文档
cat > .claude/skills/my-skill/resources/reference.md << 'EOF'
# API Reference

## Function signature
\```python
function_name(param1: str, param2: int) -> dict
\```

## Parameters
- param1: Description
- param2: Description
\```

# 创建示例代码
cat > .claude/skills/my-skill/resources/examples.md << 'EOF'
# Examples

## Example 1: Basic usage
\```python
# 完整示例
\```
EOF
```

### MCP 工具集成

如果 Skill 需要 MCP 工具，在文档中注明：

```markdown
## MCP Tools Required

- `filesystem:read_file` - 读取配置文件
- `database:query` - 查询数据

使用示例：
\```python
# Tool reference format: ServerName:tool_name
result = filesystem:read_file(path="config.json")
\```
```

### 添加模板文件

```bash
# 创建提示词模板
mkdir -p .claude/skills/my-skill/templates
cat > .claude/skills/my-skill/templates/prompt.txt << 'EOF
You are a specialized assistant for {task_type}.
Focus on {primary_objective}.

Constraints:
- {constraint_1}
- {constraint_2}
EOF
```

## 最佳实践

### 内容组织（渐进式披露）

```markdown
## Quick start        # 80% 的常见用例
## Basic usage        # 标准用法
## Advanced usage     # 复杂场景
## Edge cases         # 边缘情况
```

### 路径引用规范

```markdown
# ✅ 正确：使用正斜杠和 {baseDir}
path: "{baseDir}/resources/config.json"

# ❌ 错误：Windows 风格反斜杠
path: "resources\\config.json"
```

### 描述编写技巧

#### 🌐 语言选择原则

**重要：根据使用场景选择 description 语言**

- **中文使用环境**：description 应使用中文描述
- **国际化/英文环境**：description 应使用英文描述
- **混合环境**：可以同时提供中英文描述（英文在前，中文在后）

**为什么这样要求？**
- Claude Code 会根据 description 匹配和调用 Skill
- 使用与用户交互语言一致的 description 可以提高匹配准确度
- 母语描述更容易让用户理解 Skill 的功能和使用场景

#### 中文描述示例

```yaml
# ✅ 中文环境 - 使用中文描述
---
name: csv-analyzer
description: 分析 CSV 文件，生成统计报告，识别数据模式和异常值。当用户需要分析表格数据、生成统计摘要或探索数据集时使用。
---
```

#### 英文描述示例

```yaml
# ✅ 英文环境 - 使用英文描述
---
name: csv-analyzer
description: Analyzes CSV files, generates statistics, identifies patterns and anomalies. Use when users need to analyze tabular data, generate statistical summaries, or explore datasets.
---
```

#### 中英双语示例

```yaml
# ✅ 混合环境 - 先英文后中文
---
name: create-skill
description: Creates new Claude Skills with proper structure and templates. 创建符合规范的新 Claude Skill，提供完整结构和模板。当需要创建新 skill 或添加功能时使用。
---
```

#### 描述质量标准

```yaml
# ✅ 好的描述（中文）
description: "分析 CSV 文件，生成描述性统计，识别数据模式，创建可视化图表。当用户需要探索、汇总或理解表格数据时使用。"
# 特点：清晰说明功能、明确触发场景、具体可操作

# ❌ 不好的描述（太模糊）
description: "一个数据处理工具。"
# 问题：未说明具体功能，无法判断何时使用

# ❌ 不好的描述（太冗长）
description: "本 skill 提供全面的数据分析功能，包括但不限于统计分析、数据可视化、数据清洗、数据转换以及各种其他数据相关操作，适用于多种场景..."
# 问题：冗长啰嗦，缺乏重点
```

### 职责单一原则

```bash
# ✅ 一个 Skill = 一个明确的功能
csv-analyzer       # 只做 CSV 分析
pdf-extractor      # 只做 PDF 提取
image-resizer      # 只做图片缩放

# ❌ 避免"大杂烩" Skill
data-tool          # 太宽泛，混合多种功能
```

## 常见错误

| 错误 | 解决方案 |
|------|---------|
| `Invalid YAML frontmatter` | 确保 `---` 包围 YAML 块 |
| `name exceeds 64 chars` | 缩短 name 字段 |
| `Skill not found` | 检查目录结构：`.claude/skills/your-skill/SKILL.md` |
| `Resources not loading` | 使用 `{baseDir}` 变量和正斜杠 |

## 验证清单

创建 Skill 后检查：

- [ ] SKILL.md 位于 `.claude/skills/your-skill/` 目录
- [ ] YAML frontmatter 包含 name 和 description
- [ ] name 使用小写字母和连字符
- [ ] description 使用正确的语言（中文环境用中文，英文环境用英文）
- [ ] description 清晰说明功能和触发场景
- [ ] 使用渐进式披露组织内容
- [ ] 所有路径使用正斜杠
- [ ] 代码示例可以直接运行

## 示例 Skill 结构

```bash
.claude/skills/create-skill/
├── SKILL.md                      # 必需：入口文件
├── resources/                    # 可选：辅助资源
│   ├── skill-template.md         # SKILL.md 模板
│   └── examples.md               # 完整示例
└── templates/                    # 可选：提示词模板
    └── basic-skill.txt           # 基础 Skill 模板
```

## 相关资源

- 官方文档：https://platform.claude.com/docs/building-with-claude/skills
- 项目规范：参见项目根目录 `Claude_Skill规范与最佳实践.md`
