fn main() {
    // The exe's version comes from Cargo.toml, as "major,minor,patch,0" for the resource compiler.
    let version = format!("VERSION={},0", env!("CARGO_PKG_VERSION").replace('.', ","));
    embed_resource::compile("beacon.rc", [version]).manifest_optional().unwrap();
}
