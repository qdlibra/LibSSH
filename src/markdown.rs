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
    if matches!(mode, Mode::None) {
        buf.clear();
        return;
    }
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
        Mode::Item {
            kind,
            level,
            marker,
        } => {
            let t = raw.trim().to_string();
            if !t.is_empty() {
                blocks.push(Block {
                    kind,
                    text: t,
                    level,
                    marker,
                });
            }
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
                mode = Mode::Item {
                    kind,
                    level,
                    marker,
                };
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

    #[test]
    fn code_block_preserves_internal_newlines() {
        let md = "```\nline1\nline2\n```\n";
        let b = notes_to_blocks(md);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, "code");
        assert_eq!(b[0].text, "line1\nline2");
    }
}
