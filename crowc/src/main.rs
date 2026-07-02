//! crowc — the Crow compiler.
//!
//! Pipeline: lex -> parse -> typecheck -> Cranelift codegen (in-process
//! object emission) -> system linker (`cc`) against the Rust runtime
//! staticlib (GC + builtins).

mod ast;
mod codegen;
mod lexer;
mod mono;
mod parser;
mod typeck;
mod types;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn usage() -> ! {
    eprintln!(
        "usage:\n  crowc build <file.crow> [-o <output>]\n  crowc run <file.crow> [args...]"
    );
    std::process::exit(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => usage(),
    };
    match cmd {
        "build" => {
            let (src, out) = parse_build_args(rest);
            match build(&src, &out) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{}: {e}", src.display());
                    ExitCode::FAILURE
                }
            }
        }
        "run" => {
            let src = match rest.first() {
                Some(s) => PathBuf::from(s),
                None => usage(),
            };
            let exe = std::env::temp_dir().join(format!("crow-run-{}", std::process::id()));
            if let Err(e) = build(&src, &exe) {
                eprintln!("{}: {e}", src.display());
                return ExitCode::FAILURE;
            }
            let status = std::process::Command::new(&exe)
                .args(&rest[1..])
                .status()
                .expect("failed to launch compiled program");
            let _ = std::fs::remove_file(&exe);
            ExitCode::from(status.code().unwrap_or(1) as u8)
        }
        _ => usage(),
    }
}

fn parse_build_args(rest: &[String]) -> (PathBuf, PathBuf) {
    let mut src = None;
    let mut out = None;
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "-o" {
            if i + 1 >= rest.len() {
                usage();
            }
            out = Some(PathBuf::from(&rest[i + 1]));
            i += 2;
        } else if src.is_none() {
            src = Some(PathBuf::from(&rest[i]));
            i += 1;
        } else {
            usage();
        }
    }
    let src = src.unwrap_or_else(|| usage());
    let out = out.unwrap_or_else(|| {
        let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned());
        PathBuf::from(stem.unwrap_or_else(|| "a.out".to_string()))
    });
    (src, out)
}

fn build(src: &Path, out: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(src)
        .map_err(|e| format!("cannot read source file: {e}"))?;
    let tokens = lexer::lex(&source)?;
    let mut program = parser::parse(tokens)?;
    let checked = typeck::check(&mut program)?;
    let object_bytes = codegen::compile(&program, &checked)?;
    link(&object_bytes, out)
}

fn link(object: &[u8], out: &Path) -> Result<(), String> {
    let runtime = find_runtime()?;
    let obj_path = std::env::temp_dir().join(format!(
        "crow-{}-{}.o",
        std::process::id(),
        out.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    std::fs::write(&obj_path, object).map_err(|e| format!("cannot write object file: {e}"))?;

    let mut cmd = std::process::Command::new("cc");
    cmd.arg(&obj_path).arg(&runtime).arg("-o").arg(out);
    // The Rust staticlib runtime needs libstd's system dependencies.
    if cfg!(target_os = "linux") {
        cmd.args(["-lpthread", "-ldl", "-lm"]);
    }
    let result = cmd.output().map_err(|e| format!("failed to run linker 'cc': {e}"))?;
    let _ = std::fs::remove_file(&obj_path);
    if !result.status.success() {
        return Err(format!(
            "linking failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(())
}

/// Locate libcrow_runtime.a: next to the crowc executable (the cargo target
/// directory), or via the CROW_RUNTIME environment variable.
fn find_runtime() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("CROW_RUNTIME") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("CROW_RUNTIME points to a missing file: {}", p.display()));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().unwrap_or(Path::new("."));
    let candidate = dir.join("libcrow_runtime.a");
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(format!(
        "cannot find libcrow_runtime.a next to crowc ({}); build it with \
         `cargo build -p crow-runtime` or set CROW_RUNTIME",
        dir.display()
    ))
}
