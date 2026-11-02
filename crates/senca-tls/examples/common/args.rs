//! 例の引数の解釈（相互接続試験用の最小限）。

#![allow(dead_code)]

use senca_tls::{CipherSuite, NamedGroup};

pub fn parse_suites(s: &str) -> Vec<CipherSuite> {
    s.split(',')
        .map(|x| match x {
            "aes128" | "TLS_AES_128_GCM_SHA256" => CipherSuite::Aes128GcmSha256,
            "aes256" | "TLS_AES_256_GCM_SHA384" => CipherSuite::Aes256GcmSha384,
            "chacha" | "TLS_CHACHA20_POLY1305_SHA256" => CipherSuite::Chacha20Poly1305Sha256,
            o => panic!("未知のスイート {o}"),
        })
        .collect()
}

pub fn parse_groups(s: &str) -> Vec<NamedGroup> {
    s.split(',')
        .map(|x| match x {
            "x25519" | "X25519" => NamedGroup::X25519,
            "p256" | "P-256" | "secp256r1" => NamedGroup::Secp256r1,
            o => panic!("未知の group {o}"),
        })
        .collect()
}

pub fn parse_alpn(s: &str) -> Vec<Vec<u8>> {
    s.split(',').map(|x| x.as_bytes().to_vec()).collect()
}

/// `--name value` 形式の引数。
pub struct Args(pub Vec<String>);

impl Args {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).map(|s| s.as_str())
    }
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.0
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == name)
            .filter_map(|(i, _)| self.0.get(i + 1).map(|s| s.as_str()))
            .collect()
    }
    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}
