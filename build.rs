fn main() {
    println!(
        "cargo:rustc-env=TABA_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
