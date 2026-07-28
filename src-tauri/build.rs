fn main() {
    #[cfg(target_os = "windows")]
    println!("cargo:rustc-link-arg=/STACK:8388608");

    tauri_build::build()
}
