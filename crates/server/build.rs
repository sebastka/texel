//! Texel's own cheat and patch codes (`patches/`, a file per game, in
//! DuckStation's format): each file names the serials it is for in a
//! `; Serials: SLES-02993, SLES-12993` line; they are built into the server
//! (see `codes.rs`), with no list to keep.

use std::fmt::Write;

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("patches");
    println!("cargo::rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|e| e == "cht"));
    files.sort();
    let mut out = String::from("&[\n");
    for path in files {
        println!("cargo::rerun-if-changed={}", path.display());
        let text = std::fs::read_to_string(&path).expect("a readable patch file");
        let serials: Vec<String> = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("; Serials:"))
            .unwrap_or_else(|| panic!("{}: no `; Serials:` line", path.display()))
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(!serials.is_empty(), "{}: no serials", path.display());
        writeln!(
            out,
            "    (&{serials:?}, include_str!({:?})),",
            path.display().to_string()
        )
        .expect("a string");
    }
    out.push(']');
    let target = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("own_patches.rs");
    std::fs::write(target, out).expect("the generated list");
}
