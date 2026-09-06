fn main() {
    // Target, not host: correct even under a cross-build, where a host cfg!
    // would compile the macOS helper on the wrong platform.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/mac_perms.m")
            .flag("-fobjc-arc")
            .compile("unvelt_mac_perms");
        for fw in [
            "AVFoundation",
            "ApplicationServices",
            "Foundation",
            "CoreFoundation",
        ] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
        println!("cargo:rerun-if-changed=src/mac_perms.m");
    }
    tauri_build::build()
}
