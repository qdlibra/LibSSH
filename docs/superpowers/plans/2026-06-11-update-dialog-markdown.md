# 更新说明 markdown 块级渲染 + 自适应弹窗 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让软件更新弹窗把 release notes 的 markdown 渲染成排版块（标题/列表/代码块/分隔线），并让弹窗随窗口自适应、说明区可滚动，不再溢出窗口或显示字面符号。

**Architecture:** 保持「Rust 解析 → Slint 原生渲染」边界。Rust 侧用 `pulldown-cmark` 把 markdown 解析成有类型的 `Block` 序列（内联粗体/斜体/行内代码/链接去符号、保留文字），映射成 Slint `[NoteBlock]` 模型；Slint 侧用 `for`+`if` 按块类型用原生元素渲染，弹窗尺寸改为 `min(窗口*0.92, 上限)` 并把说明区放进 `ScrollView`。

**Tech Stack:** Rust、pulldown-cmark 0.13、Slint 1.16（std-widgets `ScrollView`）。

参考 spec：`docs/superpowers/specs/2026-06-11-update-dialog-markdown-design.md`

---

## 文件结构

- `Cargo.toml` — 新增 `pulldown-cmark` 依赖（关默认 html 特性）。
- `src/markdown.rs`（新建）— **唯一职责**：把 markdown 字符串解析成 `Vec<Block>`。不依赖任何 Slint 生成类型，纯函数 + 单元测试。
- `src/main.rs` — 注册 `mod markdown;`。
- `ui/app.slint` — 新增 `NoteBlock` 结构与 `update-note-blocks` 属性；更新弹窗的说明区改为按块渲染；弹窗尺寸自适应。
- `src/app.rs` — 新增 `notes_blocks_model` 把 `Block` 映射为 `NoteBlock` 模型；替换两处 `set_update_notes` 调用。

边界：`src/markdown.rs` 是可独立单测的纯逻辑核心；UI 渲染与 Rust 接线分两个任务，且每次提交都保持 `cargo build` 通过。

---

## Task 1: markdown 解析模块（Rust，TDD）

**Files:**
- Modify: `Cargo.toml`（依赖区，第 48-54 行的自动更新依赖之后）
- Create: `src/markdown.rs`
- Modify: `src/main.rs:8`（`mod logbuf;` 之后插入 `mod markdown;`）
- Test: `src/markdown.rs` 内的 `#[cfg(test)] mod tests`

- [ ] **Step 1: 加依赖**

在 `Cargo.toml` 第 54 行 `fs2 = "0.4"` 之后追加：

```toml
# 更新弹窗：把 release notes 的 markdown 解析成块序列做原生渲染（关默认 html 渲染器，仅用解析器）。
pulldown-cmark = { version = "0.13", default-features = false }
```

- [ ] **Step 2: 创建 `src/markdown.rs`，先放结构 + 桩函数 + 测试**

写入以下内容（`notes_to_blocks` 先返回空，使测试可编译并失败）：

```rust
//! 把 release notes 的 markdown 解析成有类型的块序列，供 Slint 更新弹窗按块渲染。
//! 内联标记（粗体/斜体/行内代码/链接）去符号、保留文字——Slint 稳定版无法在一行内
//! 混排多种样式，故只在块级别区分样式（见 docs/superpowers/specs/2026-06-11-…）。

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

/// 一个渲染块。`kind` 取值与 ui/app.slint 的 `NoteBlock.kind` 一一对应。
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// "para" | "h" | "ul" | "ol" | "code" | "hr"
    pub kind: &'static str,
    pub text: String,
    /// 标题级别 1..=6；或列表缩进深度（从 0 起）；其余为 0。
    pub level: i32,
    /// 有序项序号 "1."/"2."；无序项 "•"；其余为空。
    pub marker: String,
}

pub fn notes_to_blocks(_md: &str) -> Vec<Block> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_install_notice() {
        let md = concat!(
            "## macOS 安装提示\n",
            "\n",
            "dmg 未经 Apple 公证，首次打开可能提示「LibSSH 已损坏」。解决：\n",
            "\n",
            "1. 把 `LibSSH.app` 拖入「应用程序」文件夹；\n",
            "2. 终端执行 `xattr -dr com.apple.quarantine /Applications/LibSSH.app`；\n",
            "3. 重新打开。\n",
            "\n",
            "**Full Changelog**: https://github.com/qdlibra/LibSSH/compare/v0.2.8...v0.2.9\n",
        );
        let b = notes_to_blocks(md);

        assert_eq!(b[0].kind, "h");
        assert_eq!(b[0].level, 2);
        assert_eq!(b[0].text, "macOS 安装提示");

        assert_eq!(b[1].kind, "para");
        assert!(b[1].text.starts_with("dmg"));

        assert_eq!(b[2].kind, "ol");
        assert_eq!(b[2].marker, "1.");
        assert!(b[2].text.contains("LibSSH.app")); // 行内代码去反引号、保留文字

        assert_eq!(b[3].marker, "2.");
        assert!(b[3]
            .text
            .contains("xattr -dr com.apple.quarantine /Applications/LibSSH.app"));

        assert_eq!(b[4].marker, "3.");

        let last = b.last().unwrap();
        assert_eq!(last.kind, "para");
        assert!(last
            .text
            .contains("Full Changelog: https://github.com/qdlibra/LibSSH/compare/v0.2.8...v0.2.9"));
        assert!(!last.text.contains('*')); // 粗体星号已去掉
    }

    #[test]
    fn unordered_list_and_code_block() {
        let md = "- one\n- two\n\n```\ncargo build\n```\n";
        let b = notes_to_blocks(md);
        assert_eq!(b[0].kind, "ul");
        assert_eq!(b[0].marker, "•");
        assert_eq!(b[0].text, "one");
        assert_eq!(b[1].kind, "ul");
        assert_eq!(b[1].text, "two");
        assert_eq!(b[2].kind, "code");
        assert_eq!(b[2].text, "cargo build");
    }

    #[test]
    fn divider_and_empty() {
        assert!(notes_to_blocks("").is_empty());
        let b = notes_to_blocks("a\n\n---\n\nb");
        assert_eq!(b.iter().filter(|x| x.kind == "hr").count(), 1);
        assert_eq!(b.iter().filter(|x| x.kind == "para").count(), 2);
    }
}
```

- [ ] **Step 3: 注册模块**

在 `src/main.rs` 第 8 行 `mod logbuf;` 之后插入一行：

```rust
mod markdown;
```

- [ ] **Step 4: 跑测试确认失败**

Run: `cargo test markdown 2>&1 | tail -20`
Expected: FAIL —`parses_macos_install_notice` 等因桩函数返回空、`b[0]` 越界 panic（`index out of bounds`）。

- [ ] **Step 5: 实现 `notes_to_blocks`**

用下方完整实现替换 Step 2 里的桩 `notes_to_blocks`（保留 `Block` 结构与 `tests` 不动）：

```rust
#[derive(Clone)]
struct ListCtx {
    ordered: bool,
    next: u64,
}

/// 当前正在构建的块的种类（连同其元信息）。
enum Mode {
    None,
    Para,
    Heading(i32),
    Item {
        kind: &'static str,
        level: i32,
        marker: String,
    },
    Code,
}

fn heading_level(l: HeadingLevel) -> i32 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// 把当前累积的 `buf` 按 `mode` 落成一个块，并清空 `buf`、复位 `mode`。
fn flush(blocks: &mut Vec<Block>, buf: &mut String, mode: &mut Mode) {
    // 代码块保留内部换行，仅去首尾换行；其余块再 trim 首尾空白。
    let raw = buf.trim_matches('\n').to_string();
    match std::mem::replace(mode, Mode::None) {
        Mode::None => {}
        Mode::Para => {
            let t = raw.trim().to_string();
            if !t.is_empty() {
                blocks.push(Block {
                    kind: "para",
                    text: t,
                    level: 0,
                    marker: String::new(),
                });
            }
        }
        Mode::Heading(level) => {
            let t = raw.trim().to_string();
            if !t.is_empty() {
                blocks.push(Block {
                    kind: "h",
                    text: t,
                    level,
                    marker: String::new(),
                });
            }
        }
        Mode::Item { kind, level, marker } => {
            blocks.push(Block {
                kind,
                text: raw.trim().to_string(),
                level,
                marker,
            });
        }
        Mode::Code => {
            blocks.push(Block {
                kind: "code",
                text: raw,
                level: 0,
                marker: String::new(),
            });
        }
    }
    buf.clear();
}

pub fn notes_to_blocks(md: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut buf = String::new();
    let mut mode = Mode::None;
    let mut lists: Vec<ListCtx> = Vec::new();

    for ev in Parser::new(md) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut blocks, &mut buf, &mut mode);
                mode = Mode::Heading(heading_level(level));
            }
            Event::End(TagEnd::Heading(_)) => flush(&mut blocks, &mut buf, &mut mode),

            // 列表项内的段落不另起块：文字继续累积到当前 item。
            Event::Start(Tag::Paragraph) => {
                if !matches!(mode, Mode::Item { .. }) {
                    flush(&mut blocks, &mut buf, &mut mode);
                    mode = Mode::Para;
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if !matches!(mode, Mode::Item { .. }) {
                    flush(&mut blocks, &mut buf, &mut mode);
                }
            }

            Event::Start(Tag::List(start)) => lists.push(ListCtx {
                ordered: start.is_some(),
                next: start.unwrap_or(1),
            }),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush(&mut blocks, &mut buf, &mut mode);
                let level = lists.len().saturating_sub(1) as i32;
                let (kind, marker) = match lists.last_mut() {
                    Some(ctx) if ctx.ordered => {
                        let m = format!("{}.", ctx.next);
                        ctx.next += 1;
                        ("ol", m)
                    }
                    _ => ("ul", "•".to_string()),
                };
                mode = Mode::Item { kind, level, marker };
            }
            Event::End(TagEnd::Item) => flush(&mut blocks, &mut buf, &mut mode),

            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut blocks, &mut buf, &mut mode);
                mode = Mode::Code;
            }
            Event::End(TagEnd::CodeBlock) => flush(&mut blocks, &mut buf, &mut mode),

            Event::Rule => {
                flush(&mut blocks, &mut buf, &mut mode);
                blocks.push(Block {
                    kind: "hr",
                    text: String::new(),
                    level: 0,
                    marker: String::new(),
                });
            }

            Event::Text(s) => buf.push_str(&s),
            // 行内代码去掉反引号、保留命令文字。
            Event::Code(s) => buf.push_str(&s),
            Event::SoftBreak => buf.push(' '),
            Event::HardBreak => buf.push('\n'),

            // 其余（Strong/Emphasis/Link 等的 Start/End、Html、脚注等）：忽略标记，
            // 其内部文字已由上面的 Text/Code 事件照常流入 buf。
            _ => {}
        }
    }
    // 结尾兜底（正常情况下各块都已在对应 End 事件落地）。
    flush(&mut blocks, &mut buf, &mut mode);
    blocks
}
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test markdown 2>&1 | tail -20`
Expected: PASS（3 个测试全过）。

- [ ] **Step 7: 提交**

```bash
cargo fmt --all
git add Cargo.toml Cargo.lock src/markdown.rs src/main.rs
git commit -m "feat(update): release notes markdown 解析为块序列 (pulldown-cmark)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Slint —— NoteBlock 结构、属性、块渲染、自适应尺寸

**Files:**
- Modify: `ui/app.slint:13`（import 加 `ScrollView`）
- Modify: `ui/app.slint:37` 后（新增 `NoteBlock` 结构）
- Modify: `ui/app.slint:130` 后（新增 `update-note-blocks` 属性）
- Modify: `ui/app.slint:1174-1175`（弹窗尺寸）
- Modify: `ui/app.slint:1202-1212`（说明区渲染）

> 说明：`.slint` 无单元测试，验证靠 `cargo build`（`slint-build` 在构建期编译并校验 .slint 的语法/类型）。本任务结束时不接线 Rust，弹窗说明区运行时为空属正常，下一任务补齐。

- [ ] **Step 1: import 加入 ScrollView**

把 `ui/app.slint:13`：

```slint
import { TextEdit, Palette, ListView } from "std-widgets.slint";
```

改为：

```slint
import { TextEdit, Palette, ListView, ScrollView } from "std-widgets.slint";
```

- [ ] **Step 2: 新增 NoteBlock 结构**

在 `ui/app.slint` 的 `TransferInfo` 结构闭合 `}`（第 37 行）之后、`TerminalState`（第 39 行）之前插入：

```slint
export struct NoteBlock {
    kind: string,    // "para" | "h" | "ul" | "ol" | "code" | "hr"
    text: string,
    level: int,      // 标题级别 1..6；或列表缩进深度
    marker: string,  // 有序 "1."；无序 "•"；其余空
}
```

- [ ] **Step 3: 新增模型属性**

在 `ui/app.slint:130` 的 `in property <string> update-notes;` 之后插入一行（旧属性暂留，Task 3 移除）：

```slint
    in property <[NoteBlock]> update-note-blocks;   // 解析后的 release notes 块
```

- [ ] **Step 4: 弹窗尺寸自适应**

把 `ui/app.slint:1174-1175`：

```slint
            width: 420px;
            height: 460px;
```

改为：

```slint
            width: min(parent.width * 0.92, 560px);
            height: min(parent.height * 0.92, 600px);
```

- [ ] **Step 5: 说明区改为按块渲染**

把 `ui/app.slint:1202-1212` 整段（从 `// 更新说明（可滚动）` 注释到 `Flickable { … }` 闭合）：

```slint
                // 更新说明（可滚动）
                Flickable {
                    vertical-stretch: 1;
                    viewport-width: self.width;
                    viewport-height: max(self.height, notes-text.preferred-height);
                    notes-text := Text {
                        text: root.update-notes;
                        color: Theme.text-primary;
                        font-size: Theme.fs-sm;
                        wrap: word-wrap;
                    }
                }
```

替换为：

```slint
                // 更新说明（markdown 块级渲染，可滚动）
                ScrollView {
                    vertical-stretch: 1;
                    VerticalLayout {
                        width: parent.visible-width;
                        spacing: 6px;
                        for b in root.update-note-blocks : VerticalLayout {
                            // 每块一个 VerticalLayout 容器：只有一个 if 命中，高度自适应。
                            if b.kind == "h" : Text {
                                text: b.text;
                                color: Theme.text-primary;
                                font-weight: 700;
                                font-size: b.level <= 1
                                    ? Theme.fs-lg
                                    : (b.level == 2 ? Theme.fs-md : Theme.fs-sm);
                                wrap: word-wrap;
                            }
                            if b.kind == "para" : Text {
                                text: b.text;
                                color: Theme.text-primary;
                                font-size: Theme.fs-sm;
                                wrap: word-wrap;
                            }
                            if b.kind == "ul" || b.kind == "ol" : HorizontalLayout {
                                padding-left: b.level * 14px;
                                spacing: 6px;
                                Text {
                                    text: b.marker;
                                    color: Theme.text-secondary;
                                    font-size: Theme.fs-sm;
                                }
                                Text {
                                    text: b.text;
                                    color: Theme.text-primary;
                                    font-size: Theme.fs-sm;
                                    wrap: word-wrap;
                                    horizontal-stretch: 1;
                                }
                            }
                            if b.kind == "code" : Rectangle {
                                background: Theme.bg-panel-alt;
                                border-radius: Theme.radius-sm;
                                VerticalLayout {
                                    padding: 8px;
                                    Text {
                                        text: b.text;
                                        font-family: Theme.font-mono;
                                        font-size: Theme.fs-sm;
                                        color: Theme.text-primary;
                                        wrap: word-wrap;
                                    }
                                }
                            }
                            if b.kind == "hr" : Rectangle {
                                height: 1px;
                                background: Theme.border-subtle;
                            }
                        }
                    }
                }
```

- [ ] **Step 6: 编译确认通过**

Run: `cargo build 2>&1 | tail -20`
Expected: 编译成功（app.rs 仍引用 `update-notes`，旧属性尚在，故链接正常；新弹窗渲染 `update-note-blocks`，运行时暂为空）。

- [ ] **Step 7: 提交**

```bash
git add ui/app.slint
git commit -m "feat(update): 弹窗说明区按 markdown 块渲染 + 尺寸随窗口自适应

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Rust 接线 —— 映射模型、替换调用点、移除死属性

**Files:**
- Modify: `src/app.rs`（新增 `notes_blocks_model` 辅助函数 + 替换两处调用）
- Modify: `ui/app.slint:130`（移除已无引用的 `update-notes` 属性）

- [ ] **Step 1: 新增 `notes_blocks_model` 辅助函数**

在 `src/app.rs` 的 `empty_model`（约第 514 行）函数之后新增（`NoteBlock` 由 `slint::include_modules!()` 在本模块直接可用）：

```rust
/// 把 markdown 文本解析成 NoteBlock 模型，供更新弹窗的说明区渲染。
fn notes_blocks_model(md: &str) -> ModelRc<NoteBlock> {
    let rows: Vec<NoteBlock> = crate::markdown::notes_to_blocks(md)
        .into_iter()
        .map(|b| NoteBlock {
            kind: b.kind.into(),
            text: b.text.into(),
            level: b.level,
            marker: b.marker.into(),
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}
```

- [ ] **Step 2: 替换 show_release 里的设值**

把 `src/app.rs:1993`：

```rust
        w.set_update_notes(rel.notes.clone().into());
```

改为：

```rust
        w.set_update_note_blocks(notes_blocks_model(&rel.notes));
```

- [ ] **Step 3: 替换 GuidedManual 分支的设值**

把 `src/app.rs:2152-2155`：

```rust
                                        w.set_update_notes(crate::i18n::t(
                                            "请将 LibSSH 拖到「应用程序」文件夹以完成更新。",
                                            "Drag LibSSH into the Applications folder to finish updating.",
                                        ).into());
```

改为：

```rust
                                        w.set_update_note_blocks(notes_blocks_model(&crate::i18n::t(
                                            "请将 LibSSH 拖到「应用程序」文件夹以完成更新。",
                                            "Drag LibSSH into the Applications folder to finish updating.",
                                        )));
```

- [ ] **Step 4: 移除已无引用的旧属性**

确认无引用后删除 `ui/app.slint:130` 的：

```slint
    in property <string> update-notes;           // release notes
```

Run（确认确无其它引用）：`grep -rn "update-notes\|update_notes\|set_update_notes" ui src` 应只剩本步要删的这一行（删除后应无任何命中）。

- [ ] **Step 5: 编译确认通过**

Run: `cargo build 2>&1 | tail -20`
Expected: 编译成功，无 `update-notes`/`set_update_notes` 相关错误。

- [ ] **Step 6: 全量测试**

Run: `cargo test 2>&1 | tail -20`
Expected: 含 `markdown` 模块在内全部通过。

- [ ] **Step 7: 提交**

```bash
cargo fmt --all
git add src/app.rs ui/app.slint
git commit -m "feat(update): 接线 NoteBlock 模型并移除纯文本 update-notes 属性

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: 手动验证（真实弹窗）

> 当前包版本 `0.2.7`（Cargo.toml）低于线上最新 release `v0.2.9`，故「检查更新」会拉到真实的 release notes（含自动插入的「macOS 安装提示」：`## 标题` + 有序列表 + 行内代码 `xattr…` + `**Full Changelog**` 链接）——正是要验证的内容。无需改版本号。

- [ ] **Step 1: 启动并触发更新弹窗**

Run: `cargo run`
操作：菜单/按钮触发「检查更新」（对应 `check-update-manual()`，UI 入口见 `ui/app.slint:1029`）。

- [ ] **Step 2: 核对渲染**

确认：
- 标题「macOS 安装提示」加粗放大，**无字面 `##`**；
- 三步为带 `1. 2. 3.` 序号的列表，**无字面 `1.` 之外的 markdown 符号**；
- `xattr -dr …` 等命令清晰可读（行内代码已去反引号）；
- 末行显示「Full Changelog: https://…」，**无字面 `**`**。

- [ ] **Step 3: 核对溢出与滚动**

- 缩小应用窗口到接近最小尺寸，弹窗不超出窗口（被限制在窗口约 92%）、按钮始终可见；
- 说明较长时，说明区出现滚动条且可滚动浏览全文；
- 明亮 / 暗夜两种主题下，代码块底色、文字颜色均正常。

- [ ] **Step 4: （可选）短文本回归**

若有条件，构造一条仅一行的 notes（或临时在 `show_release` 传入单行字符串）确认其渲染为单个段落、弹窗不显异常。验证后还原任何临时改动。

> 本任务为验证，不产生提交；若发现需要微调（如标题字号、缩进、代码块换行），回到对应任务修正后重新验证。

---

## 自查（Self-Review）

**Spec 覆盖：**
- 块级渲染（Rust 解析 + 原生元素）→ Task 1 + Task 2 ✓
- 内联去符号保留文字（粗体/行内代码/链接）→ Task 1（`Event::Code` 保留文字、Strong/Emphasis/Link 标记忽略）+ 测试断言 ✓
- 块类型 para/h/ul/ol/code/hr → Task 1 解析 + Task 2 渲染，`kind` 取值两侧一致 ✓
- 自适应窗口 + 可靠滚动 → Task 2 Step 4（尺寸）+ Step 5（ScrollView）✓
- 数据流替换两处 `set_update_notes` → Task 3 Step 2/3 ✓
- 错误/降级（空输入、容错）→ Task 1 `divider_and_empty` 测试 + pulldown-cmark 容错 ✓
- 测试（真实 macOS 提示段 + 列表/代码块/空输入）→ Task 1 三个测试 ✓；UI 手动验证 → Task 4 ✓

**占位符扫描：** 无 TBD/TODO；每个代码步骤均含完整代码与可运行命令。

**类型一致性：** `Block{kind:&'static str,text:String,level:i32,marker:String}`（Task 1）↔ `NoteBlock{kind,text,level,marker}`（Task 2 Slint / Task 3 映射）字段名与语义一致；`notes_to_blocks`、`notes_blocks_model`、`set_update_note_blocks`、`update-note-blocks` 命名贯穿一致。

**范围：** 聚焦单一弹窗渲染与尺寸，适合单个计划。
