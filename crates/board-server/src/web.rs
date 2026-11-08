//! The web view: `web/dist`, embedded at compile time, with a fallback to `index.html`
//! for client-side routes, and a placeholder page when the view was not built.

use std::borrow::Cow;

use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

use crate::error::not_found;

#[derive(Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

const PLACEHOLDER: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>agent-board</title><style nonce=\"{{CSP_NONCE}}\">body {font-family: system-ui, \
sans-serif; max-width: 40rem; margin: 3rem auto; padding: 0 1rem}</style></head><body>\
<h1>agent-board</h1>\
<p>The server is running, but this build has no web view. Build <code>web/</code> \
(<code>bun run build</code>) and rebuild the server to embed it.</p>\
<p>The API is under <code>/v1</code>; <code>/healthz</code> reports status.</p></body></html>\n";

/// Where `web/build.ts` leaves the style nonce in `index.html`: in its CSP meta and on the
/// `csp-nonce` meta the app reads. Each page response replaces it with a fresh nonce. It is
/// not a valid nonce itself, so a page served without the replacement allows no `<style>`.
const NONCE_PLACEHOLDER: &str = "{{CSP_NONCE}}";

/// The policy `web/build.ts` also puts in `index.html`, plus what only a header can say.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

/// The policy for a page: `<style>` elements that carry its nonce are allowed too, since
/// Radix adds them for its scroll lock and Select. Scripts stay limited to this origin.
fn page_csp(nonce: &str) -> header::HeaderValue {
    let policy = CSP.replace(
        "style-src 'self';",
        &format!("style-src 'self' 'nonce-{nonce}';"),
    );
    header::HeaderValue::try_from(policy).expect("the policy and a hex nonce are visible ASCII")
}

/// Security headers for every web view response: the CSP matters for HTML (whether served
/// as a client route or as `/index.html`); `nosniff` and the referrer policy for all.
fn security_headers(mut response: Response, csp: header::HeaderValue) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, csp);
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
    crate::auth::random_hex::<16>()
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
    route(&method, uri.path(), embedded, new_nonce)
}

/// The response for `path`, with files looked up by `files`. Pages (`index.html`, also as
/// the fallback for client routes) carry a nonce from `nonce` in their HTML and their CSP,
/// and are not stored by caches, so a nonce serves one response.
fn route(
    method: &Method,
    path: &str,
    files: impl Fn(&str) -> Option<File>,
    nonce: impl FnOnce() -> String,
) -> Response {
    let csp = header::HeaderValue::from_static(CSP);
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
            return security_headers(with_cache(file.mime, cache, file.data), csp);
        }
        // A missing file with an extension is a real 404, not a client route.
        None if path.rsplit('/').next().unwrap_or("").contains('.') => {
            let response = (StatusCode::NOT_FOUND, "not found").into_response();
            return security_headers(response, csp);
        }
        None => files("index.html"),
    };
    // The placeholder, like index.html, styles itself with a `<style>` that carries the nonce.
    let (mime, html) = match &page {
        Some(file) => (file.mime.clone(), String::from_utf8_lossy(&file.data)),
        None => (
            "text/html; charset=utf-8".to_owned(),
            Cow::Borrowed(PLACEHOLDER),
        ),
    };
    let nonce = nonce();
    let html = html.replace(NONCE_PLACEHOLDER, &nonce);
    security_headers(with_cache(mime, "no-store", html), page_csp(&nonce))
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    const INDEX: &str = "<meta http-equiv=\"Content-Security-Policy\" content=\"style-src \
'self' 'nonce-{{CSP_NONCE}}'\" /><meta property=\"csp-nonce\" nonce=\"{{CSP_NONCE}}\" />";

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

    /// One real nonce for every response in a test run, so tests can look for it.
    static NONCE: std::sync::LazyLock<String> = std::sync::LazyLock::new(new_nonce);

    fn nonce() -> String {
        NONCE.clone()
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
            let response = route(&Method::GET, path, fake, nonce);
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
        let post = route(&Method::POST, "/", fake, nonce);
        assert_eq!(post.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pages_carry_the_nonce_in_their_html_and_policy() {
        for path in ["/", "/index.html", "/tasks/factory-12"] {
            let response = route(&Method::GET, path, fake, nonce);
            assert!(
                csp_of(&response).contains(&format!("style-src 'self' 'nonce-{}';", *NONCE)),
                "{path}"
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert!(!html.contains(NONCE_PLACEHOLDER), "{path}: {html}");
            assert_eq!(html.matches(NONCE.as_str()).count(), 2, "{path}: {html}");
        }
    }

    #[test]
    fn other_files_allow_no_inline_styles() {
        let unused = || -> String { panic!("only pages need a nonce") };
        for path in ["/assets/app.js", "/favicon.svg", "/missing.js"] {
            let response = route(&Method::GET, path, fake, unused);
            assert!(csp_of(&response).contains("style-src 'self';"), "{path}");
        }
    }

    #[tokio::test]
    async fn the_placeholder_styles_itself_with_the_nonce_only() {
        let unbuilt = route(&Method::GET, "/tasks/factory-12", |_| None, nonce);
        assert_eq!(unbuilt.status(), StatusCode::OK);
        assert!(csp_of(&unbuilt).contains(&format!("style-src 'self' 'nonce-{}';", *NONCE)));
        assert_eq!(unbuilt.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(unbuilt.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("style=\""), "{html}");
        assert!(
            html.contains(&format!("<style nonce=\"{}\">", *NONCE)),
            "{html}"
        );
        assert!(!html.contains(NONCE_PLACEHOLDER), "{html}");
    }

    #[test]
    fn the_build_writes_the_placeholder_this_server_replaces() {
        let build = include_str!("../../../web/build.ts");
        assert!(
            build.contains(&format!("NONCE_PLACEHOLDER = \"{NONCE_PLACEHOLDER}\"")),
            "web/build.ts and web.rs must use the same nonce placeholder"
        );
    }

    #[test]
    fn nonces_are_fresh_and_hex() {
        let (a, b) = (new_nonce(), new_nonce());
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
