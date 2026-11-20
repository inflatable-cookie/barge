fn main() {
    // Plain `build()` (no `codegen` feature) emits no `rerun-if-changed` for
    // the bundle icons, so an icon edit would otherwise leave a stale icon
    // embedded. `include_bytes!` already rebuilds the crate itself; this
    // rebuilds what tauri-build embeds.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}
