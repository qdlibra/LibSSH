//! 主机密钥（known_hosts）存储与验证。
//!
//! 我们维护应用自己的 `known_hosts` 文件（位于 LibSSH 配置目录），而不去碰用户的
//! `~/.ssh/known_hosts`，使 GUI 的信任决定与其命令行设置相互隔离。每行一条记录：
//! `host\x1fport\x1falgo\x1fbase64key`，字段以 US(0x1f) 分隔。
//!
//! 验证是对已解析记录的纯查找；IO（读取/追加）被隔离在小函数里，使解析/匹配逻辑
//! 可被单元测试覆盖。

use base64::Engine;
use russh::keys::PublicKey;
use std::path::PathBuf;

/// 把所呈现的主机密钥与本地存储比对的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// 存储中完全没有该主机——首次见到。
    Unknown,
    /// 主机已存在且密钥一致——可静默放行。
    Match,
    /// 主机已存在但密钥不同——可能是中间人攻击，或服务器合法换了密钥。
    Mismatch,
}

/// 计算密钥的 SSH 风格 SHA256 指纹（`SHA256:base64nopad`）。
pub fn fingerprint(key: &PublicKey) -> String {
    use sha2::{Digest, Sha256};
    let blob = key.to_bytes().unwrap_or_default();
    let digest = Sha256::digest(&blob);
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest);
    format!("SHA256:{b64}")
}

/// known_hosts 文件路径：`<config_dir>/known_hosts`（与 sessions.json 同目录）。
fn known_hosts_path() -> Option<PathBuf> {
    let proj = directories::ProjectDirs::from("", "", "LibSSH")?;
    Some(proj.config_dir().join("known_hosts"))
}

/// 把密钥编码为 (算法名, base64 密钥)。
fn encode_key(key: &PublicKey) -> (String, String) {
    let algo = key.algorithm().to_string();
    let blob = key.to_bytes().unwrap_or_default();
    let b64 = base64::engine::general_purpose::STANDARD.encode(blob);
    (algo, b64)
}

/// 从 known_hosts 文件解析出的一条记录。
struct Record {
    host: String,
    port: u16,
    algo: String,
    key_b64: String,
}

fn parse_line(line: &str) -> Option<Record> {
    let mut parts = line.split('\x1f');
    let host = parts.next()?.to_string();
    let port: u16 = parts.next()?.parse().ok()?;
    let algo = parts.next()?.to_string();
    let key_b64 = parts.next()?.to_string();
    Some(Record {
        host,
        port,
        algo,
        key_b64,
    })
}

fn match_record(
    records: &[Record],
    host: &str,
    port: u16,
    algo: &str,
    key_b64: &str,
) -> HostKeyStatus {
    let host_port_matches: Vec<&Record> = records
        .iter()
        .filter(|r| r.host == host && r.port == port)
        .collect();
    if host_port_matches.is_empty() {
        return HostKeyStatus::Unknown;
    }
    // 同一 host:port 存在记录。任一记录的 算法+密钥 一致即视为 Match。
    if host_port_matches
        .iter()
        .any(|r| r.algo == algo && r.key_b64 == key_b64)
    {
        HostKeyStatus::Match
    } else {
        HostKeyStatus::Mismatch
    }
}

/// 验证 `host:port` 所呈现的密钥与本地存储的关系。
pub fn verify(host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    let (algo, key_b64) = encode_key(key);
    let Some(path) = known_hosts_path() else {
        return HostKeyStatus::Unknown;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HostKeyStatus::Unknown;
    };
    let records: Vec<Record> = content.lines().filter_map(parse_line).collect();
    match_record(&records, host, port, &algo, &key_b64)
}

/// 记住（追加）一条受信任的主机密钥。
///
/// 注意：以追加方式写入，绝不覆盖已有记录（meatshell 原实现误用 `std::fs::write`
/// 覆盖整个文件、丢失全部历史；此处修正为 `append`）。
pub fn remember(host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    use std::io::Write;
    let (algo, key_b64) = encode_key(key);
    let Some(path) = known_hosts_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = format!("{host}\x1f{port}\x1f{algo}\x1f{key_b64}\n");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_extracts_fields_and_rejects_short_lines() {
        let r = parse_line("example.com\x1f22\x1fssh-ed25519\x1fAAAAbase64key").unwrap();
        assert_eq!(r.host, "example.com");
        assert_eq!(r.port, 22);
        assert_eq!(r.algo, "ssh-ed25519");
        assert_eq!(r.key_b64, "AAAAbase64key");
        // 字段不足或端口非法 → None。
        assert!(parse_line("only\x1ftwo").is_none());
        assert!(parse_line("h\x1fNaN\x1falgo\x1fkey").is_none());
    }

    #[test]
    fn match_record_distinguishes_unknown_match_mismatch() {
        let recs = vec![Record {
            host: "h".into(),
            port: 22,
            algo: "ssh-ed25519".into(),
            key_b64: "KEY1".into(),
        }];
        // 未知主机。
        assert_eq!(
            match_record(&recs, "other", 22, "ssh-ed25519", "KEY1"),
            HostKeyStatus::Unknown
        );
        // 已知且密钥一致。
        assert_eq!(
            match_record(&recs, "h", 22, "ssh-ed25519", "KEY1"),
            HostKeyStatus::Match
        );
        // 已知但密钥不同 → 可能 MITM。
        assert_eq!(
            match_record(&recs, "h", 22, "ssh-ed25519", "KEY2"),
            HostKeyStatus::Mismatch
        );
        // 同主机不同端口视为未知。
        assert_eq!(
            match_record(&recs, "h", 2222, "ssh-ed25519", "KEY1"),
            HostKeyStatus::Unknown
        );
    }
}
