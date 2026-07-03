# Crow

A small Rust-flavored language, AOT-compiled to native code via Cranelift, with a precise generational GC. Two crates: `crowc/` (compiler: lexer → parser → typeck → mono → codegen) and `runtime/` (staticlib linked into every executable: GC, strings, arrays, panics).

## Commands

```sh
cargo build                                # crowc + runtime staticlib
cargo test                                 # full suite
cargo test --test lang                     # language semantics (crowc/tests/)
./target/debug/crowc run examples/hello.crow
```

Needs a system C compiler (`cc`) for the final link.

## Non-obvious constraints

- `.cargo/config.toml` forces frame pointers — the GC stack walker follows the frame-pointer chain through runtime frames. Don't remove it.
- Every semantic test in `crowc/tests/lang.rs` runs twice: normally and with `CROW_NURSERY_KB=64`, so the suite doubles as a GC relocation stress test. New language tests get this for free via the shared harness in `crowc/tests/common/`.
- GC descriptors are deduplicated by object shape and carry no type identity — anything needing runtime type info (e.g. future reflection) must add its own type-id table rather than lean on descriptors.

## Docs

- `BOOK.md` — the language guide
- `INTERNALS.md` — compiler/runtime architecture (mono-by-shape, GC design)
- `bench/README.md` — benchmark numbers and analysis
