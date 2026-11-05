//! Embedded static frontend. The React build output lives in web/dist and
//! is compiled into the binary — no user-side Node dependency at runtime.

use include_dir::{include_dir, Dir};

pub static WEB_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

pub fn dist() -> &'static Dir<'static> {
    &WEB_DIST
}
