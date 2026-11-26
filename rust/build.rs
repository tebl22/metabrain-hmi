// Embeds the MetaBrain icon and file details in the Windows executables.
fn main() {
    println!("cargo:rerun-if-changed=../build/metabrain.ico");
    println!("cargo:rerun-if-changed=../build/metabrain-tray.png");
    println!("cargo:rerun-if-changed=../metabrain/web");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../build/metabrain.ico");
        res.set("ProductName", "MetaBrain");
        res.set(
            "FileDescription",
            "MetaBrain — Mission Control for AI Coding Agents",
        );
        res.set("CompanyName", "MetaBrain");
        res.compile()
            .expect("could not embed the icon (rc.exe from the Windows SDK is needed)");
    }
}
