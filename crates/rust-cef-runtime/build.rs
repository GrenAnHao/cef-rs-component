use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

#[allow(dead_code)]
#[path = "src/build_stamp.rs"]
mod stamp;

fn main() {
    println!("cargo:rerun-if-changed=src/build_stamp.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let digest = Sha256::digest(stamp::HELPER_STAMP.as_bytes());
    let output = format!(
        "const HELPER_STAMP_LEN: usize = {};\nconst HELPER_STAMP_DIGEST: [u8; 32] = {:?};\n",
        stamp::HELPER_STAMP.len(),
        digest.as_slice(),
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("helper_stamp.rs"),
        output,
    )
    .expect("write helper stamp digest into Cargo OUT_DIR");
}
