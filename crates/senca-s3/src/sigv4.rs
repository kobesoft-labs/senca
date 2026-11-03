//! AWS Signature Version 4（ヘッダーに署名を載せる形）。
//!
//! 手順は AWS の文書「Create a signed AWS API request」のとおり:
//! 1. 正規の要求（canonical request）を作る
//! 2. 署名する文字列（string to sign）を作る
//! 3. 秘密鍵から日付・地域・サービスの順に HMAC-SHA256 で署名鍵を導く
//! 4. 署名して `Authorization` を組み立てる
//!
//! HMAC・SHA-256 は RustCrypto（ADR-0002）。ここでは文字列の組み立てだけを書く。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// 空の本文の SHA-256。
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// 本文に署名しない（`x-amz-content-sha256` の値）。
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// アクセスキー。Debug では秘密を出さない。
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// 一時的な資格情報（STS）のトークン。
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// AWS の UriEncode。RFC 3986 の非予約文字（`A-Z a-z 0-9 - . _ ~`）以外を `%XX`（大文字）にする。
/// `encode_slash` が偽なら `/` を残す（オブジェクトのキーのパス）。
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 正規の問い合わせ文字列。値は生のまま渡す（ここで符号化して、キー・値の順に並べる）。
pub fn canonical_query(q: &[(&str, &str)]) -> String {
    let mut v: Vec<(String, String)> = q.iter().map(|(k, x)| (uri_encode(k, true), uri_encode(x, true))).collect();
    v.sort();
    v.iter().map(|(k, x)| format!("{k}={x}")).collect::<Vec<_>>().join("&")
}

/// 値の前後の空白を除き、続く空白を 1 つにする。
fn trim_all(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut space = false;
    for c in v.trim_matches([' ', '\t']).chars() {
        if c == ' ' || c == '\t' {
            space = true;
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// 正規のヘッダーの並び（各行が `\n` で終わる）と、署名するヘッダーの名前（`;` 区切り）。
/// 名前は小文字にして並べる。同じ名前は値を `,` でつなぐ（出てきた順）。
pub fn canonical_headers(h: &[(&str, &str)]) -> (String, String) {
    let mut v: Vec<(String, String)> = Vec::new();
    for (k, x) in h {
        let k = k.to_ascii_lowercase();
        let x = trim_all(x);
        match v.iter_mut().find(|(a, _)| *a == k) {
            Some((_, cur)) => {
                cur.push(',');
                cur.push_str(&x);
            }
            None => v.push((k, x)),
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    let block = v.iter().map(|(k, x)| format!("{k}:{x}\n")).collect::<String>();
    let signed = v.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    (block, signed)
}

/// 正規の要求。`canonical_uri` は符号化済みのパス（S3 は 2 度符号化しない・正規化しない）。
pub fn canonical_request(method: &str, canonical_uri: &str, canonical_query: &str, headers: &[(&str, &str)], payload_hash: &str) -> (String, String) {
    let (block, signed) = canonical_headers(headers);
    (format!("{method}\n{canonical_uri}\n{canonical_query}\n{block}\n{signed}\n{payload_hash}"), signed)
}

/// 資格情報の範囲（`20150830/us-east-1/service/aws4_request`）。
pub fn scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/aws4_request")
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()))
}

/// 署名鍵（kSigning）。
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

pub fn signature(key: &[u8; 32], string_to_sign: &str) -> String {
    hex(&hmac(key, string_to_sign.as_bytes()))
}

/// 署名の材料。
#[derive(Debug, Clone, Copy)]
pub struct SignInput<'a> {
    pub method: &'a str,
    pub canonical_uri: &'a str,
    /// 生の問い合わせ（キー・値）。
    pub query: &'a [(&'a str, &'a str)],
    /// 署名するヘッダー（`host`・`x-amz-date` を含める）。
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_hash: &'a str,
    /// `YYYYMMDDTHHMMSSZ`。
    pub amz_date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

/// 署名の結果（試験と診断のために途中の値も返す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signed_headers: String,
    pub signature: String,
    pub authorization: String,
}

pub fn sign(creds: &Credentials, i: &SignInput<'_>) -> Signed {
    let cq = canonical_query(i.query);
    let (cr, signed_headers) = canonical_request(i.method, i.canonical_uri, &cq, i.headers, i.payload_hash);
    let date = i.amz_date.get(..8).unwrap_or(i.amz_date);
    let sc = scope(date, i.region, i.service);
    let sts = string_to_sign(i.amz_date, &sc, &cr);
    let sig = signature(&signing_key(&creds.secret_access_key, date, i.region, i.service), &sts);
    let authorization = format!("{ALGORITHM} Credential={}/{sc}, SignedHeaders={signed_headers}, Signature={sig}", creds.access_key_id);
    Signed { canonical_request: cr, string_to_sign: sts, signed_headers, signature: sig, authorization }
}

/// UNIX 時刻（秒）を `YYYYMMDDTHHMMSSZ` にする。
pub fn amz_date(unix_secs: i64) -> String {
    senca_core::rfc3339::format_millis(unix_secs.saturating_mul(1000)).chars().filter(|c| *c != '-' && *c != ':').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS の SigV4 の試験一式（aws-sig-v4-test-suite）の資格情報と時刻。
    fn suite_creds() -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    fn suite(method: &str, query: &[(&str, &str)], extra: &[(&str, &str)]) -> Signed {
        let mut h = vec![("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")];
        h.extend_from_slice(extra);
        sign(
            &suite_creds(),
            &SignInput {
                method,
                canonical_uri: "/",
                query,
                headers: &h,
                payload_hash: EMPTY_SHA256,
                amz_date: "20150830T123600Z",
                region: "us-east-1",
                service: "service",
            },
        )
    }

    #[test]
    fn suite_get_vanilla() {
        let s = suite("GET", &[], &[]);
        assert_eq!(
            s.canonical_request,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            s.string_to_sign,
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n\
             bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(
            s.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn suite_query_order_and_post() {
        // get-vanilla-query-order-key-case: GET /?Param2=value2&Param1=value1
        let s = suite("GET", &[("Param2", "value2"), ("Param1", "value1")], &[]);
        assert!(s.canonical_request.contains("\nParam1=value1&Param2=value2\n"));
        assert_eq!(s.signature, "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500");
        // post-vanilla
        let s = suite("POST", &[], &[]);
        assert_eq!(s.signature, "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b");
    }

    #[test]
    fn signing_key_known() {
        // AWS の文書「Deriving the signing key」の例（IAM、2012-02-15）。
        let k = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20120215", "us-east-1", "iam");
        assert_eq!(hex(&k), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    // Amazon S3 の文書「Signature Calculations for the Authorization Header」の例。
    fn s3_creds() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    fn s3(method: &str, uri: &str, query: &[(&str, &str)], headers: &[(&str, &str)], payload: &str) -> Signed {
        sign(
            &s3_creds(),
            &SignInput {
                method,
                canonical_uri: uri,
                query,
                headers,
                payload_hash: payload,
                amz_date: "20130524T000000Z",
                region: "us-east-1",
                service: "s3",
            },
        )
    }

    #[test]
    fn s3_get_object_with_range() {
        let s = s3(
            "GET",
            "/test.txt",
            &[],
            &[
                ("Host", "examplebucket.s3.amazonaws.com"),
                ("Range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY_SHA256,
        );
        assert_eq!(s.signed_headers, "host;range;x-amz-content-sha256;x-amz-date");
        assert_eq!(s.signature, "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    }

    #[test]
    fn s3_put_object() {
        let body = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(body, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let s = s3(
            "PUT",
            &format!("/{}", uri_encode("test$file.text", false)),
            &[],
            &[
                ("Host", "examplebucket.s3.amazonaws.com"),
                ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-date", "20130524T000000Z"),
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ("x-amz-content-sha256", &body),
            ],
            &body,
        );
        assert!(s.canonical_request.starts_with("PUT\n/test%24file.text\n\n"));
        assert_eq!(s.signature, "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
    }

    #[test]
    fn s3_get_lifecycle_and_list() {
        let h = [("Host", "examplebucket.s3.amazonaws.com"), ("x-amz-date", "20130524T000000Z"), ("x-amz-content-sha256", EMPTY_SHA256)];
        let s = s3("GET", "/", &[("lifecycle", "")], &h, EMPTY_SHA256);
        assert!(s.canonical_request.contains("\nlifecycle=\n"));
        assert_eq!(s.signature, "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543");
        let s = s3("GET", "/", &[("max-keys", "2"), ("prefix", "J")], &h, EMPTY_SHA256);
        assert_eq!(s.signature, "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }

    #[test]
    fn header_and_uri_rules() {
        let (b, s) = canonical_headers(&[("My-Header1", "  a   b  "), ("my-header1", "c"), ("Host", "x")]);
        assert_eq!(b, "host:x\nmy-header1:a b,c\n");
        assert_eq!(s, "host;my-header1");
        assert_eq!(uri_encode("a b/c~d+e=", false), "a%20b/c~d%2Be%3D");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("é", true), "%C3%A9");
        assert_eq!(canonical_query(&[("b", "2"), ("a", "x y"), ("a", "1")]), "a=1&a=x%20y&b=2");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        let d = format!("{:?}", suite_creds());
        assert!(!d.contains("wJalr"));
    }
}
