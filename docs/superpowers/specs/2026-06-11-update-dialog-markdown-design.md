# 设计：更新说明 markdown 块级渲染 + 自适应弹窗

日期：2026-06-11
状态：已批准（待实现）

## 背景与问题

软件更新弹窗（`ui/app.slint` 的 update dialog）把 GitHub release 的 `body`（markdown 原文）当**纯文本**显示，用户看到字面的 `##`、`**`、`` ` ``、`1.` 等符号；同时弹窗为**固定 420×460px**，长说明会「超出窗口」、展示效果差。

发布脚本（`.github/workflows/release.yml:233-245`）会在每个 release 说明开头自动插入一段「macOS 安装提示」，含 `## 标题`、有序列表、行内代码（`xattr -dr …` 命令）、`**Full Changelog**` 加链接——这正是最常展示、也最容易溢出和显示难看的内容。

## 关键约束

- 项目锁定 **Slint 1.16.1**（`Cargo.lock`）。Slint 的原生 `StyledText` 元素与 `@markdown()` 宏目前**仅在未发布的 master**，1.16.1 不含，故不可用于发版构建。
- Slint 稳定版 `Text` 元素**无法在一行内混排多种样式**（粗体/常规不能同段混排）。
- 现有代码已使用 `std-widgets` 的 `ScrollView`/`Palette`，并内置 Cascadia Mono 等宽字体（`ui/app.slint:8-9`），可复用。

## 决策

- **渲染方案：块级渲染。** 在 Rust 侧把 markdown 解析成有类型的块序列喂给 Slint，Slint 用原生元素按块渲染。内联标记（粗体/行内代码/链接）**去符号保留文字**。当前 Slint 1.16 即可上线，稳定。（放弃「块级+内联样式」因 Slint 稳定版无内联混排排版、实现复杂易碎；放弃「升级 Slint 原生」因依赖未发布版本、发版风险高。）
- **尺寸/溢出：自适应窗口 + 可靠滚动。** 弹窗宽高随窗口自适应并设上限，说明区为弹性可滚动区，任何窗口尺寸下都不溢出。

## 总体架构

保持「Rust 解析 → Slint 原生渲染」的边界，分三个改动单元：

### (a) Rust 解析模块 `src/markdown.rs`（新增，可独立单测）

- 新增依赖 `pulldown-cmark`（纯 Rust、轻量、无系统依赖，契合项目 RustCrypto/rustls 精简取向）。关闭默认 html 特性，仅用解析器。
- 自有结构（不依赖 Slint 生成类型，便于脱离 UI 单测）：

  ```rust
  pub struct Block {
      pub kind: &'static str, // "para" | "h" | "ul" | "ol" | "code" | "hr"
      pub text: String,
      pub level: i32,         // 标题级别 1..=6；或列表缩进深度（从 0 起）
      pub marker: String,     // 有序项序号 "1." / "2."；无序项 "•"；其余为空
  }

  pub fn notes_to_blocks(md: &str) -> Vec<Block>;
  ```

- 解析规则：
  - 标题 → `kind="h"`，`level` 取标题级别。
  - 无序列表项 → `kind="ul"`，`marker="•"`，`level` 取缩进深度。
  - 有序列表项 → `kind="ol"`，`marker` 为按起始序号递增算出的 `"N."`，`level` 取缩进深度。
  - 围栏/缩进代码块 → `kind="code"`，`text` 为代码原文（多行）。
  - 主题分隔线（`---`）→ `kind="hr"`。
  - 其余文本 → `kind="para"`。
- **内联去符号保留文字**：`**粗体**`→`粗体`、`*斜体*`→`斜体`、`` `行内代码` ``→命令原文、`[文字](url)`→`文字`。GitHub 自动链接本身文本即 URL，故 `**Full Changelog**: https://…` 渲染为干净的一行「Full Changelog: https://…」。

### (b) Slint 结构与渲染（`ui/app.slint`）

- 新增结构与属性：

  ```slint
  export struct NoteBlock { kind: string, text: string, level: int, marker: string }
  // 在 AppWindow 内：
  in property <[NoteBlock]> update-note-blocks;
  ```

- 移除弹窗中对旧 `update-notes` 字符串属性的使用（`ui/app.slint:1207`）。是否保留该属性视实现便利决定，但弹窗不再引用它。
- 把现有「`Flickable { notes-text := Text }`」（`ui/app.slint:1202-1212`）替换为 `ScrollView`（带可见滚动条）内嵌 `VerticalLayout { for b in root.update-note-blocks : … }`，按 `b.kind` 渲染：
  - `"h"`：`Text`，加粗、字号按 `level` 梯度（如 1 级 `Theme.fs-lg`、其余 `Theme.fs-md`），`text-primary`。
  - `"para"`：`Text`，常规字号 `Theme.fs-sm`，`text-primary`。
  - `"ul"` / `"ol"`：`HorizontalLayout`，左侧 `Text { text: b.marker }`，右侧 `Text { text: b.text }`；按 `level` 左缩进。
  - `"code"`：`Rectangle`（浅底色、圆角）内 `Text`，`font-family` 用 Cascadia Mono。
  - `"hr"`：`Rectangle { height: 1px; background: Theme.border-subtle }`。
  - 所有文本 `wrap: word-wrap`。

### (c) 自适应弹窗尺寸（`ui/app.slint`）

- 弹窗外层 Rectangle：`width: min(parent.width * 0.92, 560px); height: min(parent.height * 0.92, 600px);` 仍水平/垂直居中。
- 说明区 `ScrollView` 设 `vertical-stretch: 1`，吸收剩余高度；标题、版本号、进度/错误区、按钮区固定。任何窗口尺寸下弹窗不超出窗口，长说明在说明区内滚动。

## 数据流

```
GitHub release.body (markdown)
  → updater::ReleaseInfo.notes (String)
  → markdown::notes_to_blocks(&notes) -> Vec<Block>
  → app.rs 映射为 Vec<NoteBlock> -> VecModel<NoteBlock> -> ModelRc
  → window.set_update_note_blocks(...)
```

- 替换现 `src/app.rs:1993` 的 `w.set_update_notes(rel.notes.clone().into())`。
- 错误提示分支 `src/app.rs:2152`（检查失败时写入一段提示文本）同样走该管线——纯文本消息会被解析为单个 `"para"` 块，正常渲染。

## 边界与降级

- pulldown-cmark 解析容错：非法/不规范 markdown 不报错，按尽力而为解析。
- `notes` 为空 → 返回空块序列，弹窗其余部分正常显示。
- 嵌套列表仅保留 `level` 缩进层级，不做深层嵌套结构渲染（GitHub 自动说明几乎不用）。
- 代码块超宽采用换行展示，不做横向滚动，避免命令被裁切。

## 测试

- `src/markdown.rs` 单元测试：
  - 用真实「macOS 安装提示」段（`## 标题` + 有序列表 + 行内代码 `xattr…` + `**Full Changelog**` + 链接）断言块序列与各字段。
  - 覆盖：标题级别、无序列表、有序列表序号、代码块、内联去符号（粗体/行内代码/链接）、空输入。
- UI 改动：`cargo build` 通过后，手动触发一次更新弹窗肉眼核对——长说明与短说明、缩小窗口不溢出、说明区可滚动。

## 不在本次范围（YAGNI）

- 链接可点击（块级方案下 `Text` 无逐段可点能力；如需后续单独做）。
- 行内粗体/斜体/行内代码的逐词样式还原。
- 深层嵌套列表的结构化渲染。
