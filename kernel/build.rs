// SPDX-License-Identifier: BSD-2-Clause
//! Links the kernel with its own script (higher-half PIE, W^X segments).

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("-none") {
        println!("cargo:rustc-link-arg-bins=-T{dir}/kernel.ld");
    }
    println!("cargo:rerun-if-changed=kernel.ld");
}
