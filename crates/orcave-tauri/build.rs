fn main() {
    // Tell Cargo to re-run this build script if tauri.conf.json changes
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=../../frontend-tauri/dist/index.html");
    tauri_build::build();
}
