use std::process::Command;

fn assert_version_output(flag: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_moraebot"))
        .arg(flag)
        .output()
        .expect("spawn moraebot");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    assert_eq!(stdout, format!("moraebot {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn long_version_flag_prints_version() {
    assert_version_output("--version");
}

#[test]
fn short_version_flag_prints_version() {
    assert_version_output("-V");
}
