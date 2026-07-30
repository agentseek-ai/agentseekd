fn main() {
    tauri_build::build();

    // Read version from ../package.json (single source of truth) and expose
    // it to Rust code via env!("APP_VERSION").
    let manifest = std::fs::read_to_string("../package.json")
        .expect("Failed to read ../package.json");
    let version: String = serde_json::from_str::<serde_json::Value>(&manifest)
        .expect("Failed to parse package.json")
        .get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("package.json has no \"version\" field");
    println!("cargo:rustc-env=APP_VERSION={}", version);

    // Rebuild if package.json changes.
    println!("cargo:rerun-if-changed=../package.json");
}
