//! Compiled EE blocks are added to this module's function table as they are
//! made (src/jit.rs): it must be able to grow.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-cdylib=--growable-table");
    }
}
