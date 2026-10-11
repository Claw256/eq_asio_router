// Export the COM entry points with undecorated names on every target
// (on i686 `extern "system"` symbols are otherwise `_Name@N`).
fn main() {
    println!("cargo:rerun-if-changed=eq_asio_driver.def");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-cdylib-link-arg=/DEF:{dir}/eq_asio_driver.def");
    }
}
