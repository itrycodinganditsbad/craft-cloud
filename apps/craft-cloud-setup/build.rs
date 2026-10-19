//! The setup's payload and, on Windows, its icon and version resource.
//!
//! `cargo xtask package` builds the app and the agent first, then this crate with `CRAFT_CLOUD_SETUP_PAYLOAD` naming
//! the folder they are in: their bytes are then built in (`include_bytes!`, under `cfg(payload)`). Without it (every
//! other build: clippy, tests) the setup carries nothing and says so if run.

use std::path::Path;

/// The files the payload folder must hold.
const PAYLOAD: [&str; 3] = ["craft-cloud.exe", "craft-cloud-agent.exe", "craft-cloud-agent-bg.exe"];

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=CRAFT_CLOUD_SETUP_PAYLOAD");
    println!("cargo::rustc-check-cfg=cfg(payload)");
    if let Some(dir) = std::env::var_os("CRAFT_CLOUD_SETUP_PAYLOAD") {
        let dir = Path::new(&dir);
        let missing: Vec<&str> = PAYLOAD.iter().copied().filter(|name| !dir.join(name).is_file()).collect();
        if missing.is_empty() {
            for name in PAYLOAD {
                println!("cargo::rerun-if-changed={}", dir.join(name).display());
            }
            println!("cargo::rustc-cfg=payload");
            println!("cargo::rustc-env=CRAFT_CLOUD_SETUP_PAYLOAD={}", dir.display());
        } else {
            println!("cargo::warning=the setup is built without its payload: {} missing in {}", missing.join(", "), dir.display());
        }
    }
    resources("Craft Cloud setup");
}

/// The exe's icon (Craft Cloud's) and version resource, when building for Windows.
fn resources(description: &str) {
    let icon = "../craft-cloud-desktop/assets/icon.ico";
    println!("cargo::rerun-if-changed={icon}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon).set("ProductName", "Craft Cloud").set("FileDescription", description);
    if let Err(e) = resource.compile() {
        println!("cargo::warning=no icon or version resource for the exe: {e}");
    }
}
