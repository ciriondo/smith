//! The browser UI consists of three files embedded in the binary. There is no build step.
//!
//! The JavaScript uses no framework. `include_str!` is sufficient for three files. If the
//! asset count grows, this can move to a directory crate.

use axum::http::header;
use axum::response::{IntoResponse, Response};

pub async fn index() -> Response {
    page(include_str!("../ui/index.html"), "text/html; charset=utf-8")
}

pub async fn javascript() -> Response {
    page(
        include_str!("../ui/app.js"),
        "text/javascript; charset=utf-8",
    )
}

pub async fn stylesheet() -> Response {
    page(include_str!("../ui/style.css"), "text/css; charset=utf-8")
}

fn page(body: &'static str, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}
