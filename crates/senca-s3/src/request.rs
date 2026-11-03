//! 要求の組み立て: 宛先（パス形式・仮想ホスト形式）、保存先の場所（`s3://bucket/prefix`）、署名付きのヘッダー。

use crate::sigv4::{self, Credentials, SignInput};

/// S3 互換のサービスの宛先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// パス形式（`host/bucket/key`）。偽なら仮想ホスト形式（`bucket.host/key`）。
    pub path_style: bool,
    pub region: String,
}

impl Endpoint {
    /// AWS の地域の宛先（仮想ホスト形式）。
    pub fn aws(region: &str) -> Endpoint {
        Endpoint { tls: true, host: format!("s3.{region}.amazonaws.com"), port: 443, path_style: false, region: region.to_string() }
    }

    /// `http(s)://host[:port]` を読む（パス・問い合わせ・利用者情報は受け付けない）。
    pub fn parse(url: &str, region: &str, path_style: bool) -> Result<Endpoint, String> {
        let (scheme, rest) = url.split_once("://").ok_or("endpoint は http(s)://host[:port]")?;
        let tls = match scheme {
            "https" => true,
            "http" => false,
            _ => return Err("endpoint は http か https".into()),
        };
        let auth = rest.strip_suffix('/').unwrap_or(rest);
        if auth.is_empty() || auth.contains(['/', '?', '#', '@', '[', ']']) {
            return Err("endpoint は http(s)://host[:port]（パス・IPv6 の表記は使えません）".into());
        }
        let (host, port) = match auth.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "endpoint の番号が違います")?),
            None => (auth, if tls { 443 } else { 80 }),
        };
        if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return Err("endpoint のホスト名が違います".into());
        }
        if !valid_region(region) {
            return Err("region の形が違います".into());
        }
        Ok(Endpoint { tls, host: host.to_ascii_lowercase(), port, path_style, region: region.to_string() })
    }

    fn default_port(&self) -> bool {
        (self.tls && self.port == 443) || (!self.tls && self.port == 80)
    }

    /// 接続先のホスト名（仮想ホスト形式ならバケット名が付く）。
    pub fn connect_host(&self, bucket: &str) -> String {
        if self.path_style { self.host.clone() } else { format!("{bucket}.{}", self.host) }
    }

    /// `Host` ヘッダーの値（既定の番号は省く）。
    pub fn host_header(&self, bucket: &str) -> String {
        let h = self.connect_host(bucket);
        if self.default_port() { h } else { format!("{h}:{}", self.port) }
    }

    /// 符号化済みのパス（正規の URI と同じ）。`key` が空ならバケット自体。
    pub fn path(&self, bucket: &str, key: &str) -> String {
        let k = sigv4::uri_encode(key, false);
        match (self.path_style, key.is_empty()) {
            (true, true) => format!("/{bucket}"),
            (true, false) => format!("/{bucket}/{k}"),
            (false, _) => format!("/{k}"),
        }
    }

    pub fn scheme(&self) -> &'static str {
        if self.tls { "https" } else { "http" }
    }
}

fn valid_region(r: &str) -> bool {
    !r.is_empty() && r.len() <= 32 && r.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// バケット名の規則（S3 の汎用バケット）: 3〜63 文字、小文字・数字・`.`・`-`、先頭と末尾は英数字。
pub fn valid_bucket(b: &str) -> bool {
    (3..=63).contains(&b.len())
        && b.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && b.starts_with(|c: char| c.is_ascii_alphanumeric())
        && b.ends_with(|c: char| c.is_ascii_alphanumeric())
        && !b.contains("..")
}

/// 保存先の場所: バケットとキーの接頭辞。`s3://bucket` または `s3://bucket/a/b`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub bucket: String,
    /// 末尾の `/` を含まない（空なら接頭辞なし）。
    pub prefix: String,
}

impl Location {
    pub fn parse(uri: &str) -> Result<Location, String> {
        let rest = uri.strip_prefix("s3://").ok_or("保存先は s3://bucket[/prefix]")?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if !valid_bucket(bucket) {
            return Err(format!("バケット名が違います: {bucket}"));
        }
        let prefix = prefix.trim_end_matches('/');
        if !prefix.is_empty() && !valid_rel(prefix) {
            return Err("接頭辞に空・`.`・`..` の段や使えない文字があります".into());
        }
        Ok(Location { bucket: bucket.to_string(), prefix: prefix.to_string() })
    }

    pub fn uri(&self) -> String {
        if self.prefix.is_empty() { format!("s3://{}", self.bucket) } else { format!("s3://{}/{}", self.bucket, self.prefix) }
    }

    /// データ置き場からの相対パスに対応するキー。
    pub fn key(&self, rel: &str) -> String {
        if self.prefix.is_empty() { rel.to_string() } else { format!("{}/{rel}", self.prefix) }
    }
}

/// キーにしてよい相対パス（空・`.`・`..` の段なし、制御文字なし、1024 バイトまで）。
pub fn valid_rel(p: &str) -> bool {
    p.len() <= 1024
        && !p.starts_with('/')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
        && !p.chars().any(|c| c.is_control() || c == '\\')
}

/// 署名済みの要求。`target` は `path?query`。`headers` に Host は含めない（接続側が `host_header` と同じ値を入れる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub method: &'static str,
    pub host: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

/// 要求の中身。
#[derive(Debug, Clone, Copy)]
pub struct Op<'a> {
    pub method: &'static str,
    pub bucket: &'a str,
    pub key: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    /// 署名に含める追加のヘッダー（`range`・`content-type` など）。
    pub headers: &'a [(&'a str, &'a str)],
    /// 本文の SHA-256（16 進）。
    pub payload_sha256: &'a str,
}

/// 署名して組み立てる。`now_unix_secs` は呼び出し側の時計。
pub fn prepare(ep: &Endpoint, creds: &Credentials, now_unix_secs: i64, op: &Op<'_>) -> Prepared {
    let amz_date = sigv4::amz_date(now_unix_secs);
    let host = ep.host_header(op.bucket);
    let path = ep.path(op.bucket, op.key);
    let mut h: Vec<(&str, &str)> = vec![("host", &host), ("x-amz-content-sha256", op.payload_sha256), ("x-amz-date", &amz_date)];
    if let Some(t) = &creds.session_token {
        h.push(("x-amz-security-token", t));
    }
    h.extend_from_slice(op.headers);
    let s = sigv4::sign(
        creds,
        &SignInput {
            method: op.method,
            canonical_uri: &path,
            query: op.query,
            headers: &h,
            payload_hash: op.payload_sha256,
            amz_date: &amz_date,
            region: &ep.region,
            service: "s3",
        },
    );
    let q = sigv4::canonical_query(op.query);
    let target = if q.is_empty() { path.clone() } else { format!("{path}?{q}") };
    let mut headers: Vec<(String, String)> = h.iter().filter(|(k, _)| *k != "host").map(|(k, v)| (k.to_string(), v.to_string())).collect();
    headers.push(("authorization".into(), s.authorization));
    Prepared { method: op.method, host, target, headers }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        let e = Endpoint::parse("http://127.0.0.1:9000", "us-east-1", true).unwrap();
        assert_eq!(e.host_header("b"), "127.0.0.1:9000");
        assert_eq!(e.path("b", "x/y z.ts"), "/b/x/y%20z.ts");
        assert_eq!(e.path("b", ""), "/b");
        let a = Endpoint::aws("ap-northeast-1");
        assert_eq!(a.host_header("my-bkt"), "my-bkt.s3.ap-northeast-1.amazonaws.com");
        assert_eq!(a.path("my-bkt", "k"), "/k");
        assert!(Endpoint::parse("ftp://x", "r", true).is_err());
        assert!(Endpoint::parse("http://u:p@x", "r", true).is_err());
        assert!(Endpoint::parse("http://x/path", "r", true).is_err());
        assert!(Endpoint::parse("http://x:99999", "r", true).is_err());
        assert!(Endpoint::parse("https://x.r2.cloudflarestorage.com/", "auto", true).is_ok());
        assert!(Endpoint::parse("https://x", "US EAST", true).is_err());
    }

    #[test]
    fn locations() {
        let l = Location::parse("s3://senca-rec/a/b/").unwrap();
        assert_eq!((l.bucket.as_str(), l.prefix.as_str()), ("senca-rec", "a/b"));
        assert_eq!(l.key("segments/s/r/0-1.ts"), "a/b/segments/s/r/0-1.ts");
        assert_eq!(l.uri(), "s3://senca-rec/a/b");
        assert_eq!(Location::parse("s3://bkt").unwrap().key("x"), "x");
        for bad in ["s3://", "s3://AB", "s3://a", "s3://-ab", "s3://ab..c", "s3://abc/../x", "s3://abc//x", "http://abc", "s3://abc/a\nb"] {
            assert!(Location::parse(bad).is_err(), "{bad}");
        }
        assert!(!valid_rel("a/./b") && !valid_rel("/a") && valid_rel("recordings/x/index.jsonl"));
    }

    #[test]
    fn prepared_request() {
        // S3 の文書の GET の例を、組み立ての側から作っても同じ署名になる
        let ep = Endpoint { host: "s3.amazonaws.com".into(), ..Endpoint::aws("us-east-1") };
        let creds = Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        // 2013-05-24T00:00:00Z
        let p = prepare(
            &ep,
            &creds,
            1_369_353_600,
            &Op {
                method: "GET",
                bucket: "examplebucket",
                key: "test.txt",
                query: &[],
                headers: &[("range", "bytes=0-9")],
                payload_sha256: sigv4::EMPTY_SHA256,
            },
        );
        assert_eq!(p.host, "examplebucket.s3.amazonaws.com");
        assert_eq!(p.target, "/test.txt");
        let auth = &p.headers.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), "{auth}");
        assert!(!p.headers.iter().any(|(k, _)| k == "host"));
        // 一時的な資格情報はトークンも署名する
        let c2 = Credentials { session_token: Some("tok".into()), ..creds };
        let p = prepare(
            &ep,
            &c2,
            0,
            &Op { method: "GET", bucket: "b-1", key: "", query: &[("list-type", "2"), ("prefix", "a/")], headers: &[], payload_sha256: sigv4::EMPTY_SHA256 },
        );
        assert_eq!(p.target, "/?list-type=2&prefix=a%2F");
        let auth = &p.headers.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token,"));
    }
}
