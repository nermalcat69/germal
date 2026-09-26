//! `.germal` 文件：抓取到的数据先 zstd 压缩、再用 AES-256-GCM 加密。
//!
//! 顺序是「先压缩、后加密」，不是「先加密、后压缩」：密文与随机数据无异，压缩它什么也省不下来，
//! 文件会跟明文一样大。数据落盘（静态存储）时先压后加没有额外风险；压缩侧信道（CRIME 类）
//! 要求攻击者能往同一份明文里塞内容并观察长度，这里不成立。
//!
//! 布局：`"GERMAL"` | 版本(1) | argon2 盐(16) | GCM nonce(12) | 密文 + 16 字节认证标签。
//! 头部整体作为 AAD 参与认证，篡改头部与篡改密文一样解不开。
//! 密钥 = Argon2id(口令, 盐)，参数取 argon2 crate 默认值（v1 固化，改参数要升版本号）。
//!
//! ponytail: 整个文件在内存里处理，GB 级数据要改成分块流式（每块独立 nonce）。

use aes_gcm::aead::{Aead, OsRng, Payload, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{Context, Result, bail};
use argon2::Argon2;

const MAGIC: &[u8; 6] = b"GERMAL";
const VERSION: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = MAGIC.len() + 1 + SALT_LEN + NONCE_LEN;
const ZSTD_LEVEL: i32 = 19;

fn cipher(passphrase: &str, salt: &[u8]) -> Result<Aes256Gcm> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("key derivation failed: {e}"))?;
    Ok(Aes256Gcm::new_from_slice(&key).expect("32-byte key"))
}

pub fn seal(plain: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    if passphrase.is_empty() {
        bail!("empty passphrase");
    }
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.push(VERSION);
    header.resize(HEADER_LEN, 0);
    OsRng.fill_bytes(&mut header[MAGIC.len() + 1..]);
    let salt = &header[MAGIC.len() + 1..MAGIC.len() + 1 + SALT_LEN];
    let nonce = &header[HEADER_LEN - NONCE_LEN..];

    let compressed = zstd::encode_all(plain, ZSTD_LEVEL)?;
    let ct = cipher(passphrase, salt)?
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: &compressed,
                aad: &header,
            },
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    header.extend_from_slice(&ct);
    Ok(header)
}

pub fn open(file: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    if file.len() < HEADER_LEN || &file[..MAGIC.len()] != MAGIC {
        bail!("not a .germal file");
    }
    let version = file[MAGIC.len()];
    if version != VERSION {
        bail!("unsupported .germal version {version}");
    }
    let (header, ct) = file.split_at(HEADER_LEN);
    let salt = &header[MAGIC.len() + 1..MAGIC.len() + 1 + SALT_LEN];
    let nonce = &header[HEADER_LEN - NONCE_LEN..];
    // GCM 标签校验失败 = 口令错误或文件被改动，两者无法区分
    let compressed = cipher(passphrase, salt)?
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: header,
            },
        )
        .map_err(|_| anyhow::anyhow!("wrong passphrase or corrupted file"))?;
    zstd::decode_all(&compressed[..]).context("decompression failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_shrinks() {
        let plain = "{\"user\":\"alice\",\"token\":\"abc\"}\n"
            .repeat(5000)
            .into_bytes();
        let sealed = seal(&plain, "hunter2").unwrap();
        assert!(
            sealed.len() < plain.len() / 10,
            "{} vs {}",
            sealed.len(),
            plain.len()
        );
        assert_eq!(open(&sealed, "hunter2").unwrap(), plain);
        assert!(!sealed.windows(5).any(|w| w == b"alice"));
    }

    #[test]
    fn wrong_passphrase_and_tampering_fail() {
        let sealed = seal(b"secret data", "right").unwrap();
        assert!(open(&sealed, "wrong").is_err());
        for i in [0, MAGIC.len() + 2, HEADER_LEN + 1, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert!(open(&bad, "right").is_err(), "flip at {i}");
        }
        assert!(open(b"nope", "right").is_err());
    }
}
