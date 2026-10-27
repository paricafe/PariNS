//! Embedded console files.
//!
//! The build precompresses `app.js` and `app.css` (Brotli and gzip) and gives
//! every resource address in `index.html` a content version (`?v=` and the
//! first 16 hex digits of the file's SHA-256). Only a request that names the
//! version of the embedded file is cached for a year; any other request, such
//! as a document from another build, and the document itself stay `no-store`
//! (set for every other response in [`super::handle`]). HEAD gets the same
//! headers without a body. API, WebSocket and DoH responses are never compressed.

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, header, request::Parts},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub(super) struct Asset {
    kind: &'static str,
    plain: &'static [u8],
    /// Brotli and gzip versions of `plain`.
    encoded: Option<(&'static [u8], &'static [u8])>,
    version: OnceLock<String>,
}

pub(super) fn find(path: &str) -> Option<&'static Asset> {
    const JS: &str = "text/javascript; charset=utf-8";
    static DOCUMENT: Asset = Asset {
        kind: "text/html; charset=utf-8",
        plain: include_bytes!("../../web/dist/index.html"),
        encoded: None,
        version: OnceLock::new(),
    };
    static CSS: Asset = Asset {
        kind: "text/css; charset=utf-8",
        plain: include_bytes!("../../web/dist/assets/app.css"),
        encoded: Some((
            include_bytes!("../../web/dist/assets/app.css.br"),
            include_bytes!("../../web/dist/assets/app.css.gz"),
        )),
        version: OnceLock::new(),
    };
    static APP: Asset = Asset {
        kind: JS,
        plain: include_bytes!("../../web/dist/assets/app.js"),
        encoded: Some((
            include_bytes!("../../web/dist/assets/app.js.br"),
            include_bytes!("../../web/dist/assets/app.js.gz"),
        )),
        version: OnceLock::new(),
    };
    static THEME: Asset = Asset {
        kind: JS,
        plain: include_bytes!("../../web/dist/theme-init.js"),
        encoded: None,
        version: OnceLock::new(),
    };
    static ICON: Asset = Asset {
        kind: "image/svg+xml",
        plain: include_bytes!("../../web/dist/favicon.svg"),
        encoded: None,
        version: OnceLock::new(),
    };
    match path {
        "/" => Some(&DOCUMENT),
        "/favicon.svg" => Some(&ICON),
        "/assets/app.css" => Some(&CSS),
        "/assets/app.js" => Some(&APP),
        "/theme-init.js" => Some(&THEME),
        _ => None,
    }
}

impl Asset {
    /// The content version that `index.html` gives this file.
    fn version(&self) -> &str {
        self.version.get_or_init(|| {
            Sha256::digest(self.plain)[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        })
    }

    /// A GET or HEAD `request` for this file.
    pub(super) fn response(&self, request: &Parts) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(self.kind));
        if request
            .uri
            .query()
            .and_then(|query| query.strip_prefix("v="))
            == Some(self.version())
        {
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
        }
        let mut body = self.plain;
        if let Some((brotli, gzip)) = self.encoded {
            headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
            let accepted = |coding| quality(&request.headers, coding) > 0;
            let selected = if accepted("br") {
                Some(("br", brotli))
            } else if accepted("gzip") {
                Some(("gzip", gzip))
            } else {
                None
            };
            if let Some((coding, encoded)) = selected {
                headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
                body = encoded;
            }
        }
        headers.insert(header::CONTENT_LENGTH, body.len().into());
        if request.method == Method::HEAD {
            return (headers, Body::empty()).into_response();
        }
        (headers, body).into_response()
    }
}

/// The weight in thousandths that the request's `Accept-Encoding` (RFC 9110
/// section 12.5.3) gives `coding`. An explicit entry overrides `*`; an absent
/// header or coding gets 0, so the original file is sent.
fn quality(request: &HeaderMap, coding: &str) -> u16 {
    let mut wildcard = None;
    for entry in request
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
    {
        let mut parts = entry.split(';').map(str::trim);
        let name = parts.next().unwrap_or_default();
        let weight = parts
            .find_map(|parameter| {
                let (key, value) = parameter.split_once('=')?;
                key.trim()
                    .eq_ignore_ascii_case("q")
                    .then(|| weight(value.trim()))
            })
            .unwrap_or(Some(1000));
        if name.eq_ignore_ascii_case(coding) {
            return weight.unwrap_or(0);
        }
        if name == "*" {
            wildcard = weight;
        }
    }
    wildcard.unwrap_or(0)
}

/// Parses a qvalue (`0`, `0.5`, `1.000`) into thousandths; anything else is invalid.
fn weight(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let thousandths = format!("{fraction:0<3}").parse::<u16>().ok()?;
    match whole {
        "0" => Some(thousandths),
        "1" if thousandths == 0 => Some(1000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, uri: &str, values: &[&str]) -> Parts {
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        for value in values {
            request = request.header(header::ACCEPT_ENCODING, *value);
        }
        request.body(()).unwrap().into_parts().0
    }

    /// The address `index.html` gives `path`, with its content version.
    fn versioned(path: &str) -> String {
        let document = std::str::from_utf8(find("/").unwrap().plain).unwrap();
        let start = document.find(&format!("\"{path}?v=")).unwrap() + 1;
        let end = start + document[start..].find('"').unwrap();
        document[start..end].to_owned()
    }

    fn served(values: &[&str]) -> (Option<String>, axum::body::Body) {
        let response = find("/assets/app.js").unwrap().response(&request(
            "GET",
            &versioned("/assets/app.js"),
            values,
        ));
        assert_eq!(response.headers()[header::VARY], "accept-encoding");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let coding = response
            .headers()
            .get(header::CONTENT_ENCODING)
            .map(|value| value.to_str().unwrap().to_owned());
        (coding, response.into_body())
    }

    async fn bytes(body: axum::body::Body) -> Vec<u8> {
        axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn accept_encoding_selects_brotli_then_gzip_then_the_original() {
        let app = find("/assets/app.js").unwrap();
        let (brotli, gzip) = app.encoded.unwrap();
        for (values, coding, content) in [
            (&["gzip, deflate, br, zstd"][..], Some("br"), brotli),
            (&["br"], Some("br"), brotli),
            (&["GZIP"], Some("gzip"), gzip),
            (&["gzip", "br;q=0.5"], Some("br"), brotli),
            (&["br;q=0, gzip"], Some("gzip"), gzip),
            (&["br; Q=0.000, gzip;q=0.001"], Some("gzip"), gzip),
            (&["*"], Some("br"), brotli),
            (&["br;q=0, *"], Some("gzip"), gzip),
            (&["*;q=0, gzip"], Some("gzip"), gzip),
            (&["*;q=0"], None, app.plain),
            (&["br;q=0, gzip;q=0"], None, app.plain),
            (&["br;q=2, gzip;q=x"], None, app.plain),
            (&["identity"], None, app.plain),
            (&[], None, app.plain),
        ] {
            let (selected, body) = served(values);
            assert_eq!(selected.as_deref(), coding, "{values:?}");
            assert_eq!(bytes(body).await, content, "{values:?}");
        }
    }

    #[tokio::test]
    async fn only_the_current_version_is_cacheable_and_only_large_files_vary() {
        let all = ["br, gzip"];
        let document = find("/").unwrap().response(&request("GET", "/", &all));
        assert!(!document.headers().contains_key(header::CACHE_CONTROL));
        assert!(!document.headers().contains_key(header::CONTENT_ENCODING));
        for path in ["/theme-init.js", "/favicon.svg", "/assets/app.js"] {
            let asset = find(path).unwrap();
            let current = asset.response(&request("GET", &versioned(path), &all));
            assert_eq!(
                current.headers()[header::CACHE_CONTROL],
                "public, max-age=31536000, immutable"
            );
            // Another build's version, or none, must not pin this content.
            for other in [path.to_owned(), format!("{path}?v=0000000000000000")] {
                let response = asset.response(&request("GET", &other, &all));
                assert!(!response.headers().contains_key(header::CACHE_CONTROL));
            }
            let head = asset.response(&request("HEAD", &versioned(path), &all));
            assert_eq!(head.headers(), current.headers());
            assert!(bytes(head.into_body()).await.is_empty());
            let length: usize = current.headers()[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(bytes(current.into_body()).await.len(), length);
        }
        for path in ["/theme-init.js", "/favicon.svg"] {
            let small = find(path).unwrap().response(&request("GET", path, &all));
            assert!(!small.headers().contains_key(header::VARY));
            assert!(!small.headers().contains_key(header::CONTENT_ENCODING));
        }
        assert!(find("/index.html").is_none());
        assert!(find("/assets/app.js.br").is_none());
    }
}
