// Every profile in the repository's profiles/ folder is built into the receiver.
use std::io::Write;

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = std::fs::read_dir(&dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|x| x == "json"));
    files.sort();
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("builtin_profiles.rs");
    let mut f = std::fs::File::create(out).unwrap();
    writeln!(f, "pub const BUILTIN: &[(&str, &str)] = &[").unwrap();
    for p in files {
        println!("cargo:rerun-if-changed={}", p.display());
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        writeln!(f, "    ({name:?}, include_str!({:?})),", p.canonicalize().unwrap().display().to_string()).unwrap();
    }
    writeln!(f, "];").unwrap();
}
