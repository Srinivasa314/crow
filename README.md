# Crow

[![CI](https://github.com/Srinivasa314/crow/actions/workflows/ci.yml/badge.svg)](https://github.com/Srinivasa314/crow/actions/workflows/ci.yml)

A small, **Rust-flavored** programming language, compiled ahead of time to
native code with [Cranelift](https://cranelift.dev/) and running on a **precise generational garbage collector**. It supports Linux and macOS, x86-64 and ARM64.


```
struct Star { name: string, mag: float }

fn brightest(stars: [Star]): Star {
    let best = stars[0];
    for (let i = 1; i < len(stars); i += 1) {
        best = if stars[i].mag < best.mag { stars[i] } else { best };
    }
    best
}

fn last<T>(xs: [T]): T { xs[len(xs) - 1] }

fn main() {
    let sky = [
        Star { name: "Vega",   mag: 0.03 },
        Star { name: "Sirius", mag: -1.46 },
        Star { name: "Deneb",  mag: 1.25 },
    ];
    println("brightest: " + brightest(sky).name);   // brightest: Sirius
    println("last entry: " + last(sky).name);       // last entry: Deneb
    println(itos(len(sky)) + " stars, from magnitude " + ftos(brightest(sky).mag));
}
```

## Building and running

```sh
cargo build                              # builds crowc + the runtime staticlib
./target/debug/crowc run examples/hello.crow
./target/debug/crowc build examples/fib.crow -o fib && ./fib
cargo test                               # full test suite (see below)
```

Requirements: Rust toolchain and a system C compiler (`cc`, used only for the
final link).

## The language

Statically typed with inferred locals, reference semantics for heap types,
and no undefined behavior. The short version:

- Sized integers with **no implicit conversions**, checked
  `as` casts, and literals that adopt the type context expects; bitwise
  operators and compound assignment included.
- Structs, growable arrays `[T]`, immutable UTF-8 strings (byte-indexable,
  `stob`/`btos` to and from `[u8]`), first-class functions and lambdas with
  by-value capture, and inference-only generics.
- `if` is an expression, and a function body's final bare expression is
  returned: `fn double(x: int): int { x * 2 }`.
- `nil` is the checked null reference, and every error path — bounds,
  integer overflow, division, casts, shifts, even runaway recursion — panics
  with a source line number.

**[Read the book](BOOK.md)** for the full guide. `examples/` has runnable
programs — `features.crow` is a tour of the whole language.

## Implementation

Two Rust crates: `crowc` (the compiler) and `runtime` (a staticlib linked
into every executable). Key points, expanded in
**[INTERNALS.md](INTERNALS.md)**:

- **In-process codegen**: lexer → recursive-descent parser → type checker →
  Cranelift, emitting a native object file directly; the only external step
  is the final `cc` link, exactly like rustc.
- **Precise, moving GC**: compiler-emitted stack maps plus a write barrier
  and remembered set; a bump-allocated nursery is evacuated Cheney-style
  into a mark-sweep old generation. Objects really move — the test suite
  reruns every semantic test with a 64 KiB nursery to prove it.
- **Monomorphization by shape**: all reference instantiations of a generic
  function share one compiled body; scalar ones specialize by width and
  register class. GC descriptors deduplicate by object shape.
- **Everything panics with a line number**, including stack overflow — a
  prologue guard in non-leaf functions that costs nothing measurable.

## Testing

`cargo test` runs five suites:

- **Unit tests** in `crowc` for the lexer, parser, and type checker
  (token streams, AST shapes, inference results, ~50 diagnostic cases, and
  compiler limits: struct fields, locals, captures, nesting depth).
- **Unit tests** in `runtime` for the pure helpers (float formatting, buffer
  size accounting); the GC itself is exercised end-to-end.
- **`tests/lang.rs`** — language semantics end-to-end: every operator and
  type, division/overflow edge cases, short-circuit evaluation, closures and
  capture rules, nil, reference semantics, runtime panics with line numbers.
  Every semantic program — including the panic tests — runs **twice**: once
  normally and once with a 64 KiB nursery (`CROW_NURSERY_KB=64`), so the
  whole suite doubles as a GC relocation stress test. GC-specific tests cover
  mixed scalar/reference field bitmaps, the write barrier, large-object
  pretenuring, and collections during 8000-deep recursion (a stack-walker
  workout).
- **`tests/e2e.rs`** — the example programs against golden outputs.
- **`tests/cli.rs`** — crowc CLI behavior, including error paths and exit
  codes.
