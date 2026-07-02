//! End-to-end tests: compile each example with crowc, run the produced
//! binary, and compare stdout against the golden .out file. Also checks
//! runtime panics and compile errors.
//!
//! Run with `cargo test` after `cargo build` (the runtime staticlib must
//! exist next to the crowc binary).

use std::path::{Path, PathBuf};
use std::process::Command;

fn crowc() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crowc"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// The runtime staticlib is a separate workspace member; make sure it has
/// been built before linking anything.
fn ensure_runtime() {
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

struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}

fn compile_and_run(src: &Path, gc_log: bool) -> Output {
    ensure_runtime();
    let exe = std::env::temp_dir().join(format!(
        "crow-e2e-{}-{}",
        std::process::id(),
        src.file_stem().unwrap().to_string_lossy()
    ));
    let build = Command::new(crowc())
        .args(["build"])
        .arg(src)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("failed to run crowc");
    assert!(
        build.status.success(),
        "crowc build failed for {}:\n{}",
        src.display(),
        String::from_utf8_lossy(&build.stderr)
    );
    let mut cmd = Command::new(&exe);
    if gc_log {
        cmd.env("CROW_GC_LOG", "1");
        // A tiny nursery forces frequent collections.
        cmd.env("CROW_NURSERY_KB", "64");
    }
    let run = cmd.output().expect("failed to run compiled program");
    let _ = std::fs::remove_file(&exe);
    Output {
        stdout: String::from_utf8_lossy(&run.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
        code: run.status.code().unwrap_or(-1),
    }
}

fn compile_error(source: &str) -> String {
    ensure_runtime();
    let src = std::env::temp_dir().join(format!("crow-e2e-err-{}.crow", std::process::id()));
    std::fs::write(&src, source).unwrap();
    let out = Command::new(crowc())
        .args(["build"])
        .arg(&src)
        .arg("-o")
        .arg("/dev/null")
        .output()
        .expect("failed to run crowc");
    let _ = std::fs::remove_file(&src);
    assert!(!out.status.success(), "expected a compile error for:\n{source}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn check_example(name: &str) {
    let dir = repo_root().join("examples");
    let out = compile_and_run(&dir.join(format!("{name}.crow")), false);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let expected = std::fs::read_to_string(dir.join(format!("{name}.out"))).unwrap();
    assert_eq!(out.stdout, expected, "output mismatch for example '{name}'");
}

#[test]
fn example_hello() {
    check_example("hello");
}

#[test]
fn example_features() {
    check_example("features");
}

#[test]
fn example_fib() {
    check_example("fib");
}

#[test]
fn example_sized_ints() {
    check_example("sized_ints");
}

#[test]
fn example_mandelbrot() {
    check_example("mandelbrot");
}

#[test]
fn example_gc_stress() {
    check_example("gc_stress");
}

/// The stress test must also pass with a tiny nursery (64 KiB), which forces
/// hundreds of collections and exercises evacuation on every code path.
#[test]
fn gc_stress_tiny_nursery() {
    let src = repo_root().join("examples/gc_stress.crow");
    let out = compile_and_run(&src, true);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.ends_with("gc stress passed\n"), "stdout: {}", out.stdout);
    assert!(out.stderr.contains("major"), "expected major collections:\n{}", out.stderr);
}

#[test]
fn runtime_panics() {
    let cases = [
        ("fn main() { let xs = [1]; println(xs[3]); }", "index 3 out of bounds"),
        ("struct P { x: int } fn main() { let p: P = nil; println(p.x); }", "nil dereference"),
        ("fn main() { let z = 0; println(1 / z); }", "division by zero"),
        ("fn main() { assert(1 == 2); }", "assertion failed"),
        ("fn main() { let xs: [int] = []; println(pop(xs)); }", "pop on empty array"),
    ];
    for (src_text, want) in cases {
        let src = std::env::temp_dir().join(format!(
            "crow-e2e-panic-{}-{}.crow",
            std::process::id(),
            want.len()
        ));
        std::fs::write(&src, src_text).unwrap();
        let out = compile_and_run(&src, false);
        let _ = std::fs::remove_file(&src);
        assert_eq!(out.code, 101, "expected panic for: {src_text}");
        assert!(
            out.stderr.contains(want),
            "expected '{want}' in stderr, got: {}",
            out.stderr
        );
    }
}

#[test]
fn compile_errors() {
    let cases = [
        ("fn main() { println(1 + \"x\"); }", "mixed types"),
        ("fn main() { let x = nil; }", "cannot infer"),
        ("fn main() { unknown(); }", "unknown function"),
        ("fn main() { let x: int = 1; let y: bool = x; }", "type mismatch"),
        ("fn f(): int { let x = 1; } fn main() { }", "must return a value"),
        ("fn main() { break; }", "outside of a loop"),
        (
            "fn main() { let n = 1; let f = fn() { n = 2; }; f(); }",
            "capture by value",
        ),
        ("struct P { x: int } fn main() { let p = P { }; }", "missing field"),
    ];
    for (src, want) in cases {
        let err = compile_error(src);
        assert!(err.contains(want), "expected '{want}' in error, got: {err}");
    }
}

/// Closures, capture chains, and function values keep working when the GC
/// relocates closure objects (tiny nursery).
#[test]
fn closures_survive_gc() {
    let src = std::env::temp_dir().join(format!("crow-e2e-clo-{}.crow", std::process::id()));
    std::fs::write(
        &src,
        r#"
struct Box { v: int }
fn churn(n: int) {
    let xs: [Box] = [];
    for (let i = 0; i < n; i = i + 1) { push(xs, Box { v: i }); }
}
fn main() {
    let fs: [fn(): int] = [];
    for (let i = 0; i < 200; i = i + 1) {
        let b = Box { v: i };
        push(fs, fn(): int { return b.v; });
        churn(50);
    }
    let total = 0;
    for (let i = 0; i < len(fs); i = i + 1) {
        total = total + fs[i]();
    }
    println(total);
}
"#,
    )
    .unwrap();
    let out = compile_and_run(&src, true);
    let _ = std::fs::remove_file(&src);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "19900\n");
}
