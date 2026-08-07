use embuild::build::LinkArgs;
use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=components/biometrics_wrapper/biometrics_wrapper.cpp");
    println!("cargo:rerun-if-changed=components/biometrics_wrapper/include/bindings.h");

    // Propagate ESP-IDF build flags and linker scripts
    LinkArgs::output_propagated("ESP_IDF").expect("Failed to propagate link args");

    println!("cargo:rustc-link-arg=-lstdc++");
    
    // Generate Rust FFI bindings from clean C header
    let bindings = bindgen::Builder::default()
        .header("components/biometrics_wrapper/include/bindings.h")
        .clang_arg("-Icomponents/biometrics_wrapper/include")
        .derive_default(true)
        .use_core()
        .generate()
        .expect("Unable to generate FFI bindings for biometrics_wrapper");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}