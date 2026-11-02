//! AWS の REST-JSON API に共通の部品（署名付きの要求と、誤りの解釈）。

use crate::call::{Endpoint, HttpCall, HttpReply};
use crate::error::{PaasError, classify_aws_code, classify_status, sanitize};
use crate::secret::Secret;
use crate::sigv4::{AwsCredentials, sign_call};
use senca_http::Method;
use std::collections::BTreeMap;

/// 署名付きの要求を作る部品。
#[derive(Clone)]
pub(crate) struct AwsClient {
    pub creds: AwsCredentials,
    pub region: String,
    pub service: &'static str,
    pub endpoint: Endpoint,
}

impl std::fmt::Debug for AwsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsClient")
            .field("creds", &self.creds)
            .field("region", &self.region)
            .field("service", &self.service)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// region の形（`ap-northeast-1` など）。
fn valid_region(r: &str) -> bool {
    !r.is_empty() && r.len() <= 32 && r.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

impl AwsClient {
    /// 資格情報と options（`region` は必須、`endpoint` は任意）から作る。
    pub fn from_options(credential: &Secret, options: &BTreeMap<String, String>, service: &'static str) -> Result<Self, PaasError> {
        let creds = AwsCredentials::parse(credential)?;
        let region = options.get("region").ok_or_else(|| PaasError::config("AWS の連携には options.region が要る"))?;
        if !valid_region(region) {
            return Err(PaasError::config("region の形が不正"));
        }
        let endpoint = match options.get("endpoint") {
            Some(e) => Endpoint::parse(e)?,
            None => Endpoint::https(format!("{service}.{region}.amazonaws.com")),
        };
        Ok(AwsClient { creds, region: region.clone(), service, endpoint })
    }

    /// 署名した要求。`body` があれば JSON で送る。
    pub fn call(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&serde_json::Value>, now: u64) -> HttpCall {
        let mut c = HttpCall::new(method, self.endpoint.clone(), path);
        for (k, v) in query {
            c = c.query(k, v);
        }
        if let Some(b) = body {
            c = c.json(b);
        }
        c.set_header("Accept", "application/json");
        sign_call(&mut c, &self.creds, &self.region, self.service, now);
        c
    }
}

/// AWS の誤りの応答（`x-amzn-ErrorType` header、body の `__type`・`code`・`message`）を誤りにする。
pub(crate) fn aws_error(reply: &HttpReply) -> PaasError {
    let v = reply.json().unwrap_or(serde_json::Value::Null);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str());
    let raw = reply.header("x-amzn-errortype").or_else(|| s("__type")).or_else(|| s("code")).or_else(|| s("Code"));
    // `com.amazonaws.ivs#ConflictException` や `NotFoundException:http://…` の形を名前だけにする。
    let code = raw.map(|c| {
        let c = c.rsplit('#').next().unwrap_or(c);
        c.split(':').next().unwrap_or(c).to_string()
    });
    let msg = s("message").or_else(|| s("Message")).or_else(|| s("exceptionMessage")).or_else(|| s("errorMessage")).unwrap_or("");
    let kind = code.as_deref().and_then(classify_aws_code).unwrap_or_else(|| classify_status(reply.status));
    let code = code.filter(|c| !c.is_empty() && c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric()));
    PaasError { kind, status: Some(reply.status), code, message: sanitize(msg) }
}

/// 2xx なら JSON を返し、そうでなければ誤り。
pub(crate) fn aws_json(reply: &HttpReply) -> Result<serde_json::Value, PaasError> {
    if reply.is_success() { reply.json() } else { Err(aws_error(reply)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    #[test]
    fn error_shapes() {
        let r = HttpReply::new(404, r#"{"exceptionMessage":"channel not found"}"#)
            .with_header("x-amzn-ErrorType", "ResourceNotFoundException:http://internal.amazon.com/coral/com.amazonaws.ivs/");
        let e = aws_error(&r);
        assert_eq!((e.kind, e.code.as_deref(), e.message.as_str()), (ErrorKind::NotFound, Some("ResourceNotFoundException"), "channel not found"));
        let e = aws_error(&HttpReply::new(400, r#"{"__type":"com.amazonaws.ivs#ThrottlingException","message":"Rate exceeded"}"#));
        assert_eq!(e.kind, ErrorKind::Retryable);
        assert_eq!(e.code.as_deref(), Some("ThrottlingException"));
        let e = aws_error(&HttpReply::new(403, r#"{"message":"The security token included in the request is invalid."}"#));
        assert_eq!(e.kind, ErrorKind::Unauthorized);
        let e = aws_error(&HttpReply::new(502, "Bad Gateway"));
        assert!(e.is_retryable());
        let e = aws_error(&HttpReply::new(422, r#"{"__type":"UnprocessableEntityException","message":"bad"}"#));
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[test]
    fn options() {
        let mut o = BTreeMap::new();
        let cred = Secret::new("AKIA1:SECRET");
        assert!(AwsClient::from_options(&cred, &o, "ivs").is_err());
        o.insert("region".into(), "ap-northeast-1".into());
        let c = AwsClient::from_options(&cred, &o, "ivs").unwrap();
        assert_eq!(c.endpoint.host, "ivs.ap-northeast-1.amazonaws.com");
        assert!(!format!("{c:?}").contains("SECRET"));
        o.insert("region".into(), "ap/../x".into());
        assert!(AwsClient::from_options(&cred, &o, "ivs").is_err());
    }
}
