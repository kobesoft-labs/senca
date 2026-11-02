//! `Authorization: Bearer <token>` の取り出し（RFC 6750 2.1）。token は表示しない。

use crate::error::HttpError;
use crate::headers::HeaderMap;

fn is_token68_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
}

/// Bearer token を返す。header がなければ `Ok(None)`。
/// 2 行以上・Bearer 以外の scheme・token68 でない値は `Err`（401 で返すこと）。
pub fn bearer_token(headers: &HeaderMap) -> Result<Option<&str>, HttpError> {
    let Some(v) = headers.get_unique("authorization")? else { return Ok(None) };
    let v = std::str::from_utf8(v).map_err(|_| HttpError::Malformed("authorization"))?;
    let (scheme, rest) = v.split_once(' ').ok_or(HttpError::Malformed("authorization"))?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(HttpError::Malformed("authorization scheme"));
    }
    let token = rest.trim_start_matches(' ');
    let body = token.trim_end_matches('=');
    if body.is_empty() || token.len() > 4096 || !body.bytes().all(is_token68_char) {
        return Err(HttpError::Malformed("bearer token"));
    }
    Ok(Some(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.append("Authorization", v.to_owned()).unwrap();
        h
    }

    #[test]
    fn bearer() {
        assert_eq!(bearer_token(&h("Bearer abc.DEF-_~+/=")).unwrap(), Some("abc.DEF-_~+/="));
        assert_eq!(bearer_token(&h("bearer   x")).unwrap(), Some("x"));
        assert!(bearer_token(&h("Basic Zm9vOmJhcg==")).is_err());
        assert!(bearer_token(&h("Bearer")).is_err());
        assert!(bearer_token(&h("Bearer a b")).is_err());
        assert!(bearer_token(&h("Bearer =")).is_err());
        assert_eq!(bearer_token(&HeaderMap::new()).unwrap(), None);
        let mut two = h("Bearer a");
        two.append("authorization", "Bearer b").unwrap();
        assert!(bearer_token(&two).is_err());
    }
}
