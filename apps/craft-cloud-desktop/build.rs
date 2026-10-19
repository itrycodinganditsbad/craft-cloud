//! On Windows: `craft-cloud.exe`'s icon (Craft Cloud's, from `assets/`) and version resource.

fn main() {
    let icon = "assets/icon.ico";
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={icon}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon).set("ProductName", "Craft Cloud").set("FileDescription", "Craft Cloud");
    if let Err(e) = resource.compile() {
        println!("cargo::warning=no icon or version resource for the exe: {e}");
    }
}
