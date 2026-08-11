fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // libzmq's Windows TCP socket implementation uses Advapi32 security APIs.
        println!("cargo::rustc-link-lib=advapi32");
    }
}
