use embuild::build::LinkArgs;
use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=components/biometrics_wrapper/biometrics_wrapper.cpp");
    println!("cargo:rerun-if-changed=components/biometrics_wrapper/include/bindings.h");
    println!("cargo:rerun-if-changed=components/biometrics_wrapper/include/biometrics_wrapper.h");
    println!("cargo:rerun-if-changed=sdkconfig.defaults");

    // Propagate ESP-IDF build flags and linker configuration
    LinkArgs::output_propagated("ESP_IDF").expect("Failed to propagate link args");

    // Ensure C++ standard library is explicitly linked for biometrics_wrapper
    println!("cargo:rustc-link-arg=-lstdc++");

    let bindings = bindgen::Builder::default()
        .header("components/biometrics_wrapper/include/biometrics_wrapper.h")
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