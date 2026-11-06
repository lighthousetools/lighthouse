//! Rebuilds the crate when a migration is added, since they are embedded.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
