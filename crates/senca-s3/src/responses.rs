//! S3 の応答の本文（XML）と、応答ヘッダーの値の解釈。

use crate::xml::{self, Element, XmlError};

/// ListObjectsV2 の 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListObjectsV2 {
    pub objects: Vec<Object>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// S3 の誤りの応答（`<Error>`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
    pub request_id: Option<String>,
}

impl std::fmt::Display for ErrorResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(r) = &self.request_id {
            write!(f, " (request {r})")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseError {
    Xml(XmlError),
    /// 期待した形と違う（根の名前・必須の項目）。
    Unexpected(&'static str),
    /// 200 で返ってきた `<Error>`（CompleteMultipartUpload は 200 の後で失敗を返すことがある）。
    S3(ErrorResponse),
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseError::Xml(e) => e.fmt(f),
            ResponseError::Unexpected(w) => write!(f, "s3: unexpected response ({w})"),
            ResponseError::S3(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ResponseError {}

impl From<XmlError> for ResponseError {
    fn from(e: XmlError) -> Self {
        ResponseError::Xml(e)
    }
}

fn root(body: &[u8], want: &'static str) -> Result<Element, ResponseError> {
    let r = xml::parse(body)?;
    if r.name == "Error" {
        return Err(ResponseError::S3(error_from(&r)));
    }
    if r.name != want {
        return Err(ResponseError::Unexpected(want));
    }
    Ok(r)
}

fn error_from(r: &Element) -> ErrorResponse {
    ErrorResponse {
        code: r.text_of("Code").unwrap_or("").to_string(),
        message: r.text_of("Message").unwrap_or("").to_string(),
        request_id: r.text_of("RequestId").map(str::to_string),
    }
}

/// 誤りの応答を読む（`<Error>` でなければ None）。
pub fn parse_error(body: &[u8]) -> Option<ErrorResponse> {
    let r = xml::parse(body).ok()?;
    (r.name == "Error").then(|| error_from(&r))
}

/// ListObjectsV2（`ListBucketResult`）。
pub fn parse_list_objects_v2(body: &[u8]) -> Result<ListObjectsV2, ResponseError> {
    let r = root(body, "ListBucketResult")?;
    let mut out = ListObjectsV2 {
        is_truncated: r.text_of("IsTruncated").is_some_and(|v| v.trim() == "true"),
        next_continuation_token: r.text_of("NextContinuationToken").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
        ..Default::default()
    };
    for c in r.children_named("Contents") {
        let key = c.text_of("Key").ok_or(ResponseError::Unexpected("Contents/Key"))?.to_string();
        let size = c.text_of("Size").ok_or(ResponseError::Unexpected("Contents/Size"))?.trim().parse().map_err(|_| ResponseError::Unexpected("Contents/Size"))?;
        out.objects.push(Object {
            key,
            size,
            etag: c.text_of("ETag").map(str::to_string),
            last_modified: c.text_of("LastModified").map(str::to_string),
        });
    }
    for p in r.children_named("CommonPrefixes") {
        if let Some(v) = p.text_of("Prefix") {
            out.common_prefixes.push(v.to_string());
        }
    }
    if out.is_truncated && out.next_continuation_token.is_none() {
        return Err(ResponseError::Unexpected("NextContinuationToken"));
    }
    Ok(out)
}

/// CreateMultipartUpload（`InitiateMultipartUploadResult`）の UploadId。
pub fn parse_initiate_multipart(body: &[u8]) -> Result<String, ResponseError> {
    let r = root(body, "InitiateMultipartUploadResult")?;
    let id = r.text_of("UploadId").map(str::trim).filter(|v| !v.is_empty()).ok_or(ResponseError::Unexpected("UploadId"))?;
    Ok(id.to_string())
}

/// CompleteMultipartUpload（`CompleteMultipartUploadResult`）の ETag。200 でも `<Error>` なら失敗。
pub fn parse_complete_multipart(body: &[u8]) -> Result<String, ResponseError> {
    let r = root(body, "CompleteMultipartUploadResult")?;
    Ok(r.text_of("ETag").unwrap_or("").to_string())
}

/// CompleteMultipartUpload の本文。`parts` は（番号, ETag）で、番号の昇順。
pub fn complete_multipart_body(parts: &[(u32, String)]) -> String {
    let mut s = String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for (n, etag) in parts {
        s.push_str(&format!("<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag></Part>", xml::escape(etag)));
    }
    s.push_str("</CompleteMultipartUpload>");
    s
}

/// `Content-Range: bytes 0-9/100` →（先頭, 末尾, 全体）。全体が `*` なら None。
pub fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let r = v.trim().strip_prefix("bytes ")?;
    let (range, total) = r.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    if b < a {
        return None;
    }
    let total = if total == "*" { None } else { Some(total.parse::<u64>().ok()?) };
    if total.is_some_and(|t| b >= t) {
        return None;
    }
    Some((a, b, total))
}

/// 分割アップロードの最小の部分（最後を除く）と、部分の数の上限（S3 の制約）。
pub const MIN_PART: u64 = 5 << 20;
pub const MAX_PART: u64 = 5 << 30;
pub const MAX_PARTS: u64 = 10_000;

/// `len` バイトを `part` バイトずつに分けた（番号, 先頭, 長さ）。部分が多すぎるときは大きくする。
pub fn plan_parts(len: u64, part: u64) -> Vec<(u32, u64, u64)> {
    let mut part = part.clamp(MIN_PART, MAX_PART);
    if len.div_ceil(part) > MAX_PARTS {
        part = len.div_ceil(MAX_PARTS);
    }
    let mut v = Vec::new();
    let mut off = 0;
    let mut n = 1u32;
    while off < len {
        let l = part.min(len - off);
        v.push((n, off, l));
        off += l;
        n += 1;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    // Amazon S3 API リファレンスの ListObjectsV2 の応答の例（形をそろえたもの）。
    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name>
  <Prefix/>
  <KeyCount>2</KeyCount>
  <MaxKeys>2</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>
  <Contents>
    <Key>happyface.jpg</Key>
    <LastModified>2014-11-21T19:40:05.000Z</LastModified>
    <ETag>&quot;70ee1738b6b21e2c8a43f3a5ab0eee71&quot;</ETag>
    <Size>11</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>a &amp; b/c.ts</Key>
    <Size>1048576</Size>
  </Contents>
  <CommonPrefixes><Prefix>photos/</Prefix></CommonPrefixes>
</ListBucketResult>"#;

    #[test]
    fn list() {
        let l = parse_list_objects_v2(LIST.as_bytes()).unwrap();
        assert!(l.is_truncated);
        assert_eq!(l.next_continuation_token.as_deref(), Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM="));
        assert_eq!(l.objects.len(), 2);
        assert_eq!(l.objects[0].etag.as_deref(), Some("\"70ee1738b6b21e2c8a43f3a5ab0eee71\""));
        assert_eq!(l.objects[1].key, "a & b/c.ts");
        assert_eq!(l.objects[1].size, 1_048_576);
        assert_eq!(l.common_prefixes, vec!["photos/"]);
        // 打ち切りなのに続きの印が無いのは拒否
        let bad = LIST.replace("<NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>", "");
        assert!(parse_list_objects_v2(bad.as_bytes()).is_err());
        let bad = LIST.replace("<Size>11</Size>", "<Size>-1</Size>");
        assert!(parse_list_objects_v2(bad.as_bytes()).is_err());
    }

    #[test]
    fn errors_and_multipart() {
        let e = br#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>NoSuchKey</Code><Message>The resource you requested does not exist</Message><Resource>/mybucket/myfoto.jpg</Resource><RequestId>4442587FB7D0A2F9</RequestId></Error>"#;
        let p = parse_error(e).unwrap();
        assert_eq!(p.code, "NoSuchKey");
        assert_eq!(p.request_id.as_deref(), Some("4442587FB7D0A2F9"));
        assert!(matches!(parse_list_objects_v2(e), Err(ResponseError::S3(_))));
        assert!(parse_error(LIST.as_bytes()).is_none());
        let i = br#"<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>b</Bucket><Key>k</Key><UploadId>VXBsb2FkIElEIGZvciA2aWWpbmcncyBteS1tb3ZpZS5tMnRzIHVwbG9hZA</UploadId></InitiateMultipartUploadResult>"#;
        assert_eq!(parse_initiate_multipart(i).unwrap(), "VXBsb2FkIElEIGZvciA2aWWpbmcncyBteS1tb3ZpZS5tMnRzIHVwbG9hZA");
        let c = br#"<CompleteMultipartUploadResult><Location>x</Location><Bucket>b</Bucket><Key>k</Key><ETag>"3858f62230ac3c915f300c664312c11f-9"</ETag></CompleteMultipartUploadResult>"#;
        assert_eq!(parse_complete_multipart(c).unwrap(), "\"3858f62230ac3c915f300c664312c11f-9\"");
        let ce = br#"<Error><Code>InternalError</Code><Message>We encountered an internal error. Please try again.</Message></Error>"#;
        assert!(matches!(parse_complete_multipart(ce), Err(ResponseError::S3(e)) if e.code == "InternalError"));
        let body = complete_multipart_body(&[(1, "\"a\"".into()), (2, "\"b\"".into())]);
        assert!(body.contains("<Part><PartNumber>1</PartNumber><ETag>&quot;a&quot;</ETag></Part><Part><PartNumber>2</PartNumber>"));
        // 自分で作った本文を自分の読み手で読める
        assert_eq!(xml::parse(body.as_bytes()).unwrap().children_named("Part").count(), 2);
    }

    #[test]
    fn ranges_and_parts() {
        assert_eq!(parse_content_range("bytes 0-9/100"), Some((0, 9, Some(100))));
        assert_eq!(parse_content_range("bytes 5-5/*"), Some((5, 5, None)));
        assert_eq!(parse_content_range("bytes 9-0/100"), None);
        assert_eq!(parse_content_range("bytes 0-100/100"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        let p = plan_parts(12 << 20, 5 << 20);
        assert_eq!(p, vec![(1, 0, 5 << 20), (2, 5 << 20, 5 << 20), (3, 10 << 20, 2 << 20)]);
        // 小さすぎる部分は最小まで広げる
        assert_eq!(plan_parts(6 << 20, 1).len(), 2);
        // 部分の数は 10000 まで
        let p = plan_parts(MIN_PART * 20_000, MIN_PART);
        assert!(p.len() as u64 <= MAX_PARTS);
        assert_eq!(p.iter().map(|x| x.2).sum::<u64>(), MIN_PART * 20_000);
        assert!(plan_parts(0, MIN_PART).is_empty());
    }
}
