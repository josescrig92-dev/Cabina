fn main() {
    println!("cargo:rerun-if-changed=vendor/stretch_wrapper.cpp");
    cc::Build::new()
        .cpp(true)
        .std("c++14")
        .file("vendor/stretch_wrapper.cpp")
        .include("vendor")
        .opt_level(3)
        .warnings(false)
        .compile("cabina_stretch");
    tauri_build::build()
}
