/// Build script for generating N-API bindings metadata.
///
/// This is required by napi-rs to generate the correct linker exports
/// for the native module. It must be present for the `.node` binary
/// to load correctly in Node.js.
fn main() {
    napi_build::setup();
}
