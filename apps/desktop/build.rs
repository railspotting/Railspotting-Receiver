fn main() {
    #[cfg(target_os = "macos")]
    {
        tauri_build::build();
        link_clang_runtime();
    }
}

/// whisper.cpp's Metal code checks the macOS version with `@available`, which calls
/// `__isPlatformVersionAtLeast` from clang's runtime. Built for the app's minimum macOS those
/// checks stay in, and rustc's link step doesn't add clang's runtime the way clang would.
#[cfg(target_os = "macos")]
fn link_clang_runtime() {
    let Ok(out) = std::process::Command::new("xcrun").args(["clang", "--print-resource-dir"]).output() else {
        println!("cargo:warning=clang not found; the link may miss __isPlatformVersionAtLeast");
        return;
    };
    let dir = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("lib/darwin");
    if dir.join("libclang_rt.osx.a").exists() {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    } else {
        println!("cargo:warning=libclang_rt.osx.a not found in {}", dir.display());
    }
}
