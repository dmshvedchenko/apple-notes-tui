use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=scripts/notes_probe.applescript");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let profile_dir = out_dir
        .parent()
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
        .expect("OUT_DIR has Cargo target profile parents")
        .to_path_buf();
    let source = PathBuf::from("scripts/notes_probe.applescript");
    let destination_dir = profile_dir.join("scripts");
    let destination = destination_dir.join("notes_probe.applescript");

    fs::create_dir_all(&destination_dir).expect("create runtime script directory");
    fs::copy(&source, &destination).expect("copy canonical AppleScript sidecar");
}
