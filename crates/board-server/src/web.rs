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

fn html_headers(mut response: Response) -> Response {
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
    if method != Method::GET && method != Method::HEAD {
        return not_found("no such endpoint", "the API is under /v1; see docs/API.md")
            .into_response();
    }
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty()
        && let Some(response) = asset(path)
    {
        return response;
    }
    // A missing file with an extension is a real 404, not a client route.
    let last = path.rsplit('/').next().unwrap_or("");
    if last.contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    html_headers(asset("index.html").unwrap_or_else(|| Html(PLACEHOLDER).into_response()))
}
