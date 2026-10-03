//! Compiles the C++ rasterizer core.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    vbuild::compile_cpp(
        "v3d_core",
        &["cpp/transform.cpp", "cpp/setup.cpp", "cpp/raster.cpp", "cpp/post.cpp"],
        &["cpp"],
    );
}
