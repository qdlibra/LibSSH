//! 应用内运行日志缓冲：捕获 `tracing` 的 WARN/ERROR 事件，供「运行日志」浮层显示。
//!
//! release 版在 Windows 下没有控制台（`windows_subsystem = "windows"`），stderr 日志
//! 完全不可见；此环形缓冲是用户排查运行期错误的唯一途径。设计成「stderr 层照旧 +
//! 额外一层写入本缓冲」，互不影响。

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::writer::MakeWriter;

/// 共享日志缓冲（最近若干行 WARN/ERROR，已格式化为纯文本）。
pub type LogBuffer = Arc<Mutex<VecDeque<String>>>;

/// 缓冲容量上限：仅保留最近这么多行，防止长时间运行内存无界增长。
pub const LOG_CAPACITY: usize = 500;

/// 新建一个空日志缓冲。
pub fn new_buffer() -> LogBuffer {
    Arc::new(Mutex::new(VecDeque::with_capacity(64)))
}

/// 向缓冲追加一行；超过 `cap` 时丢弃最旧的行（环形）。空行忽略。
/// 抽成纯函数便于单测「只进不出会撑爆内存」这一不变量。
pub fn push_line(buf: &LogBuffer, line: String, cap: usize) {
    if line.is_empty() {
        return;
    }
    if let Ok(mut q) = buf.lock() {
        while q.len() >= cap {
            q.pop_front();
        }
        q.push_back(line);
    }
}

/// 取出缓冲当前所有行的快照（最旧在前、最新在后），用于刷新 UI。
pub fn snapshot(buf: &LogBuffer) -> Vec<String> {
    buf.lock()
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default()
}

/// 清空缓冲。
pub fn clear(buf: &LogBuffer) {
    if let Ok(mut q) = buf.lock() {
        q.clear();
    }
}

/// `tracing` fmt 层使用的 Writer：把格式化后的一行日志写入共享缓冲，而非控制台。
pub struct BufferWriter {
    buf: LogBuffer,
}

impl io::Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // fmt 层每条事件调用一次 write，`bytes` 是格式化后的整行（含尾随换行）。
        if let Ok(text) = std::str::from_utf8(bytes) {
            push_line(&self.buf, text.trim_end().to_string(), LOG_CAPACITY);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `MakeWriter` 工厂：每条事件产生一个写入共享缓冲的 [`BufferWriter`]。
pub struct BufferMakeWriter {
    buf: LogBuffer,
}

impl BufferMakeWriter {
    pub fn new(buf: LogBuffer) -> Self {
        Self { buf }
    }
}

impl<'a> MakeWriter<'a> for BufferMakeWriter {
    type Writer = BufferWriter;
    fn make_writer(&'a self) -> Self::Writer {
        BufferWriter {
            buf: self.buf.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_line_evicts_oldest_beyond_capacity() {
        let buf = new_buffer();
        for i in 0..5 {
            push_line(&buf, format!("line{i}"), 3);
        }
        // 只保留最近 3 行，最旧的 line0/line1 被丢弃。
        assert_eq!(snapshot(&buf), vec!["line2", "line3", "line4"]);
    }

    #[test]
    fn push_line_ignores_empty() {
        let buf = new_buffer();
        push_line(&buf, String::new(), 10);
        assert!(snapshot(&buf).is_empty());
    }

    #[test]
    fn clear_empties_buffer() {
        let buf = new_buffer();
        push_line(&buf, "boom".into(), 10);
        clear(&buf);
        assert!(snapshot(&buf).is_empty());
    }

    #[test]
    fn buffer_writer_captures_trimmed_line() {
        let buf = new_buffer();
        let mut w = BufferWriter { buf: buf.clone() };
        use std::io::Write;
        w.write_all(b"WARN something bad\n").unwrap();
        assert_eq!(snapshot(&buf), vec!["WARN something bad"]);
    }
}
