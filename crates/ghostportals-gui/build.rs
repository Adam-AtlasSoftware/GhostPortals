//! Gives the Windows executable its icon and file details. Only on MSVC
//! builds (what releases use): cross builds lack a resource compiler.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows && msvc {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon.ico")
            .set("FileDescription", "Ghost Portals")
            .set("ProductName", "Ghost Portals")
            .set("LegalCopyright", "GPL-3.0-or-later");
        res.compile().expect("compiling the Windows resources");
    }
}
