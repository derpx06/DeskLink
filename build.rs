use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    let resource_xml = manifest_dir.join("src").join("desklink.gresource.xml");
    let output = out_dir.join("desklink.gresource");

    println!("cargo:rerun-if-changed={}", resource_xml.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src").join("window.ui").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir
            .join("src")
            .join("shortcuts-dialog.ui")
            .display()
    );

    let status = Command::new("glib-compile-resources")
        .arg("--target")
        .arg(&output)
        .arg("--sourcedir")
        .arg(manifest_dir.join("src"))
        .arg(&resource_xml)
        .status()
        .expect("failed to run glib-compile-resources");

    if !status.success() {
        panic!("glib-compile-resources failed");
    }
}
