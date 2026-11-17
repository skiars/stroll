fn main() {
    println!("cargo:rerun-if-changed=locales");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=src/terminal/conpty.c");
        let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
        cc::Build::new()
            .file("src/terminal/conpty.c")
            .flag_if_supported(if msvc { "/std:c11" } else { "-std=c11" })
            .compile("stroll_conpty");
        println!("cargo:rustc-link-lib=kernel32");
    }
}
