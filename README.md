# Crow

[![CI](https://github.com/Srinivasa314/crow/actions/workflows/ci.yml/badge.svg)](https://github.com/Srinivasa314/crow/actions/workflows/ci.yml)

A small, **Rust-flavored** programming language, compiled ahead of time to
native code with [Cranelift](https://cranelift.dev/) and running on a **precise generational garbage collector**. It supports Linux and macOS, x86-64 and ARM64.


```
enum Shape {
    Circle(float),
    Rect { w: float, h: float },    
}

impl Shape {
    fn area(self): float {
        (match self {
            Shape.Circle(r) => 3.14159 * r * r,
            Shape.Rect { w, h } => w * h,
        })
    }
}

fn find<T>(xs: [T], want: fn(T): bool): Option<T> {
    for (let i = 0; i < xs.len(); i += 1) {
        if want(xs[i]) { return Option.Some(xs[i]); }
    }
    Option.None
}

fn main() {
    let shapes = [Shape.Circle(1.5), Shape.Rect { w: 4.0, h: 5.0 }];
    let min = 10.0;                                  
    match find(shapes, fn(s: Shape): bool { s.area() > min }) {
        Option.Some(s) => { println("found: " + s.area().to_string()); }    // found: 20.0
        Option.None => { println("all smaller than " + min.to_string()); }
    }

    let name = find(["crow", "raven", "rook"], fn(n: string): bool { n.len() == 4 });
    println(name.unwrap());                          // crow
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
- Structs, enums (bare, single-value, or inline-field variants) with
  exhaustive `match`, growable arrays `[T]`, immutable UTF-8 strings
  (byte-indexable, `.to_bytes()`/`.to_string()` to and from `[u8]`),
  first-class functions and lambdas with by-value capture, and
  inference-only generics.
- **Methods** in Rust-style `impl` blocks on structs and enums (generic
  ones included), associated functions (`Point.new(1, 2)`), and bound
  methods — `p.norm` without a call is a closure over its receiver. The
  built-in operations are methods too: `xs.len()`, `xs.push(v)`,
  `o.unwrap()`, `n.to_string()`.
- `if` and `match` are expressions, and a function body's final bare
  expression is returned: `fn double(x: int): int { x * 2 }`.
- **No null**: absence is the predeclared `Option<T>` enum, and every error
  path — bounds, `unwrap` of `None`, integer overflow, division, casts,
  shifts, even runaway recursion — panics with a source line number.

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
  and remembered set; a bump-allocated nursery — sized adaptively
  (512 KiB–64 MiB) from observed survival — is evacuated Cheney-style into
  a mark-sweep old generation. Objects really move — the test suite reruns
  every semantic test with a 64 KiB nursery to prove it.
- **Monomorphization by shape**: all reference instantiations of a generic
  function share one compiled body; scalar ones specialize by width and
  register class. GC descriptors deduplicate by object shape — enums keep
  their variant tag in a spare header word, so the GC never knows they
  exist, and bare variants are allocation-free static singletons.
- **Everything panics with a line number**, including stack overflow — a
  prologue guard in non-leaf functions that costs nothing measurable.
- **Performance** is within 1.2–1.4× of Go on small benchmarks, including
  an allocation-bound tree benchmark (the upper end is fib paying for
  checked integer arithmetic); see [bench/](bench/README.md) for numbers
  and analysis.

## Testing

`cargo test` runs five suites:

- **Unit tests** in `crowc` for the lexer, parser, and type checker
  (token streams, AST shapes, inference results, ~50 diagnostic cases, and
  compiler limits: struct fields, locals, captures, nesting depth).
- **Unit tests** in `runtime` for the pure helpers (float formatting, buffer
  size accounting); the GC itself is exercised end-to-end.
- **`tests/lang.rs`** — language semantics end-to-end: every operator and
  type, division/overflow edge cases, short-circuit evaluation, closures and
  capture rules, enums and match, `Option`, reference semantics, runtime
  panics with line numbers.
  Every semantic program — including the panic tests — runs **twice**: once
  normally and once with a 64 KiB nursery (`CROW_NURSERY_KB=64`), so the
  whole suite doubles as a GC relocation stress test. GC-specific tests cover
  mixed scalar/reference field bitmaps, the write barrier, large-object
  pretenuring, and collections during 8000-deep recursion (a stack-walker
  workout).
- **`tests/e2e.rs`** — the example programs against golden outputs.
- **`tests/cli.rs`** — crowc CLI behavior, including error paths and exit
  codes.
