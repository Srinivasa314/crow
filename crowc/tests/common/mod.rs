//! Shared helpers for integration tests: compile Crow source strings with
//! the real crowc binary, run the produced executables, and check results.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub fn crowc() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crowc"))
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// The runtime staticlib is a separate workspace member; make sure it has
/// been built before linking anything.
pub fn ensure_runtime() {
    let lib = crowc().parent().unwrap().join("libcrow_runtime.a");
    if lib.exists() {
        return;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .args(["build", "-p", "crow-runtime"])
        .current_dir(repo_root())
        .status()
        .expect("failed to run cargo");
    assert!(status.success(), "building crow-runtime failed");
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

fn unique_path(ext: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("crow-test-{}-{}{}", std::process::id(), n, ext))
}

/// Compile source text; returns the executable path or the compiler stderr.
pub fn try_compile(source: &str) -> Result<PathBuf, String> {
    ensure_runtime();
    let src = unique_path(".crow");
    let exe = unique_path("");
    std::fs::write(&src, source).unwrap();
    let out = Command::new(crowc())
        .arg("build")
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("failed to run crowc");
    let _ = std::fs::remove_file(&src);
    if out.status.success() {
        Ok(exe)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

pub fn run_program(source: &str, envs: &[(&str, &str)]) -> Output {
    let exe = match try_compile(source) {
        Ok(exe) => exe,
        Err(e) => panic!("compilation failed:\n{e}\nsource:\n{source}"),
    };
    let mut cmd = Command::new(&exe);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let run = cmd.output().expect("failed to run compiled program");
    let _ = std::fs::remove_file(&exe);
    Output {
        stdout: String::from_utf8_lossy(&run.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
        code: run.status.code().unwrap_or(-1),
    }
}

/// Run a program that asserts its way through and prints "ok" at the end.
/// Each program runs twice: once normally and once with a tiny 64 KiB
/// nursery, which forces frequent GC through every code path.
pub fn check_ok(source: &str) {
    for envs in [&[][..], &[("CROW_NURSERY_KB", "64")][..]] {
        let out = run_program(source, envs);
        assert_eq!(
            out.code, 0,
            "exit {} (envs {envs:?})\nstderr: {}\nsource:\n{source}",
            out.code, out.stderr
        );
        assert!(
            out.stdout.ends_with("ok\n"),
            "missing ok sentinel (envs {envs:?})\nstdout: {}\nsource:\n{source}",
            out.stdout
        );
    }
}

/// Run a program and compare exact stdout (also dual-run).
pub fn check_output(source: &str, expected: &str) {
    for envs in [&[][..], &[("CROW_NURSERY_KB", "64")][..]] {
        let out = run_program(source, envs);
        assert_eq!(out.code, 0, "stderr: {}\nsource:\n{source}", out.stderr);
        assert_eq!(out.stdout, expected, "source:\n{source}");
    }
}

/// The program must die with a runtime panic (exit 101) mentioning `needle`
/// (also dual-run, so panics fire correctly under constant GC pressure).
pub fn expect_panic(source: &str, needle: &str) {
    for envs in [&[][..], &[("CROW_NURSERY_KB", "64")][..]] {
        let out = run_program(source, envs);
        assert_eq!(
            out.code, 101,
            "expected runtime panic, got exit {} (envs {envs:?})\nstdout: {}\nstderr: {}\nsource:\n{source}",
            out.code, out.stdout, out.stderr
        );
        assert!(
            out.stderr.contains(needle),
            "expected '{needle}' in stderr (envs {envs:?}), got: {}\nsource:\n{source}",
            out.stderr
        );
    }
}

/// The program must fail to compile with an error mentioning `needle`.
pub fn expect_compile_error(source: &str, needle: &str) {
    match try_compile(source) {
        Ok(exe) => {
            let _ = std::fs::remove_file(&exe);
            panic!("expected compile error containing '{needle}' for:\n{source}");
        }
        Err(stderr) => assert!(
            stderr.contains(needle),
            "expected '{needle}' in compile error, got: {stderr}\nsource:\n{source}"
        ),
    }
}
