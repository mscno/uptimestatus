// `include_dir!` cannot tell Cargo which files it read; rebuild when any
// embedded static file (or the vendored Datastar bundle) changes.
fn main() {
    println!("cargo:rerun-if-changed=static");
    println!("cargo:rerun-if-changed=vendor/datastar/datastar-rocket.js");
}
