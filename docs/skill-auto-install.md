# SKILL 自动安装指令文档

> **致正在阅读本文档的 AI Agent（Claude Code / Codex CLI）：**
> 这是一份可执行的安装指令。请按下面的步骤，把本文档「待安装 SKILL」一节中的内容安装到当前环境的 Skill 目录中。全程使用你自己的文件工具（创建目录、写文件），不需要向用户追问，安装完成后输出验证结果即可。

---

## 第 0 步：识别你所在的环境

按以下特征判断自己是哪个 agent，并记住对应的 **用户级 Skill 根目录**：

| 环境 | 判断特征 | 用户级 Skill 目录 | 项目级 Skill 目录 |
|---|---|---|---|
| **Claude Code** | 系统提示中自称 "Claude Code"；存在 `~/.claude/` 目录 | `~/.claude/skills/` | `<项目根>/.claude/skills/` |
| **Codex CLI** | 系统提示中自称 "Codex"；存在 `~/.codex/` 目录（或 `$CODEX_HOME`） | `~/.codex/skills/`（若设置了 `CODEX_HOME` 则为 `$CODEX_HOME/skills/`；也兼容 `~/.agents/skills/`） | `<项目根>/.codex/skills/` |

安装范围选择：
- **默认安装到用户级目录**（所有项目可用）。
- 仅当用户明确说"装到这个项目"时，才安装到项目级目录。

## 第 1 步：读取待安装的 SKILL

本文档末尾「待安装 SKILL」一节的代码块中是完整的 `SKILL.md` 内容。从 YAML frontmatter 中取出 `name` 字段的值作为 **skill 目录名**（kebab-case，全小写，连字符分隔）。

格式校验（不符合则先修正再安装）：
1. 文件以 `---` 开头的 YAML frontmatter 起始，且包含 `name:` 与 `description:` 两个字段。
2. **frontmatter 中只保留 `name` 和 `description` 两个字段**（Codex 只读取这两个字段；多余字段会被忽略甚至引发兼容问题）。
3. `description` 必须同时写清「这个 skill 做什么」和「什么情况下触发」——这是 agent 决定是否启用该 skill 的唯一依据。
4. frontmatter 之后是 Markdown 正文（具体操作指引）。

## 第 2 步：执行安装

以 skill 名为 `<skill-name>` 为例：

**Claude Code：**

```bash
mkdir -p ~/.claude/skills/<skill-name>
```

然后用文件写入工具（Write）把「待安装 SKILL」代码块的完整内容写入：

```
~/.claude/skills/<skill-name>/SKILL.md
```

**Codex CLI：**

```bash
mkdir -p "${CODEX_HOME:-$HOME/.codex}/skills/<skill-name>"
```

然后把同样的内容写入：

```
${CODEX_HOME:-$HOME/.codex}/skills/<skill-name>/SKILL.md
```

注意事项：
- 若目标目录已存在同名 skill，先读取旧的 `SKILL.md` 给用户看一眼差异，再覆盖（不要静默覆盖来历不明的文件）。
- 若 skill 带有附属资源（`scripts/`、`references/`、`assets/` 等子目录，本文档中以额外代码块给出的），一并写入同一 skill 目录下对应的子路径。

## 第 3 步：验证

依次执行并把结果报告给用户：

1. `ls` 确认 `SKILL.md` 已落盘到正确路径。
2. 读回文件头部，确认 frontmatter 的 `name`、`description` 完整无误。
3. 告知用户：**Skill 在新会话启动时被发现**——需要重启 agent 或开新会话后生效；之后正常对话中命中 `description` 描述的场景时会自动触发，无需手动调用。

## 失败处理

- 目录无写权限 → 报告具体报错，建议用户检查目录归属，不要擅自 `sudo`。
- 无法判断环境 → 把第 0 步的判断依据列给用户，请用户指定。
- 「待安装 SKILL」一节为空或格式损坏 → 不要猜测内容，向用户要完整的 SKILL.md。

---

## 待安装 SKILL

> 使用者注意：把你要分发的 `SKILL.md` 完整内容粘贴到下面的代码块中（替换示例），然后把整份文档发给 Claude Code 或 Codex 即可自动安装。

````markdown
---
name: example-skill
description: 这是一个示例 skill，演示安装文档的用法。当用户提到"示例 skill"或要求演示 skill 安装流程时使用。请在分发前把本块整体替换为真实的 SKILL.md 内容。
---

# Example Skill

## 使用说明

1. 这里写 skill 被触发后 agent 应当执行的具体步骤。
2. 步骤使用祈使句，清晰、可执行。
3. 如需附带脚本或参考资料，放在 skill 目录的 `scripts/`、`references/` 子目录中，并在此处说明何时读取。
````

<!-- 如 skill 含附属文件，按下面的格式追加，agent 会一并安装：

### 附属文件：scripts/run.sh

```bash
#!/usr/bin/env bash
echo "hello from skill"
```
-->
