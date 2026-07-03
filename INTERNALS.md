# Crow internals

How the compiler and runtime work. For the language itself, read
[the book](BOOK.md).

## Pipeline

Two Rust crates:

- **`crowc`** — the compiler: lexer → recursive-descent parser → type checker
  (name resolution, local type inference, closure capture analysis) →
  monomorphization → code generation with [Cranelift]. Machine code is
  emitted **in-process** into a native object file (Mach-O/ELF); the only
  external step is linking with the system `cc`, exactly like rustc does.
  Cranelift's fast single-pass code generation is what keeps `crowc build`
  quick.
- **`runtime`** — `libcrow_runtime.a`, linked into every executable: the
  garbage collector, strings, arrays, printing, and panics.

[Cranelift]: https://cranelift.dev/

## Monomorphization by shape

Generic functions are compiled once per *shape* of their type arguments, not
once per type. A shape is exactly what codegen and the GC care about: the ABI
register class (float vs word), whether the value is a GC reference (stack
maps, write barriers, descriptor refmaps), and the packed storage width and
signedness (struct layout, array elements). Every reference type — strings,
structs, enums, arrays, functions — shares the `Ref` shape, so `id<string>` and
`id<Point>` share one compiled body, and generic structs instantiated at
reference types share one GC descriptor. Scalar instantiations specialize by
width, signedness, and register class.

Instantiation substitutes each type argument's *canonical type* (a
representative of its shape) into a clone of the checked AST, so codegen runs
on fully concrete types and never sees a type parameter. This keying also
makes polymorphically recursive programs compile: `f<Pair<T>>` inside `f<T>`
collapses to the `Ref` shape no matter how deep the nesting, so the set of
instantiations is finite.

Two user-visible consequences: generic functions have no single compiled
symbol, so they cannot be used as values; and `==` is unavailable on bare
type parameters, because equality is type-directed (identity for references,
content for strings) and a shared reference-shape body could not pick
between them.

## Methods

Methods are entirely a front-end construct. The parser flattens each impl
block into ordinary top-level functions named `Type.method` — a name no
identifier can spell, so no collision with user functions — with `self`
desugared to a first parameter of the impl type and the impl's type
parameters prepended to the method's own. The checker keeps a
`(type, name) → function` table; a method call `x.m(a)` type-checks the
receiver once, then rewrites into a direct call `Type.m(x, a)`, so
monomorphization and codegen never know methods exist: an `impl Pair<T>`
method is just a generic function whose first parameter drives inference,
and it shares instantiations by shape like any other. Builtin "methods"
(`len`, `push`, `to_string`, ...) are the same checker rewrite targeting
the old builtin operations.

A *bound method* (`x.m` with no call) compiles like a one-capture lambda:
a fresh closure `[code pointer, receiver]` whose code is a per-(method,
shape) **bind thunk** that loads the receiver back out of the environment
and calls the method directly. Since a receiver is always a reference, all
bind closures share one static GC descriptor (two payload words, refmap
`0b10`).

## Object model

Every heap object has a 16-byte header followed by 8-byte payload words:

```
word 0: descriptor pointer | flags (MARK / FWD / STATIC in low bits)
word 1: aux (string byte length, buffer capacity)
16..:   payload (struct fields / closure captures / bytes / elements)
```

Descriptors are static data — emitted by the compiler for each struct and
closure shape — holding the payload size and a bitmap of which words are
references. Struct fields and array elements are stored packed at their
natural size, aligned to that size; since references are 8 bytes wide they
always land on word boundaries, which the refmap relies on. String literals
are emitted as pre-built `STATIC` objects in the data segment, so the GC
never touches them.

Enum values are ordinary struct-kind objects that keep their **variant tag
in the aux word** — free for struct-kind objects, and copied with the header
when the GC moves the object. A variant's payload — one slot for a
single-value variant, packed fields (exactly like struct fields) for an
inline-field variant — lays out per variant, and each variant instantiation
gets a descriptor through the same shape-keyed cache as structs, so a
one-ref-slot variant shares its descriptor with every one-ref-field struct;
the tag lives in the object, never in the (shared, identity-free)
descriptor, and the GC needs no enum-specific code at all.
This is sound because an object's variant can never change in place — Crow
has no way to overwrite a whole payload, only fields within it. Bare
variants are emitted as pre-built `STATIC` singletons (like string
literals): constructing one is just taking its address, and for enums whose
variants are all bare, `==` is correct as plain pointer identity. `match`
compiles to a load of the aux word plus a compare chain; the final arm is an
unconditional fallthrough because the checker proved exhaustiveness. One
consequence of spending aux: strings, buffers, and enums have now used the
header's spare word, so any future per-object metadata (say, a reflection
type-id) must grow the header rather than squeeze in.

Arrays are `{ buf, len, cap }` structs pointing at a separate buffer object,
so `push` can reallocate while existing references stay valid. Closures are
`{ code pointer, captures... }`; a top-level function used as a value
resolves to a single *static* closure object wrapping a thunk that adapts
the closure calling convention to a direct call — so `f == f` holds for
named functions and using a function name as a value allocates nothing.

## The garbage collector

Precise, generational, two generations:

- **Nursery**: contiguous bump allocation. A **minor GC** evacuates live
  objects into the old generation Cheney-style and resets the bump pointer.
  Objects that survive one minor collection are promoted. The nursery is
  **sized adaptively** from the survival ratio each minor GC observes: it
  starts at 512 KiB, doubles (up to 64 MiB) after two consecutive full
  collections that promote ≥ 1/4 of it — the signature of a live working
  set that doesn't fit — and halves (back down to 512 KiB) after sixteen
  consecutive collections promoting ≤ 1/50. Resizing happens right after
  evacuation, the one moment the nursery is empty and nothing points into
  it. `CROW_NURSERY_KB` pins the size and disables adaptation.
- **Old generation**: individually allocated blocks, collected by
  **mark-sweep** when promoted volume crosses a threshold (8 MiB or 2× the
  live size after the last major GC).

Precision comes from these root sources:

1. **Stack maps for compiled code.** The compiler declares reference-typed
   values to Cranelift (`declare_var_needs_stack_map`); its safepoint pass
   spills live references to stack slots around every call, rewrites later
   uses into reloads, and emits a stack map per call site. crowc serializes
   the maps — return-address offset, frame size, root offsets — into a
   `crow_stackmaps` data section. At collection time the runtime walks the
   frame-pointer chain (hence `preserve_frame_pointers` and
   `-Cforce-frame-pointers` for the runtime). When a frame record's return
   address is in the table, the suspended Crow function's own frame-record
   address is the record's saved-fp word, its SP is that minus the frame
   size Cranelift recorded, and the live root slots sit at SP + offset; the
   GC rewrites them when it moves objects. Deriving SP from the function's
   *own* record keeps the walk independent of where callees place their
   frame records (LLVM on Linux, unlike macOS, doesn't keep them at the top
   of the frame). Liveness is per-safepoint, so dead references never
   retain garbage.
2. **Write barrier + remembered set.** Every store of a reference into a heap
   object goes through `crow_write_ref`, which records old-generation fields
   that point into the nursery. Minor GCs then never need to scan the old
   generation. (These are interior pointers — safe because the old
   generation never moves.)
3. **Runtime-internal roots** for runtime functions (e.g. string concat,
   array growth) that hold references across their own allocations.

> Note: Cranelift **0.132 is a hard minimum**. Earlier versions have a bug in
> the user-stack-map safepoint pass — uses of marked values reached through
> SSA aliases (which the frontend creates for variables used across loop
> blocks) were not rewritten into spill-slot reloads, so compiled code kept
> using stale pre-GC pointers from registers. Found while testing this very
> collector; fixed upstream by the `rewrite_uses_of_alias_values` change.

## Panics and the stack guard

Every error path in the book's "everything is defined" list compiles to a
branch that calls a dedicated runtime function (`crow_panic_bounds`,
`crow_panic_overflow`, ...) with the source line baked in as a constant.
Panics print to stderr, flush stdout, and exit with code 101 — they are not
catchable.

Runaway recursion is caught by a stack guard: every *non-leaf* function
prologue compares the stack pointer against `crow_stack_limit`, computed at
startup as the OS-reported stack bottom plus 256 KiB of slack (enough for
the deepest runtime path — an allocation triggering a full collection with
stack walking — plus printing the panic itself). Leaf functions cannot
deepen the call chain, so they skip the check entirely; their bounded frames
are covered by the slack, which makes the guard's cost unmeasurable even in
call-heavy code. The limit is zero until `crow_rt_init` runs, so code that
executes before initialization passes trivially.

## Design choices and limitations (v1)

- Promotion is all-or-nothing after one survival (no aging), and major GC is
  non-incremental mark-sweep (no compaction of the old generation).
- Closures capture by value; there are no reference cells (`upvalues`).
- Descriptor sharing assumes descriptors are *only* GC metadata: two
  different struct types with the same payload shape point at the same
  descriptor, so descriptor identity says nothing about type identity.
  Adding runtime type information later (reflection, downcasting) must not
  key off descriptors — un-share them, or add a type-id table beside the
  descriptor pointer in the object header.
- Derived interior pointers (field/element addresses) are kept correct by
  construction: the compiler always computes them immediately before the
  store/load that consumes them, never across a safepoint, and the only call
  they flow into (`crow_write_ref`) never allocates. Compound assignment
  recomputes (and re-checks) the element address after evaluating its
  right-hand side for the same reason — the RHS may have allocated (moving
  the buffer) or popped the array.
- The runtime's `extern "C"` entry points are `unsafe` with one shared
  contract: pointer arguments must be valid Crow heap objects — the language
  has no nil, so references are never null and compiled code emits no null
  checks anywhere. Their only callers are compiler-generated code and the
  runtime itself.

## Debugging knobs

- `CROW_GC_LOG=1` — log every collection and an exit summary.
- `CROW_NURSERY_KB=N` — pin the nursery size, disabling adaptive resizing
  (e.g. 64 to force frequent collections; the test suite runs every
  semantic test this way to stress-test relocation).
- `CROW_STACK_KB=N` — cap the usable stack below the startup frame (mainly
  for the stack-guard tests).
- `CROW_GC_DEBUG=1` — trace every stack-map root the collector walks.
- `CROW_DUMP_CLIF=1` (on crowc) — dump the final Cranelift IR per function.

```
[crow-gc] minor #9: 512 KiB live -> promoted 5 KiB in 57µs
[crow-gc] major #1: 8413 KiB -> 2522 KiB live in 11.4ms
[crow-gc] exit: 37 minor / 1 major collections, 5348 KiB promoted
```
