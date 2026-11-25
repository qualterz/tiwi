use std::collections::{HashSet, VecDeque};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const MAX_SEEN_IVS: usize = 512;

#[derive(Debug, Serialize)]
struct EncryptedEnvelope {
    iv: String,
    data: String,
}

pub struct SignalCrypto {
    stream_id: String,
    cipher: Aes256Gcm,
}

impl SignalCrypto {
    pub fn from_token(token: &str) -> Self {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(token.as_bytes())
            .expect("HMAC accepts any key size");
        mac.update(b"stream-id");
        let stream_id = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

        let key = Sha256::digest(token.as_bytes());
        let cipher = Aes256Gcm::new(&key);

        Self { stream_id, cipher }
    }

    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    pub fn offer_topic(&self) -> String {
        format!("stream/{}/offer", self.stream_id)
    }

    pub fn events_topic(&self) -> String {
        format!("stream/{}/events", self.stream_id)
    }

    pub fn answer_topic(&self, viewer_id: &str) -> String {
        format!("stream/{}/answer/{viewer_id}", self.stream_id)
    }

    pub fn encrypt_json<T: Serialize>(&self, value: &T) -> Result<String, String> {
        let plaintext = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        let mut iv = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut iv);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&iv), plaintext.as_ref())
            .map_err(|_| "AES-GCM encryption failed".to_string())?;

        let envelope = EncryptedEnvelope {
            iv: URL_SAFE_NO_PAD.encode(iv),
            data: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        serde_json::to_string(&envelope).map_err(|e| e.to_string())
    }

    pub fn decrypt_bytes(&self, iv_b64: &str, data_b64: &str) -> Result<Vec<u8>, String> {
        let iv = URL_SAFE_NO_PAD
            .decode(iv_b64)
            .map_err(|_| "invalid iv encoding".to_string())?;
        if iv.len() != 12 {
            return Err("invalid iv length".to_string());
        }
        let data = URL_SAFE_NO_PAD
            .decode(data_b64)
            .map_err(|_| "invalid data encoding".to_string())?;
        self.cipher
            .decrypt(Nonce::from_slice(&iv), data.as_ref())
            .map_err(|_| "AES-GCM decryption failed".to_string())
    }
}

pub struct SeenIvs {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenIvs {
    pub fn new() -> Self {
        Self {
            set: HashSet::with_capacity(MAX_SEEN_IVS),
            order: VecDeque::with_capacity(MAX_SEEN_IVS),
        }
    }

    pub fn insert(&mut self, iv: String) -> bool {
        if self.set.contains(&iv) {
            return false;
        }
        if self.order.len() >= MAX_SEEN_IVS
            && let Some(oldest) = self.order.pop_front()
        {
            self.set.remove(&oldest);
        }
        self.set.insert(iv.clone());
        self.order.push_back(iv);
        true
    }
}
