use std::env;
use std::sync::Once;

static PUBLIC_PATH: Once = Once::new();

// The router serves the static files of `crates/mobius/public`.
// The variable is global to the process, so all tests share one value.
pub fn set() {
    PUBLIC_PATH.call_once(|| {
        // SAFETY: every test calls `set` before it reads the variable,
        // and `Once` runs the write one time.
        unsafe {
            env::set_var(
                "DIOXUS_PUBLIC_PATH",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../mobius/public"),
            )
        };
    });
}
