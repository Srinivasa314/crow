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

