//! The web view: `web/dist`, embedded at compile time, with a fallback to `index.html`
//! for client-side routes, and a placeholder page when the view was not built.

use std::borrow::Cow;

use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

use crate::error::not_found;

#[derive(Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

const PLACEHOLDER: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>agent-board</title></head><body style=\"font-family: system-ui, sans-serif; \
max-width: 40rem; margin: 3rem auto; padding: 0 1rem\"><h1>agent-board</h1>\
<p>The server is running, but this build has no web view. Build <code>web/</code> \
(<code>bun run build</code>) and rebuild the server to embed it.</p>\
<p>The API is under <code>/v1</code>; <code>/healthz</code> reports status.</p></body></html>\n";

/// Where `web/build.ts` leaves the style nonce in `index.html`: in its CSP meta and in the
/// `csp-nonce` meta the app reads. Each page response replaces it with a fresh nonce.
const NONCE_PLACEHOLDER: &str = "__CSP_NONCE__";

/// The policy `web/build.ts` also puts in `index.html`, plus what only a header can say.
/// Styles may also come from `<style>` elements that carry the page's nonce: Radix adds
/// them for its scroll lock and Select. Scripts stay limited to this origin's files.
fn csp(style_nonce: Option<&str>) -> header::HeaderValue {
    let style = style_nonce.map_or_else(
        || "'self'".to_owned(),
        |nonce| format!("'self' 'nonce-{nonce}'"),
    );
    let policy = format!(
        "default-src 'self'; script-src 'self'; style-src {style}; \
img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'"
    );
    header::HeaderValue::try_from(policy).expect("the policy and a hex nonce are visible ASCII")
}

/// Security headers for every web view response: the CSP matters for HTML (whether served
/// as a client route or as `/index.html`); `nosniff` and the referrer policy for all.
fn security_headers(mut response: Response, style_nonce: Option<&str>) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, csp(style_nonce));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    response
}

/// A file of the built view.
struct File {
    data: Cow<'static, [u8]>,
    mime: String,
}

fn embedded(path: &str) -> Option<File> {
    let file = Assets::get(path)?;
    Some(File {
        mime: file.metadata.mimetype().to_owned(),
        data: file.data,
    })
}

/// A new random 128-bit nonce, hex-encoded (hex digits are valid in a CSP nonce).
fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    crate::auth::hex(&bytes)
}

fn with_cache(mime: String, cache: &'static str, body: impl IntoResponse) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, cache.to_owned()),
        ],
        body,
    )
        .into_response()
}

/// Serves the web view for every GET outside `/v1`.
pub(crate) async fn serve(method: Method, uri: Uri) -> Response {
    route(&method, uri.path(), embedded, &new_nonce())
}

/// The response for `path`, with files looked up by `files`. Pages (`index.html`, also as
/// the fallback for client routes) carry `nonce` in their HTML and in their CSP.
fn route(
    method: &Method,
    path: &str,
    files: impl Fn(&str) -> Option<File>,
    nonce: &str,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return not_found("no such endpoint", "the API is under /v1; see docs/API.md")
            .into_response();
    }
    let path = path.trim_start_matches('/');
    let page = match (!path.is_empty()).then(|| files(path)).flatten() {
        Some(file) if path == "index.html" => Some(file),
        Some(file) => {
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            let response = with_cache(file.mime, cache, file.data.into_owned());
            return security_headers(response, None);
        }
        // A missing file with an extension is a real 404, not a client route.
        None if path.rsplit('/').next().unwrap_or("").contains('.') => {
            let response = (StatusCode::NOT_FOUND, "not found").into_response();
            return security_headers(response, None);
        }
        None => files("index.html"),
    };
    match page {
        Some(file) => {
            let html = String::from_utf8_lossy(&file.data).replace(NONCE_PLACEHOLDER, nonce);
            security_headers(with_cache(file.mime, "no-cache", html), Some(nonce))
        }
        None => security_headers(Html(PLACEHOLDER).into_response(), None),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    const INDEX: &str = "<meta http-equiv=\"Content-Security-Policy\" content=\"style-src \
'self' 'nonce-__CSP_NONCE__'\" /><meta name=\"csp-nonce\" content=\"__CSP_NONCE__\" />";

    fn fake(path: &str) -> Option<File> {
        let (mime, data): (&str, &'static [u8]) = match path {
            "index.html" => ("text/html", INDEX.as_bytes()),
            "assets/app.js" => ("text/javascript", b"x"),
            "favicon.svg" => ("image/svg+xml", b"<svg/>"),
            _ => return None,
        };
        Some(File {
            mime: mime.to_owned(),
            data: Cow::Borrowed(data),
        })
    }

    fn csp_of(response: &Response) -> &str {
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
    }

    #[test]
    fn every_web_response_carries_security_headers() {
        for (path, status, mime) in [
            ("/", 200, Some("text/html")),
            ("/index.html", 200, Some("text/html")),
            ("/asks/3", 200, Some("text/html")),
            ("/assets/app.js", 200, Some("text/javascript")),
            ("/favicon.svg", 200, Some("image/svg+xml")),
            ("/missing.js", 404, None),
        ] {
            let response = route(&Method::GET, path, fake, "abc123");
            assert_eq!(response.status().as_u16(), status, "{path}");
            let headers = response.headers();
            if let Some(mime) = mime {
                assert_eq!(headers[header::CONTENT_TYPE], mime, "{path}");
            }
            let csp = csp_of(&response);
            assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
            assert!(csp.contains("script-src 'self';"), "{path}: {csp}");
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
            assert_eq!(headers[header::REFERRER_POLICY], "no-referrer", "{path}");
        }
        let post = route(&Method::POST, "/", fake, "abc123");
        assert_eq!(post.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pages_carry_the_nonce_in_their_html_and_policy() {
        for path in ["/", "/index.html", "/tasks/factory-12"] {
            let response = route(&Method::GET, path, fake, "abc123");
            assert!(
                csp_of(&response).contains("style-src 'self' 'nonce-abc123';"),
                "{path}"
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert!(!html.contains(NONCE_PLACEHOLDER), "{path}: {html}");
            assert_eq!(html.matches("abc123").count(), 2, "{path}: {html}");
        }
    }

    #[test]
    fn other_files_allow_no_inline_styles() {
        for path in ["/assets/app.js", "/favicon.svg", "/missing.js"] {
            let csp = route(&Method::GET, path, fake, "abc123");
            assert!(csp_of(&csp).contains("style-src 'self';"), "{path}");
        }
        let built = |_: &str| None;
        let placeholder = route(&Method::GET, "/", built, "abc123");
        assert!(csp_of(&placeholder).contains("style-src 'self';"));
    }

    #[test]
    fn nonces_are_fresh_and_hex() {
        let (a, b) = (new_nonce(), new_nonce());
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
