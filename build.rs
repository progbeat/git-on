fn main() {
    println!("cargo:rerun-if-changed=src/git/diagnostic_writer.c");
    cc::Build::new()
        .file("src/git/diagnostic_writer.c")
        .flag_if_supported("-fexceptions")
        .compile("git_on_diagnostic_writer");
}
