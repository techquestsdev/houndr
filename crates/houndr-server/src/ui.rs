use axum::http::header;
use axum::response::Html;
use std::sync::LazyLock;

const INDEX_HTML_TEMPLATE: &str = include_str!("../templates/index.html");
const APP_CSS: &str = include_str!("../static/app.css");
const APP_JS: &str = include_str!("../static/app.js");
const FAVICON_SVG: &str = include_str!("../static/favicon.svg");

/// FNV-1a hash → 8 hex chars. Zero dependencies.
fn short_hash(data: &[u8]) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

static INDEX_HTML: LazyLock<String> = LazyLock::new(|| {
    let css_hash = short_hash(APP_CSS.as_bytes());
    let js_hash = short_hash(APP_JS.as_bytes());
    INDEX_HTML_TEMPLATE
        .replace("/static/app.css", &format!("/static/app.css?v={css_hash}"))
        .replace("/static/app.js", &format!("/static/app.js?v={js_hash}"))
});

/// Serve the single-page web UI.
pub async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML.as_str())
}

pub async fn css_handler() -> ([(header::HeaderName, &'static str); 2], &'static str) {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        APP_CSS,
    )
}

pub async fn js_handler() -> ([(header::HeaderName, &'static str); 2], &'static str) {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        APP_JS,
    )
}

pub async fn favicon_handler() -> ([(header::HeaderName, &'static str); 2], &'static str) {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        FAVICON_SVG,
    )
}
