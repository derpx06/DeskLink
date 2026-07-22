use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo always provides OUT_DIR"));
    let resource_xml = manifest_dir.join("src").join("desklink.gresource.xml");
    let resource_output = out_dir.join("desklink.gresource");

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
        .arg(&resource_output)
        .arg("--sourcedir")
        .arg(manifest_dir.join("src"))
        .arg(&resource_xml)
        .status()
        .expect("failed to run glib-compile-resources");
    if !status.success() {
        panic!("glib-compile-resources failed");
    }

    let locale_dir =
        env::var("DESKLINK_LOCALEDIR").unwrap_or_else(|_| "/usr/local/share/locale".to_string());
    let config = format!(
        "pub static VERSION: &str = \"{}\";\n\
         pub static GETTEXT_PACKAGE: &str = \"desklink\";\n\
         pub static LOCALEDIR: &str = \"{}\";\n",
        env::var("CARGO_PKG_VERSION").expect("Cargo always provides package version"),
        locale_dir.replace('\\', "\\\\").replace('"', "\\\""),
    );
    fs::write(out_dir.join("config.rs"), config).expect("Could not write generated config");
    println!("cargo:rerun-if-env-changed=DESKLINK_LOCALEDIR");
}
