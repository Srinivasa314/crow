# Crow Benchmarks

Small runtime benchmarks that compare Crow with equivalent Go programs.

Run:

```sh
./bench/run.sh
```

The script builds `crowc` and the runtime in release mode, compiles each Crow
benchmark, compiles each Go benchmark with `go build`, checks that paired
programs print identical results, and then times the binaries with
`hyperfine`.

Benchmarks:

- `fib`: recursive integer calls and branches.
- `mandelbrot`: floating point arithmetic and nested loops.
- `binary_trees`: allocation-heavy tree construction and recursive traversal.


## Results

Apple M1 (macOS, ARM64), Go 1.26.4, hyperfine mean of 10 runs:

| Benchmark    | Crow   | Go     | Crow / Go |
|--------------|--------|--------|-----------|
| fib          | 191 ms | 134 ms | 1.43×     |
| mandelbrot   | 50 ms  | 42 ms  | 1.19×     |
| binary_trees | 127 ms | 106 ms | 1.20×     |

### Why fib lags the other two

fib is the price of **checked integer arithmetic**: every `n - 1`, `n - 2`,
and the final `+` panics on overflow in Crow, while Go silently wraps — and
fib(38) executes three checked ops per call across ~150M calls, so nothing
amortizes them. The checks already use the hardware overflow flag
(`subs`/`adds`), but Cranelift materializes the flag into a register
(`cset` + `cbnz`) instead of fusing a single `b.vs` branch, which costs
~19 ms here. The rest of the gap (~38 ms) remains even with the checks
compiled out entirely and is baseline Cranelift-vs-Go code quality on
call-heavy code (pointer-auth in every prologue, register shuffling,
unfused immediates). Crow's stack-overflow guard measures at ~1 ms —
branch prediction hides it.

mandelbrot (pure float loops, no checks involved) and binary_trees
(allocation and GC; the adaptive nursery converges to 16 MiB for its
working set) both sit around 1.2×, which is the current Cranelift-vs-Go
codegen baseline.
