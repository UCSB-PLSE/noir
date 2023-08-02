use rustc_version::{version, Version};

#[allow(dead_code)]
fn check_rustc_version() {
    assert!(
        version().unwrap() >= Version::parse("1.66.0").unwrap(),
        "The minimal supported rustc version is 1.66.0."
    );
}

fn main() {
    // check_rustc_version();
}
