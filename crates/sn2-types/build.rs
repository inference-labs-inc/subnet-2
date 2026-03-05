fn main() {
    println!("cargo:rerun-if-env-changed=SN2_VERSION");
    println!("cargo:rerun-if-env-changed=SN2_RELEASE_CHANNEL");
}
