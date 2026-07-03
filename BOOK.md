# The Crow Book

A guide to Crow: a small, statically typed, garbage-collected language with
Rust-flavored syntax, compiled ahead of time to native code. This book covers
the whole language; how the compiler and runtime work lives in
[INTERNALS.md](INTERNALS.md).

---

## 1. Getting started

```
fn main() {
    println("hello");
}
```

```sh
crowc run  prog.crow                 # compile + run
crowc build prog.crow -o prog        # produce a native executable
```

A program is one file: a sequence of top-level `struct`, `enum`, `impl`, and
`fn` declarations in any order (no forward declarations needed), with entry point `fn main()`.
Nothing else is allowed at top level — no globals, no top-level statements.

Memory is garbage-collected: you never allocate or free anything by hand.

## 2. Variables

```
let x = 5;                        // inferred: int
let y: u8 = 5;                    // annotated; the literal adopts u8
let m: Option<int> = Option.None; // a bare None needs an annotation to pick T
x = x + 1;                        // assignment
x += 1;                           // compound assignment (no ++ / --)
```

Only locals are inferred — function signatures are always fully annotated.
Variables are block-scoped and shadowing follows normal lexical rules.

Compound assignment exists for every arithmetic and bitwise operator:
`+= -= *= /= %= &= |= ^= <<= >>=`. `lhs op= v` behaves like
`lhs = lhs op v` except that the target's object and index subexpressions
are evaluated once (`a[f()] += 1` calls `f` once). `s += "x"` concatenates.

## 3. Types

| Type | Meaning |
|---|---|
| `i8 u8 i16 u16 i32 u32 i64 u64` | sized integers; `int` = alias of `i64`, the default |
| `float` | 64-bit IEEE double |
| `bool` | `true` / `false`, one byte in memory |
| `string` | immutable UTF-8 text |
| `StructName` | user struct |
| `EnumName` | user enum (§9); `Option<T>` is predeclared |
| `[T]` | growable array of `T` |
| `fn(T, ...): R` | function value (closure) |

**Value vs. reference:** numbers and bools are values. Structs, enums,
arrays, strings, and functions are **references** to heap objects —
assignment and parameter passing alias, never copy. There is no pointer
syntax, no address-of, no manual free.

**Storage is packed**: struct fields and array elements occupy their natural
size (`[u8]` is a real byte buffer, `bool` is one byte).

**There is no null.** Every reference always points at a real object, so
field access, indexing, and calls can never fail on a missing value. Absence
is a value of the predeclared `Option<T>` enum — `Option.Some(v)` or
`Option.None` — eliminated with `match` or `.unwrap()` (§9).

## 4. Integers

- **No implicit conversions.** `i32 + i64` is a compile error. Convert with
  `expr as Type`; a cast that doesn't fit **panics** at runtime.
- `as` also converts `int` ↔ `float` (`float as int` truncates, checked).
- **Literals adopt the expected type** from context: `let x: u8 = 5;` is
  fine, `let x: u8 = 300;` is a compile error. No context → `int`. Context
  flows through arithmetic: `let x: u8 = a + 1;` checks the `1` at `u8`.
- Unsigned types compare, divide, `%`, shift, and print as unsigned.
- **Overflow panics**: add/sub/mul overflow, `MIN / -1`, negating `MIN`,
  division/remainder by zero — all panic with a line number. There is no
  wrapping arithmetic; write wrap-around as widen-to-`u64`, mask, cast down:

```
h = (h * 33 + data[i] as u64) & 4294967295;   // 32-bit rolling hash in u64
```

  (Bit operations — `& | ^ ~ << >>` — never count as overflow: `<<` simply
  discards bits shifted out of the width. Only the shift *amount* is
  checked, §5.)

## 5. Operators

Precedence, loosest to tightest:

```
||
&&
==  !=
<  <=  >  >=
|
^
&
<<  >>
+  -
*  /  %
as Type
unary  -  !  ~
postfix: call ()   index []   field .
```

- `-x as i8` is `(-x) as i8`; `x as u8 * 2` is `(x as u8) * 2`.
- `&&` / `||` are short-circuiting, `bool` operands only. `!` needs `bool`.
- `+` works on two same-type numbers **or two strings** (concatenation).
- `%` is integer-only. `< <= > >=` need two ints of the same type or two
  floats — no ordering on strings or references.
- `==` / `!=` are type-directed: **strings compare by content, references
  by identity**, numbers/bools by value. Both sides must be the same type.
  `f == f` holds for a named function used as a value, and identity is
  structural for bare-only enums (§9); other enums reject `==`.
- **Bitwise** `& | ^ ~ << >>` are integer-only, both operands the same type
  (context flows through them like arithmetic, so `x & 1` works at any
  width). Unlike C, bitwise binds *tighter* than comparison:
  `x & 1 == 0` means `(x & 1) == 0`. Note shifts bind looser than `+`, so
  `1 << 2 + 1` is `1 << 3` (parenthesize when mixing).
- **Shifts**: the amount must be in `[0, bits)` — anything else (including
  a negative amount) panics with a line number. `>>` is arithmetic on
  signed types and logical on unsigned ones. `<<` discards bits shifted
  out of the width; that is a bit operation, not arithmetic overflow.

## 6. Control flow

```
if cond { ... } else if cond { ... } else { ... }

while cond { ... }

for (let i = 0; i < n; i += 1) { ... }      // C-style; each clause optional

break; continue; return; return expr;
```

Conditions are `bool` (no truthiness) and braces are mandatory. `if` and
`while` take no parentheses (plain grouping parens are fine); `for` keeps
them. One restriction: a struct literal cannot start unparenthesized in an
`if`/`while` condition — `Ident {` there is read as the body. Write
`if (p == Point { x: 1, y: 2 }) { ... }`; inside parens, brackets, or
argument lists the restriction lifts.

### `if` as an expression

In expression position `if` yields a value:

```
let max = if a > b { a } else { b };
let grade = if s >= 90 { "A" } else if s >= 80 { "B" } else { "C" };
```

The `else` is mandatory, each branch is a single expression (no statements),
branches must have the same type, and only the taken branch evaluates.
A statement *starting* with `if` is always parsed as the statement form
above.

## 7. Functions and lambdas

```
fn dist2(a: Point, b: Point): int { return a.x*b.x + a.y*b.y; }
fn side_effect(x: int) { println(x); }     // omitted return type = unit
```

- Full signature annotations required; no default args, no overloading,
  no varargs.
- Functions are **first-class**: a top-level function name used as a value
  becomes an `fn(...)` value.

**Tail expressions**: the final statement of a function or lambda body may
be a bare expression with no `;` — it is returned:

```
fn dist2(a: Point, b: Point): int { a.x*b.x + a.y*b.y }
```

This is pure sugar for `return expr;` (checked identically; for a unit
function it is an ordinary expression statement). It applies only at the
end of a body — not inside `if`/`while` blocks. A statement starting with
`if` is still the if *statement* (§6), so a tail conditional needs parens:
`fn sign(x: int): int { (if x < 0 { -1 } else { 1 }) }`.

**Lambdas** use the same syntax as declarations, as an expression:

```
fn make_adder(n: int): fn(int): int {
    fn(x: int): int { x + n }
}
```

Lambdas **capture by value at creation time**. Assigning to a captured
variable is a compile error; mutating *through* a captured reference
(`captured.field = ...`, `captured.push(...)`) works, because references
are copied but point at the same object.

## 8. Structs

```
struct Node { value: int, next: Option<Node> }   // recursion bottoms out in Option

let n = Node { value: 1, next: Option.None };    // literal: all fields, by name
n.value = 2;                                     // field read/write with .
```

Declarations are pure data: no visibility modifiers, no default values, no
inheritance. Methods live in separate `impl` blocks (§10). Struct values
are references; `a = b` aliases.

## 9. Enums and match

```
enum Shape {
    Circle(float),                  // wraps exactly one value...
    Rect { w: float, h: float },    // ...or carries named fields inline...
    Empty,                          // ...or nothing at all: a bare variant
}

let c = Shape.Circle(2.0);              // construction is qualified
let r = Shape.Rect { w: 3.0, h: 4.0 };  // field variants use literal syntax
```

An enum value is exactly one of its variants. A variant is **bare**,
**wraps a single value**, or carries **named fields** stored inline in the
enum value itself — a field variant is one object, not a wrapper around a
struct, so prefer it for multi-field payloads in allocation-heavy code.
Enum values are references, like structs.

**`match`** is the only way to look inside. As a statement, arms are blocks
(the comma after a block arm is optional):

```
match r {
    Shape.Circle(radius) => { println(radius); }  // binds the payload
    Shape.Rect { w, h } => { println(w * h); }    // binds each field
    Shape.Empty => { println("empty"); }
}
```

Arms must be **exhaustive**: cover every variant or end with a final `_`
arm. A wrapping variant's pattern must bind its payload, and a field
variant's pattern must name every field — `field: name` binds it, a lone
`field` is shorthand for `field: field`, and binding to `_` ignores the
value. Binders are new locals scoped to their arm, holding copies of the
payload values (a reference still aliases the shared object, like any
assignment).

In expression position `match` yields a value; each arm is a single
expression, comma-separated, and all arms must have one type:

```
let area = match s {
    Shape.Circle(radius) => 3.14159 * radius * radius,
    Shape.Rect { w, h } => w * h,
    Shape.Empty => 0.0,
};
```

A statement *starting* with `match` is always the statement form (the same
rule as `if`, §6), so a tail match-expression needs parens:
`fn area(s: Shape): float { (match s { ... }) }` — or write `return match ...;`.

`match` also works on **integers and bools**: arms are literals (including
`b'X'` byte literals against a `u8` scrutinee, §16). An integer match needs
a final `_` arm; a bool match is complete once `true` and `false` are both
covered.

```
let name = match n { 0 => "zero", 1 => "one", _ => "many" };
```

**Equality**: `==` works on an enum whose variants are all bare — bare
variants are shared singletons, so reference identity *is* structural
equality (`c == Color.Red` does what it says). On an enum with wrapping
variants `==` is a compile error; use `match`.

**`Option<T>`** is predeclared, exactly as if the program contained:

```
enum Option<T> { Some(T), None }
```

It replaces null everywhere: a recursive type spells its base case as
`Option.None` (`struct Node { value: int, next: Option<Node> }`), lookups
return `Option.None` for "not found", and `o.unwrap()` (§14) extracts the
`Some` payload, panicking on `None` with a line number. A user type named
`Option` shadows the prelude.

## 10. Methods

Methods are declared in `impl` blocks, Rust-style, on any struct or enum:

```
struct Point { x: int, y: int }

impl Point {
    fn new(x: int, y: int): Point { Point { x: x, y: y } }   // no self
    fn norm2(self): int { self.x * self.x + self.y * self.y }
    fn translate(self, dx: int, dy: int) {
        self.x += dx;
        self.y += dy;
    }
}

let p = Point.new(3, 4);      // associated function: called via the type
let n = p.norm2();            // method: called via a value
p.translate(1, 1);            // self aliases p — the mutation is visible
```

- The receiver is a bare **`self`** as the first parameter — no type
  annotation (`self` is a keyword, usable only in impl-block functions).
  Like every reference, `self` aliases the caller's object, so mutating
  through it is visible outside; there is no `&self`/`&mut self`
  distinction to spell.
- A function **without** `self` is an **associated function**, called as
  `Point.new(...)` — the usual home for constructors. On an enum, variant
  names win over associated functions and the two may not collide.
- A type may have **several impl blocks**; across all of them a method
  name must be unique and (on a struct) must not collide with a field name.
- Enums get methods the same way — typically a `match self` inside.

**Generic types** repeat their type parameters on the impl header, and a
method may add its own after them:

```
struct Pair<T> { a: T, b: T }

impl Pair<T> {
    fn swap(self): Pair<T> { Pair { a: self.b, b: self.a } }
    fn map<U>(self, f: fn(T): U): Pair<U> { Pair { a: f(self.a), b: f(self.b) } }
}
```

The impl's parameters are inferred from the receiver; a method's own
parameters are inferred from its arguments, like any generic call (§13).

**Bound methods**: naming a method without calling it builds a closure
that captures the receiver:

```
let f = p.norm2;      // fn(): int, capturing p
assert(f() == p.norm2());
```

The receiver is captured like a lambda capture: the reference is copied,
the object is shared. Each evaluation of `p.norm2` allocates a fresh
closure, so `p.norm2 == p.norm2` is false (function equality is identity,
§7). Two things cannot be bound: associated functions and builtin methods
(call-only), and methods whose *own* type parameters aren't determined by
the receiver (`p.map` above — nothing pins `U`).

One resolution rule worth knowing: `p.f(...)` is a method call when a
method `f` exists; otherwise `p.f` must be a field, and if that field
holds a function value the call goes through it. Field and method names
can never collide, so the two cases never compete.

## 11. Arrays

```
let xs = [1, 2, 3];          // inferred [int]
let ys: [string] = [];       // empty literal needs a context type
xs[0] = 10;                  // bounds-checked; panics if out of range
xs.push(4);                  // append (may reallocate; aliases stay valid)
let last = xs.pop();         // remove + return last (panics if empty)
xs.len();
let grid = [[1, 2], [3, 4]]; // nest freely
```

No slices, no array literals with a repeat count, no negative indexing.

## 12. Strings and bytes

Immutable. `+` concatenates, `==` compares content, `s.len()` gives byte
length.

**Byte indexing**: `s[i]` is the `i`-th **byte** of the string, of type
`u8`, bounds-checked like arrays. Strings are UTF-8, so a multi-byte
character is several bytes; the language never decodes at runtime. Compare
bytes against `b'X'` byte literals (§16):

```
if s[0] == b'-' { ... }
let digit = (s[i] - b'0') as int;
```

Writing through an index (`s[i] = ...`) is a compile error — strings stay
immutable. To *transform* text, round-trip through bytes: `s.to_bytes()`
copies the string's bytes into a fresh `[u8]`, and `bs.to_string()` builds
a new string from a byte array — panicking if the bytes are not valid
UTF-8, so every string a program can observe remains valid UTF-8:

```
fn upper(s: string): string {
    let bs = s.to_bytes();
    for (let i = 0; i < bs.len(); i += 1) {
        if bs[i] >= b'a' && bs[i] <= b'z' { bs[i] -= 32; }
    }
    bs.to_string()
}
```

No slicing, no interpolation — build strings with `+` and `.to_string()`;
parse them with `s[i]`, `.to_int()`, `.to_float()`, `.to_bytes()`.

## 13. Generics

```
fn id<T>(x: T): T { x }
struct Pair<T> { a: T, b: T }

let p = Pair { a: 1, b: 2 };        // T inferred from arguments...
let xs: [string] = empty();         // ...or from the expected type
```

- **No call-site type arguments** — inference only.
- Generic bodies are checked **once, with `T` opaque**: values of type `T`
  can be moved, stored, passed, returned — but not compared (`==` is a
  compile error), printed, or used in arithmetic. No bounds/traits exist.
- Generic functions must be called directly; they can't be used as values.
- Polymorphic recursion (`f<Pair<T>>` inside `f<T>`) compiles fine.

(Instantiations are shared aggressively under the hood — see
[INTERNALS.md](INTERNALS.md) if you're curious how.)

## 14. Builtins

Almost every built-in operation is a **method** on its receiver type; only
the four with no natural receiver are free functions:

| Free function | Does |
|---|---|
| `println(x)` / `print(x)` | any integer type, `float`, `bool`, `string` |
| `assert(cond)` | panic if false |
| `gc_collect()` | force a full collection |

| Builtin method | On | Does |
|---|---|---|
| `x.len()` | `string`, `[T]` | byte length / element count |
| `arr.push(v)` / `arr.pop()` | `[T]` | grow / shrink (`pop` **panics** when empty) |
| `o.unwrap()` | `Option<T>` | the `Some` payload; **panics** on `Option.None` |
| `i.to_string()` | any integer | decimal text (unsigned types print unsigned) |
| `f.to_string()` | `float` | decimal text |
| `i.to_float()` | any integer | exact-ish conversion (float → int is `f as int`, §4) |
| `s.to_int()` / `s.to_float()` | `string` | parse; **panics** on malformed input |
| `s.to_bytes()` | `string` | copy the bytes into a fresh `[u8]` |
| `bs.to_string()` | `[u8]` | build a string; **panics** on invalid UTF-8 |

`s.to_int()` accepts an optional leading `-` followed by decimal digits,
matching the whole string — no whitespace, no `+`. `s.to_float()` accepts
decimal or scientific notation (again with `-` only), and the result must
be finite. Anything else panics with the offending text and line number;
validate first with `s[i]` when input is untrusted.

Shadowing: a user function shadows a free builtin of the same name, and a
user *method* (on your own type — including a shadowing `Option`) shadows
a builtin method. Builtin methods and free builtins can only be called,
not used as values. One wrinkle: a receiver is checked without an expected
type, so a bare array literal is `[int]` — annotate before calling
`[u8]`-only methods (`let bs: [u8] = [104, 105]; bs.to_string()`).

## 15. When things go wrong

Every error path panics with a **line number**; there is no undefined
behavior:

- array or string index out of bounds; `.pop()` on empty
- `.unwrap()` of `Option.None`
- integer overflow, `/ 0`, `% 0`, `MIN / -1`, `-MIN`
- shift amount out of `[0, bits)`
- out-of-range `as` casts (including float → int)
- `.to_int()` / `.to_float()` on malformed input; `.to_string()` on a
  `[u8]` that is not valid UTF-8
- runaway recursion: `stack overflow at line N`

A panic prints its message to stderr and exits with code 101. Panics are
not catchable — there are no exceptions.

## 16. Appendix: lexical structure

- **Comments**: `// line` and `/* block */`. Block comments **nest**.
- **Identifiers**: `[A-Za-z_][A-Za-z0-9_]*`.
- **Keywords**: `fn struct enum impl self match let if else while for
  return break continue true false as`.
- **Integer literals**: decimal only. Untyped until context types them
  (§4); default `int`.
- **Float literals**: `4.5` — a `.` with digits makes it a float.
- **String literals**: `"..."`, escapes `\n \t \r \\ \" \0` and Unicode
  `\u{1F600}` (1–6 hex digits, any Unicode scalar). Literals may span
  multiple lines; the raw newline is kept in the string.
- **Byte literals**: `b'a'` is the byte value of one printable ASCII
  character, always of type `u8`. Escapes `\n \t \r \\ \' \0` and `\xNN`
  for arbitrary byte values (`b'\xff'`). Non-ASCII characters are a
  compile error — use `\xNN`.
- **Statements end with `;`** (except a tail expression, §7). Blocks are
  `{ ... }` and are required for all control-flow bodies (no braceless
  `if`).

## 17. Appendix: grammar sketch

```
program   := (struct | enum | impl | func)*
struct    := "struct" IDENT generics? "{" (IDENT ":" type),* "}"
enum      := "enum" IDENT generics? "{" (IDENT payload?),* "}"
payload   := "(" type ")" | "{" (IDENT ":" type),* "}"
impl      := "impl" IDENT generics? "{" method* "}"
method    := "fn" IDENT generics? "(" mparams ")" (":" type)? fbody
mparams   := "self" ("," IDENT ":" type)* | (IDENT ":" type),*
func      := "fn" IDENT generics? "(" (IDENT ":" type),* ")" (":" type)? fbody
generics  := "<" IDENT,+ ">"
type      := "int" | "i8".."u64" | "float" | "bool" | "string"
           | IDENT type_args? | "[" type "]" | "fn" "(" type,* ")" (":" type)?
block     := "{" stmt* "}"
fbody     := "{" stmt* expr? "}"     // trailing expr = implicit return
stmt      := "let" IDENT (":" type)? "=" expr ";"
           | lvalue assign_op expr ";"  | expr ";"
           | "if" cond block ("else" (if | block))?
           | "match" cond "{" (pattern "=>" block ","?)* "}"
           | "while" cond block
           | "for" "(" init? ";" cond? ";" step? ")" block
           | "return" expr? ";" | "break" ";" | "continue" ";"
assign_op := "=" | "+=" | "-=" | "*=" | "/=" | "%="
           | "&=" | "|=" | "^=" | "<<=" | ">>="
cond      := expr            // no leading struct literal unless parenthesized
expr      := precedence chain of §5 over:
             literal | IDENT | "self" | "(" expr ")"
           | IDENT "{" (IDENT ":" expr),* "}"        // struct literal
           | IDENT "." IDENT ("(" expr,* ")")?       // enum variant / assoc fn (checker-resolved)
           | IDENT "." IDENT "{" (IDENT ":" expr),* "}"   // field-variant literal
           | "[" expr,* "]"                          // array literal
           | "fn" "(" params ")" (":" type)? fbody   // lambda
           | if_expr | match_expr
           | expr "(" args ")" | expr "[" expr "]"
           | expr "." IDENT                          // field / bound method
           | expr "." IDENT "(" args ")"             // method call
           | expr "as" type
if_expr   := "if" cond "{" expr "}" "else" (if_expr | "{" expr "}")
match_expr:= "match" cond "{" (pattern "=>" expr),* "}"
pattern   := IDENT "." IDENT pargs?               // qualified variant
           | "-"? INT | BYTE | "true" | "false" | "_"
pargs     := "(" IDENT ")" | "{" (IDENT (":" IDENT)?),* "}"
```

## 18. What Crow deliberately doesn't have

Modules/imports, traits/interfaces, operator overloading, exceptions
(panics only, non-catchable), null (absence is `Option<T>`), nested match
patterns (one constructor deep only), string slicing/interpolation, hash
maps, and any form of manual memory management. One file in, one binary
out.
