//! The web view: `web/dist`, embedded at compile time, with a fallback to `index.html`
//! for client-side routes, and a placeholder page when the view was not built.

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

/// The policy `web/build.ts` also puts in `index.html`, plus what only a header can say.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

/// Security headers for every web view response: the CSP matters for HTML (whether served
/// as a client route or as `/index.html`); `nosniff` and the referrer policy for all.
fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(CSP),
    );
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

fn asset(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
                (header::CACHE_CONTROL, cache.to_owned()),
            ],
            file.data.into_owned(),
        )
            .into_response(),
    )
}

/// Serves the web view for every GET outside `/v1`.
pub(crate) async fn serve(method: Method, uri: Uri) -> Response {
    route(&method, uri.path(), asset)
}

/// The response for `path`, with files looked up by `asset`.
fn route(method: &Method, path: &str, asset: impl Fn(&str) -> Option<Response>) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return not_found("no such endpoint", "the API is under /v1; see docs/API.md")
            .into_response();
    }
    let path = path.trim_start_matches('/');
    let response = if let Some(response) = (!path.is_empty()).then(|| asset(path)).flatten() {
        response
    } else if path.rsplit('/').next().unwrap_or("").contains('.') {
        // A missing file with an extension is a real 404, not a client route.
        (StatusCode::NOT_FOUND, "not found").into_response()
    } else {
        asset("index.html").unwrap_or_else(|| Html(PLACEHOLDER).into_response())
    };
    security_headers(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(path: &str) -> Option<Response> {
        let mime = match path {
            "index.html" => "text/html",
            "assets/app.js" => "text/javascript",
            _ => return None,
        };
        Some(([(header::CONTENT_TYPE, mime)], "x").into_response())
    }

    #[test]
    fn every_web_response_carries_security_headers() {
        for (path, status, mime) in [
            ("/", 200, Some("text/html")),
            ("/index.html", 200, Some("text/html")),
            ("/asks/3", 200, Some("text/html")),
            ("/assets/app.js", 200, Some("text/javascript")),
            ("/missing.js", 404, None),
        ] {
            let response = route(&Method::GET, path, fake);
            assert_eq!(response.status().as_u16(), status, "{path}");
            let headers = response.headers();
            if let Some(mime) = mime {
                assert_eq!(headers[header::CONTENT_TYPE], mime, "{path}");
            }
            let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
            assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
            assert_eq!(headers[header::REFERRER_POLICY], "no-referrer", "{path}");
        }
        let post = route(&Method::POST, "/", fake);
        assert_eq!(post.status(), StatusCode::NOT_FOUND);
    }
}
