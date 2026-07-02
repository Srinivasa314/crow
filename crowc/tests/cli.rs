//! CLI behavior of the crowc binary itself.

mod common;
use common::{crowc, ensure_runtime};
use std::process::Command;

#[test]
fn build_to_custom_output_and_run() {
    ensure_runtime();
    let src = std::env::temp_dir().join(format!("crow-cli-{}.crow", std::process::id()));
    let exe = std::env::temp_dir().join(format!("crow-cli-{}-out", std::process::id()));
    std::fs::write(&src, "fn main() { println(40 + 2); }").unwrap();
    let build = Command::new(crowc()).arg("build").arg(&src).arg("-o").arg(&exe).output().unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
    let run = Command::new(&exe).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&exe);
}

#[test]
fn run_subcommand() {
    ensure_runtime();
    let src = std::env::temp_dir().join(format!("crow-cli-run-{}.crow", std::process::id()));
    std::fs::write(&src, "fn main() { println(\"via run\"); }").unwrap();
    let out = Command::new(crowc()).arg("run").arg(&src).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "via run\n");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn missing_source_file() {
    let out = Command::new(crowc())
        .args(["build", "/nonexistent/nope.crow"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read source file"));
}

#[test]
fn unknown_command_shows_usage() {
    let out = Command::new(crowc()).arg("frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

#[test]
fn no_arguments_shows_usage() {
    let out = Command::new(crowc()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

#[test]
fn run_propagates_panic_exit_code() {
    ensure_runtime();
    let src = std::env::temp_dir().join(format!("crow-cli-panic-{}.crow", std::process::id()));
    std::fs::write(&src, "fn main() { println(\"pre\"); assert(1 == 2); }").unwrap();
    let out = Command::new(crowc()).arg("run").arg(&src).output().unwrap();
    let _ = std::fs::remove_file(&src);
    assert_eq!(out.status.code(), Some(101), "run must forward the program's exit code");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "pre\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("assertion failed"));
}

#[test]
fn output_path_unwritable() {
    ensure_runtime();
    let src = std::env::temp_dir().join(format!("crow-cli-unw-{}.crow", std::process::id()));
    std::fs::write(&src, "fn main() { }").unwrap();
    let out = Command::new(crowc())
        .arg("build")
        .arg(&src)
        .args(["-o", "/nonexistent-dir/deeper/out"])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&src);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("linking failed"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dash_o_without_argument_shows_usage() {
    let src = std::env::temp_dir().join(format!("crow-cli-noarg-{}.crow", std::process::id()));
    std::fs::write(&src, "fn main() { }").unwrap();
    let out = Command::new(crowc()).arg("build").arg(&src).arg("-o").output().unwrap();
    let _ = std::fs::remove_file(&src);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

#[test]
fn source_is_a_directory() {
    let out = Command::new(crowc())
        .args(["build"])
        .arg(std::env::temp_dir())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read source file"));
}

#[test]
fn non_utf8_source_is_a_clean_error() {
    let src = std::env::temp_dir().join(format!("crow-cli-bin-{}.crow", std::process::id()));
    std::fs::write(&src, [0x66, 0x6e, 0xff, 0xfe, 0x00]).unwrap();
    let out = Command::new(crowc()).arg("build").arg(&src).output().unwrap();
    let _ = std::fs::remove_file(&src);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read source file"));
}

#[test]
fn compile_error_exits_nonzero() {
    let src = std::env::temp_dir().join(format!("crow-cli-err-{}.crow", std::process::id()));
    std::fs::write(&src, "fn main() { let x = ; }").unwrap();
    let out = Command::new(crowc()).arg("build").arg(&src).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected an expression"));
    let _ = std::fs::remove_file(&src);
}
