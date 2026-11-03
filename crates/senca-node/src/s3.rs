//! S3 互換のオブジェクトストレージのクライアント（PutObject・GetObject（Range）・HeadObject・DeleteObject・
//! ListObjectsV2・分割アップロード）。署名と XML は `senca-s3`、送受信は `httpc`（https は senca-tls）。
//! 宛先は管理者の設定なので、宛先の制限（egress）は通さない。資格情報はログ・エラーに出さない。

use bytes::Bytes;
use senca_http::Method;
use senca_s3::{Credentials, Endpoint, sha256_hex};
use std::time::Duration;

/// 1 回の要求の時間の上限（本文の大きさに応じて延ばす）。
fn timeout_for(len: usize) -> Duration {
    Duration::from_secs(30 + (len as u64 >> 20))
}

/// 応答の本文の上限（XML・小さな応答）。
const MAX_XML: usize = senca_s3::xml::MAX_INPUT;

pub struct S3Client {
    pub endpoint: Endpoint,
    creds: Credentials,
}

/// GetObject の結果。
pub struct Got {
    pub body: Bytes,
    /// 206 のときの（先頭, 末尾, 全体）。
    pub range: Option<(u64, u64, Option<u64>)>,
}

/// 要求の失敗。`status` は HTTP の状態（つながらなければ 0）。
#[derive(Debug)]
pub struct S3Error {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 { write!(f, "s3: {}", self.message) } else { write!(f, "s3: {} {}", self.status, self.message) }
    }
}

fn net(e: String) -> S3Error {
    S3Error { status: 0, message: e }
}

fn from_response(r: &crate::httpc::HttpResponse) -> S3Error {
    let message = match senca_s3::responses::parse_error(&r.body) {
        Some(e) => e.to_string(),
        None => String::from_utf8_lossy(&r.body[..r.body.len().min(200)]).into_owned(),
    };
    S3Error { status: r.status, message }
}

impl S3Client {
    pub fn new(endpoint: Endpoint, creds: Credentials) -> S3Client {
        S3Client { endpoint, creds }
    }

    #[allow(clippy::too_many_arguments)]
    async fn call(
        &self,
        method: &'static str,
        bucket: &str,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Option<Bytes>,
        max_body: usize,
    ) -> Result<crate::httpc::HttpResponse, S3Error> {
        let hash = body.as_ref().map(|b| sha256_hex(b)).unwrap_or_else(|| senca_s3::EMPTY_SHA256.to_string());
        let now = crate::util::now_ms() / 1000;
        let p = senca_s3::prepare(
            &self.endpoint,
            &self.creds,
            now,
            &senca_s3::Op { method, bucket, key, query, headers, payload_sha256: &hash },
        );
        let url = format!("{}://{}{}", self.endpoint.scheme(), p.host, p.target);
        let m = match method {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "PUT" => Method::Put,
            "POST" => Method::Post,
            "DELETE" => Method::Delete,
            _ => return Err(net("unsupported method".into())),
        };
        let hs: Vec<(&str, String)> = p.headers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let len = body.as_ref().map(|b| b.len()).unwrap_or(0).max(max_body.min(1 << 30));
        crate::httpc::request_trusted(m, &url, &hs, body, max_body, timeout_for(len)).await.map_err(net)
    }

    /// PutObject。
    pub async fn put_object(&self, bucket: &str, key: &str, body: Bytes, content_type: &str) -> Result<(), S3Error> {
        let r = self.call("PUT", bucket, key, &[], &[("content-type", content_type)], Some(body), MAX_XML).await?;
        if r.status != 200 {
            return Err(from_response(&r));
        }
        Ok(())
    }

    /// GetObject。無ければ None。`range` は `bytes=0-9` などをそのまま渡す。
    pub async fn get_object(&self, bucket: &str, key: &str, range: Option<&str>, max: usize) -> Result<Option<Got>, S3Error> {
        let hs: Vec<(&str, &str)> = range.map(|r| vec![("range", r)]).unwrap_or_default();
        let r = self.call("GET", bucket, key, &[], &hs, None, max).await?;
        match r.status {
            200 => Ok(Some(Got { body: r.body, range: None })),
            206 => {
                let cr = r.header("content-range").and_then(senca_s3::responses::parse_content_range);
                if cr.is_none() {
                    return Err(S3Error { status: 206, message: "Content-Range が読めません".into() });
                }
                Ok(Some(Got { body: r.body, range: cr }))
            }
            404 => Ok(None),
            _ => Err(from_response(&r)),
        }
    }

    /// HeadObject。あれば大きさ、無ければ None。
    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<Option<u64>, S3Error> {
        let r = self.call("HEAD", bucket, key, &[], &[], None, 0).await?;
        match r.status {
            200 => Ok(Some(r.header("content-length").and_then(|v| v.trim().parse().ok()).unwrap_or(0))),
            404 => Ok(None),
            _ => Err(S3Error { status: r.status, message: "HEAD failed".into() }),
        }
    }

    /// DeleteObject（無くても成功）。
    pub async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        let r = self.call("DELETE", bucket, key, &[], &[], None, MAX_XML).await?;
        if r.status != 204 && r.status != 200 && r.status != 404 {
            return Err(from_response(&r));
        }
        Ok(())
    }

    /// ListObjectsV2 の 1 ページ。
    pub async fn list_objects_v2(&self, bucket: &str, prefix: &str, token: Option<&str>) -> Result<senca_s3::ListObjectsV2, S3Error> {
        let mut q = vec![("list-type", "2"), ("prefix", prefix)];
        if let Some(t) = token {
            q.push(("continuation-token", t));
        }
        let r = self.call("GET", bucket, "", &q, &[], None, MAX_XML).await?;
        if r.status != 200 {
            return Err(from_response(&r));
        }
        senca_s3::responses::parse_list_objects_v2(&r.body).map_err(|e| S3Error { status: 200, message: e.to_string() })
    }

    /// 接頭辞の下のキーをすべて（`limit` 件まで）。
    pub async fn list_all(&self, bucket: &str, prefix: &str, limit: usize) -> Result<Vec<senca_s3::Object>, S3Error> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self.list_objects_v2(bucket, prefix, token.as_deref()).await?;
            out.extend(page.objects);
            if out.len() >= limit || !page.is_truncated {
                out.truncate(limit);
                return Ok(out);
            }
            token = page.next_continuation_token;
        }
    }

    /// 大きければ分割アップロード、そうでなければ PutObject。
    pub async fn put_auto(&self, bucket: &str, key: &str, body: Bytes, content_type: &str, part_size: u64) -> Result<(), S3Error> {
        if (body.len() as u64) <= part_size {
            return self.put_object(bucket, key, body, content_type).await;
        }
        self.put_multipart(bucket, key, body, content_type, part_size).await
    }

    /// 分割アップロード（CreateMultipartUpload → UploadPart → CompleteMultipartUpload）。失敗したら中止する。
    pub async fn put_multipart(&self, bucket: &str, key: &str, body: Bytes, content_type: &str, part_size: u64) -> Result<(), S3Error> {
        let r = self.call("POST", bucket, key, &[("uploads", "")], &[("content-type", content_type)], None, MAX_XML).await?;
        if r.status != 200 {
            return Err(from_response(&r));
        }
        let upload_id = senca_s3::responses::parse_initiate_multipart(&r.body).map_err(|e| S3Error { status: 200, message: e.to_string() })?;
        let res = self.upload_parts(bucket, key, &upload_id, &body, part_size).await;
        if res.is_err() {
            let _ = self.call("DELETE", bucket, key, &[("uploadId", &upload_id)], &[], None, MAX_XML).await;
        }
        res
    }

    async fn upload_parts(&self, bucket: &str, key: &str, upload_id: &str, body: &Bytes, part_size: u64) -> Result<(), S3Error> {
        let mut etags = Vec::new();
        for (n, off, len) in senca_s3::responses::plan_parts(body.len() as u64, part_size) {
            let part = body.slice(off as usize..(off + len) as usize);
            let num = n.to_string();
            let r = self.call("PUT", bucket, key, &[("partNumber", &num), ("uploadId", upload_id)], &[], Some(part), MAX_XML).await?;
            if r.status != 200 {
                return Err(from_response(&r));
            }
            let etag = r.header("etag").ok_or_else(|| S3Error { status: 200, message: "UploadPart に ETag がありません".into() })?;
            etags.push((n, etag.to_string()));
        }
        let xml = Bytes::from(senca_s3::responses::complete_multipart_body(&etags));
        let r = self.call("POST", bucket, key, &[("uploadId", upload_id)], &[("content-type", "application/xml")], Some(xml), MAX_XML).await?;
        if r.status != 200 {
            return Err(from_response(&r));
        }
        senca_s3::responses::parse_complete_multipart(&r.body).map_err(|e| S3Error { status: 200, message: e.to_string() })?;
        Ok(())
    }
}

/// 資格情報を読む: ファイル（JSON）、無ければ環境変数（AWS_ACCESS_KEY_ID・AWS_SECRET_ACCESS_KEY・AWS_SESSION_TOKEN）。
pub fn load_credentials(file: Option<&std::path::Path>) -> Result<Credentials, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct F {
        access_key_id: String,
        secret_access_key: String,
        #[serde(default)]
        session_token: Option<String>,
    }
    let (id, secret, token) = match file {
        Some(p) => {
            let b = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
            // 中身はエラーに出さない
            let f: F = serde_json::from_slice(&b).map_err(|_| format!("{}: JSON の形が違います（access_key_id・secret_access_key・session_token）", p.display()))?;
            (f.access_key_id, f.secret_access_key, f.session_token)
        }
        None => {
            let id = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| "S3 の資格情報がありません（--s3-credentials か AWS_ACCESS_KEY_ID・AWS_SECRET_ACCESS_KEY）")?;
            let secret = std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| "AWS_SECRET_ACCESS_KEY がありません")?;
            (id, secret, std::env::var("AWS_SESSION_TOKEN").ok())
        }
    };
    let bad = |s: &str| s.is_empty() || s.len() > 4096 || s.chars().any(|c| c.is_control() || c == ' ');
    if bad(&id) || bad(&secret) || token.as_deref().is_some_and(|t| !t.is_empty() && bad(t)) {
        return Err("S3 の資格情報の形が違います".into());
    }
    Ok(Credentials { access_key_id: id, secret_access_key: secret, session_token: token.filter(|t| !t.is_empty()) })
}
