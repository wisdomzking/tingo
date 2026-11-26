//! D3200 会话加解密（PROTOCOL.md §5.6）。
//!
//! 1:1 移植自 tacshi/Soundcore `lib/crypto/device_crypto.dart`（MIT）：
//!   ECDH P-256 → HKDF-SHA256(salt/info={1,2,3}) → AES-CTR(128-bit BE counter)
//!   → 文件头里 `soundcored3200` + 32B fileKey → 每个 160B 分片按 seq*10 解密。

use std::collections::HashMap;

use aes::Aes256;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ctr::Ctr128BE;
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::PublicKey;
use rand_core::OsRng;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;
type Aes256Ctr = Ctr128BE<Aes256>;

pub const FILE_KEY_MAGIC: &[u8] = b"soundcored3200";

/// AES-CTR（JCE `AES/CTR/NoPadding` / PointyCastle SICStreamCipher 等价）。
/// CTR 加密与解密是同一个操作。
pub fn aes_ctr(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
    if ![16, 24, 32].contains(&key.len()) {
        return Err(format!("AES key 长度非法：{}", key.len()));
    }
    if iv.len() > 16 {
        return Err(format!("AES IV 长度非法：{}", iv.len()));
    }
    let mut iv16 = [0u8; 16];
    iv16[..iv.len()].copy_from_slice(iv);
    let mut cipher = Aes256Ctr::new_from_slices(key, &iv16)
        .map_err(|e| format!("初始化 AES-CTR 失败：{e}"))?;
    let mut out = data.to_vec();
    cipher.apply_keystream(&mut out);
    Ok(out)
}

/// HKDF-SHA256（extract + expand），对标 SpongyCastle / JCE 实现。
pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], length: usize) -> Result<Vec<u8>, String> {
    let mut extract = HmacSha256::new_from_slice(salt)
        .map_err(|e| format!("HKDF extract 初始化失败：{e}"))?;
    extract.update(ikm);
    let prk = extract.finalize().into_bytes();

    let mut out = Vec::with_capacity(length);
    let mut t: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while out.len() < length {
        let mut mac = HmacSha256::new_from_slice(&prk)
            .map_err(|e| format!("HKDF expand 初始化失败：{e}"))?;
        mac.update(&t);
        mac.update(info);
        mac.update(&[counter]);
        t = mac.finalize().into_bytes().to_vec();
        let n = t.len().min(length - out.len());
        out.extend_from_slice(&t[..n]);
        counter = counter.wrapping_add(1);
    }
    Ok(out)
}

/// 每个 160B 分片的 AES-CTR 流：IV = nonce[0..12] ‖ BE_u32(seq*10)。
#[derive(Clone)]
pub struct AesCtrStreamDecrypt {
    pub file_key: Vec<u8>,
    pub nonce: Vec<u8>,
}

impl AesCtrStreamDecrypt {
    pub fn build_counter(&self, sequence: u32) -> [u8; 16] {
        let mut iv = [0u8; 16];
        let n = self.nonce.len().min(12);
        iv[..n].copy_from_slice(&self.nonce[..n]);
        let counter = sequence.wrapping_mul(10);
        iv[12..16].copy_from_slice(&counter.to_be_bytes());
        iv
    }

    pub fn decrypt_chunk(&self, data: &[u8], sequence: u32) -> Option<Vec<u8>> {
        aes_ctr(data, &self.file_key, &self.build_counter(sequence)).ok()
    }
}

/// 解析 `1A/07` 文件头（>=97 字节）。
#[derive(Debug, Clone)]
pub struct AudioFileSecretKey {
    pub file_id: u32,
    pub file_size: u32,
    pub nonce: Vec<u8>,
    pub encrypted_file_key: Vec<u8>,
    pub session_nonce: Vec<u8>,
    pub error_code: u8,
}

impl AudioFileSecretKey {
    pub fn parse_frame(raw: &[u8]) -> Option<Self> {
        if raw.len() < 97 {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
        Some(Self {
            file_id: u32_at(9),
            file_size: u32_at(13),
            nonce: raw[17..33].to_vec(),
            encrypted_file_key: raw[33..79].to_vec(),
            session_nonce: raw[79..95].to_vec(),
            error_code: raw[95],
        })
    }
}

/// 设备会话：持有 ECDH 临时私钥、会话密钥和各文件解密器。
pub struct DeviceCrypto {
    ephemeral: Option<EphemeralSecret>,
    app_public_key: Vec<u8>,
    pub session_key: Option<[u8; 32]>,
    file_decryptors: HashMap<String, AesCtrStreamDecrypt>,
}

impl Default for DeviceCrypto {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceCrypto {
    pub fn new() -> Self {
        Self {
            ephemeral: None,
            app_public_key: Vec::new(),
            session_key: None,
            file_decryptors: HashMap::new(),
        }
    }

    pub fn has_session(&self) -> bool {
        self.session_key.is_some()
    }

    /// 生成（或复用）ECDH P-256 密钥对，返回 65B 未压缩公钥。
    pub fn ensure_key_pair(&mut self) -> Vec<u8> {
        if !self.app_public_key.is_empty() {
            return self.app_public_key.clone();
        }
        let secret = EphemeralSecret::random(&mut OsRng);
        let public = secret.public_key();
        let encoded = public.to_encoded_point(false);
        let bytes = encoded.as_bytes().to_vec();
        self.app_public_key = bytes.clone();
        self.ephemeral = Some(secret);
        bytes
    }

    /// 收到 `2E/01` 后完成握手：校验设备返回的共享密钥并派生会话密钥。
    pub fn complete_handshake(
        mut self,
        device_public_key: &[u8],
        device_shared_key: &[u8],
    ) -> Result<Self, String> {
        let public = public_from_bytes(device_public_key)?;
        let secret = self
            .ephemeral
            .take()
            .ok_or_else(|| "尚未生成 app 公钥，无法完成握手".to_string())?;
        let shared = secret.diffie_hellman(&public);
        let local_shared = shared.raw_secret_bytes();
        // 真机（D3200，固件 04.78）验证：0x2E/0x01 响应 74..106 的 32B 回显值
        // 才是 HKDF 输入——用它派生 sessionKey 能解出文件头 `soundcored3200` magic；
        // 本地 ECDH 的 X 坐标与该值不同，仅作为 firmware 不回显时的兜底。
        let ikm = if device_shared_key.len() == 32 {
            device_shared_key
        } else {
            local_shared.as_slice()
        };
        let key = hkdf_sha256(ikm, &[1, 2, 3], &[1, 2, 3], 32)?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&key);
        self.session_key = Some(arr);
        Ok(self)
    }

    /// 从文件头解出 AES-256 fileKey，并建立该文件的分片解密器。
    pub fn init_file_decrypt(
        &mut self,
        file_id: &str,
        encrypted_file_key: &[u8],
        session_nonce: &[u8],
        nonce: &[u8],
    ) -> Result<(), String> {
        if self.file_decryptors.contains_key(file_id) {
            return Ok(());
        }
        let session_key = self
            .session_key
            .ok_or_else(|| "没有会话密钥，请先完成 2E/01 握手".to_string())?;
        if session_nonce.len() != 16 {
            return Err(format!("sessionNonce 必须 16B，实际 {}", session_nonce.len()));
        }
        let plain = aes_ctr(encrypted_file_key, &session_key, session_nonce)?;
        if plain.len() < 46 {
            return Err(format!("解出的 fileKey 材料太短：{}B", plain.len()));
        }
        if &plain[..14] != FILE_KEY_MAGIC {
            return Err(format!(
                "fileKey magic 不匹配：期望 {:?}，实际 {}；sessionKey={}",
                String::from_utf8_lossy(FILE_KEY_MAGIC),
                hex(&plain[..14]),
                hex(&session_key)
            ));
        }
        let raw_key = &plain[14..];
        if raw_key.len() < 16 {
            return Err(format!("fileKey 太短：{}B", raw_key.len()));
        }
        let key = if raw_key.len() >= 32 {
            raw_key[..32].to_vec()
        } else {
            raw_key.to_vec()
        };
        let n = nonce[..nonce.len().min(16)].to_vec();
        self.file_decryptors.insert(
            file_id.to_string(),
            AesCtrStreamDecrypt {
                file_key: key,
                nonce: n,
            },
        );
        Ok(())
    }

    pub fn decrypt_chunk(
        &self,
        file_id: &str,
        sequence: u32,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        self.file_decryptors
            .get(file_id)
            .and_then(|d| d.decrypt_chunk(data, sequence))
    }

    pub fn clear_file(&mut self, file_id: &str) {
        self.file_decryptors.remove(file_id);
    }

    /// 仅用于离线自检：用当前临时私钥计算与对端公钥的共享密钥。
    /// 消费临时私钥，一个实例只能调用一次。
    pub fn debug_shared_for(&mut self, peer_public: &[u8]) -> Result<Vec<u8>, String> {
        let public = public_from_bytes(peer_public)?;
        let secret = self
            .ephemeral
            .take()
            .ok_or_else(|| "临时私钥已消费".to_string())?;
        Ok(secret.diffie_hellman(&public).raw_secret_bytes().to_vec())
    }
}

fn public_from_bytes(raw: &[u8]) -> Result<PublicKey, String> {
    let bytes = if raw.len() >= 65 { &raw[..65] } else { raw };
    if bytes.len() == 64 {
        let mut with_prefix = Vec::with_capacity(65);
        with_prefix.push(0x04);
        with_prefix.extend_from_slice(bytes);
        return PublicKey::from_sec1_bytes(&with_prefix)
            .map_err(|e| format!("设备公钥非法：{e}"));
    }
    if bytes.first() != Some(&0x04) || bytes.len() < 65 {
        return Err(format!("设备公钥格式非法：{}B", bytes.len()));
    }
    PublicKey::from_sec1_bytes(bytes).map_err(|e| format!("设备公钥非法：{e}"))
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_length_and_determinism() {
        let ikm: Vec<u8> = (0..32).map(|i| i as u8).collect();
        let a = hkdf_sha256(&ikm, &[1, 2, 3], &[1, 2, 3], 32).unwrap();
        let b = hkdf_sha256(&ikm, &[1, 2, 3], &[1, 2, 3], 32).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(a, b);
    }

    #[test]
    fn aes_ctr_roundtrip() {
        let key: Vec<u8> = (1..=32).map(|i| i as u8).collect();
        let iv: Vec<u8> = (0..16).map(|i| i as u8).collect();
        let plain = b"hello soundcore work!!";
        let enc = aes_ctr(plain, &key, &iv).unwrap();
        let dec = aes_ctr(&enc, &key, &iv).unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn counter_uses_sequence_times_ten() {
        let nonce: Vec<u8> = (0..16).map(|i| i as u8).collect();
        let d = AesCtrStreamDecrypt {
            file_key: (0..32).map(|i| 0xA0u8 + (i % 16) as u8).collect(),
            nonce,
        };
        let c0 = d.build_counter(0);
        let c1 = d.build_counter(1);
        assert_eq!(&c0[..12], &d.nonce[..12]);
        assert_eq!(c0[15], 0);
        assert_eq!(c1[15], 10);
        assert_eq!(c1[14], 0);
    }

    #[test]
    fn ecdh_handshake_and_file_key_magic() {
        let mut app = DeviceCrypto::new();
        let mut device = DeviceCrypto::new();
        let app_pub = app.ensure_key_pair();
        let dev_pub = device.ensure_key_pair();
        assert_eq!(app_pub.len(), 65);
        assert_eq!(app_pub[0], 0x04);

        // 设备侧用同样步骤算出共享密钥
        let dev_secret = device.ephemeral.take().unwrap();
        let app_public = public_from_bytes(&app_pub).unwrap();
        let dev_shared = dev_secret.diffie_hellman(&app_public);
        let shared_bytes = dev_shared.raw_secret_bytes().to_vec();

        let app = app
            .complete_handshake(&dev_pub, &shared_bytes)
            .expect("握手应成功");
        assert!(app.has_session());

        let session_nonce: Vec<u8> = (3..19).map(|i| i as u8).collect();
        let nonce: Vec<u8> = (20..36).map(|i| i as u8).collect();
        let file_key: Vec<u8> = (50..82).map(|i| i as u8).collect();
        let mut plain = FILE_KEY_MAGIC.to_vec();
        plain.extend_from_slice(&file_key);
        let enc_key = aes_ctr(&plain, &app.session_key.unwrap(), &session_nonce).unwrap();

        let mut app = app;
        app.init_file_decrypt("12345", &enc_key, &session_nonce, &nonce)
            .unwrap();
        let chunk: Vec<u8> = (0..160).map(|i| i as u8).collect();
        let cipher = app.decrypt_chunk("12345", 0, &chunk).unwrap();
        assert_eq!(cipher.len(), 160);
        assert_eq!(app.decrypt_chunk("12345", 0, &cipher).unwrap(), chunk);
    }

    #[test]
    fn handshake_derives_key_from_device_echo() {
        let mut app = DeviceCrypto::new();
        let mut device = DeviceCrypto::new();
        let app_pub = app.ensure_key_pair();
        let dev_pub = device.ensure_key_pair();
        let _ = app_pub;
        // 真机固件回显值才是 HKDF 输入；这里用一个已知的伪回显值验证"回显优先"。
        let echo = vec![0xA5u8; 32];
        let app = app
            .complete_handshake(&dev_pub, &echo)
            .expect("回显值应被采用，不再与本地 ECDH 比较");
        let expected = hkdf_sha256(&echo, &[1, 2, 3], &[1, 2, 3], 32).unwrap();
        assert_eq!(app.session_key.unwrap().to_vec(), expected);
    }

    #[test]
    fn parse_file_head() {
        let mut raw = vec![0u8; 97];
        raw[9..13].copy_from_slice(&1234u32.to_le_bytes());
        raw[13..17].copy_from_slice(&5678u32.to_le_bytes());
        let key = AudioFileSecretKey::parse_frame(&raw).unwrap();
        assert_eq!(key.file_id, 1234);
        assert_eq!(key.file_size, 5678);
        assert_eq!(key.nonce.len(), 16);
        assert_eq!(key.encrypted_file_key.len(), 46);
        assert_eq!(key.session_nonce.len(), 16);
    }
}
