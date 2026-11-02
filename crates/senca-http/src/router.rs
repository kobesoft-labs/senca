//! 簡単な path router。`/api/v1/streams/{id}` の形の pattern を segment 単位で照合する。

use crate::error::HttpError;
use crate::pct::decode_path;
use crate::request::Method;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Literal(String),
    Param(String),
}

/// path の pattern。`{name}` は 1 segment に一致する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    segs: Vec<Seg>,
}

impl Pattern {
    pub fn parse(p: &str) -> Result<Pattern, HttpError> {
        let rest = p.strip_prefix('/').ok_or(HttpError::InvalidInput("pattern must start with /"))?;
        let mut segs = Vec::new();
        if !rest.is_empty() {
            for s in rest.split('/') {
                if s.is_empty() {
                    return Err(HttpError::InvalidInput("empty pattern segment"));
                }
                if let Some(name) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                    if name.is_empty() || segs.iter().any(|x| matches!(x, Seg::Param(n) if n == name)) {
                        return Err(HttpError::InvalidInput("pattern parameter"));
                    }
                    segs.push(Seg::Param(name.to_owned()));
                } else if s.contains(['{', '}']) {
                    return Err(HttpError::InvalidInput("pattern segment"));
                } else {
                    segs.push(Seg::Literal(s.to_owned()));
                }
            }
        }
        Ok(Pattern { segs })
    }

    /// 復号済みの segment 列と照合する。
    pub fn matches(&self, segments: &[String]) -> Option<Params> {
        if segments.len() != self.segs.len() {
            return None;
        }
        let mut params = Vec::new();
        for (p, s) in self.segs.iter().zip(segments) {
            match p {
                Seg::Literal(l) if l == s => {}
                Seg::Literal(_) => return None,
                Seg::Param(n) => params.push((n.clone(), s.clone())),
            }
        }
        Some(Params(params))
    }
}

/// 取り出した path parameter（復号済み）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(pub Vec<(String, String)>);

impl Params {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// 照合の結果。
#[derive(Debug, PartialEq, Eq)]
pub enum RouteMatch<'a, T> {
    Found { handler: &'a T, params: Params },
    /// path は一致するがメソッドが違う（405 と Allow に使う）。
    MethodNotAllowed { allow: Vec<Method> },
    NotFound,
    /// path の復号に失敗した（400）。
    BadPath(HttpError),
}

/// メソッドと pattern の組の一覧。先に登録したものが優先。
#[derive(Debug, Clone)]
pub struct Router<T> {
    routes: Vec<(Method, Pattern, T)>,
}

impl<T> Default for Router<T> {
    fn default() -> Self {
        Router { routes: Vec::new() }
    }
}

impl<T> Router<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, method: Method, pattern: &str, handler: T) -> Result<(), HttpError> {
        self.routes.push((method, Pattern::parse(pattern)?, handler));
        Ok(())
    }

    /// HEAD は HEAD の登録がなければ GET に一致させる。
    pub fn route(&self, method: &Method, path: &str) -> RouteMatch<'_, T> {
        let segs = match decode_path(path) {
            Ok(s) => s,
            Err(e) => return RouteMatch::BadPath(e),
        };
        let mut allow = Vec::new();
        let mut get_fallback = None;
        for (m, p, h) in &self.routes {
            if let Some(params) = p.matches(&segs) {
                if m == method {
                    return RouteMatch::Found { handler: h, params };
                }
                if *method == Method::Head && *m == Method::Get && get_fallback.is_none() {
                    get_fallback = Some((h, params));
                }
                if !allow.contains(m) {
                    allow.push(m.clone());
                }
            }
        }
        if let Some((handler, params)) = get_fallback {
            return RouteMatch::Found { handler, params };
        }
        if allow.is_empty() { RouteMatch::NotFound } else { RouteMatch::MethodNotAllowed { allow } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing() {
        let mut r = Router::new();
        r.add(Method::Get, "/api/v1/streams", 1).unwrap();
        r.add(Method::Get, "/api/v1/streams/{id}", 2).unwrap();
        r.add(Method::Patch, "/api/v1/streams/{id}", 3).unwrap();
        r.add(Method::Post, "/api/v1/streams/{id}/inputs", 4).unwrap();
        match r.route(&Method::Get, "/api/v1/streams/ab%20c") {
            RouteMatch::Found { handler, params } => {
                assert_eq!(*handler, 2);
                assert_eq!(params.get("id"), Some("ab c"));
            }
            m => panic!("{m:?}"),
        }
        assert!(matches!(r.route(&Method::Head, "/api/v1/streams"), RouteMatch::Found { handler: 1, .. }));
        assert_eq!(r.route(&Method::Delete, "/api/v1/streams/x"), RouteMatch::MethodNotAllowed { allow: vec![Method::Get, Method::Patch] });
        assert_eq!(r.route(&Method::Get, "/api/v1/nope"), RouteMatch::NotFound);
        assert!(matches!(r.route(&Method::Get, "/api/v1/streams/a%2Fb"), RouteMatch::BadPath(_)));
        assert!(Pattern::parse("/a/{x}/{x}").is_err());
        assert!(Pattern::parse("/a/b{x}").is_err());
    }
}
