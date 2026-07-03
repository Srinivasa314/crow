//! Language semantics tests. Every program here runs twice: once with the
//! default nursery and once with a 64 KiB nursery (constant GC pressure),
//! so each test also exercises object relocation through its code paths.

mod common;
use common::{check_ok, check_output, expect_compile_error, expect_panic, run_program};

#[test]
fn integer_arithmetic() {
    check_ok(
        r#"
fn main() {
    assert(2 + 3 * 4 == 14);
    assert((2 + 3) * 4 == 20);
    assert(10 - 3 - 2 == 5);
    assert(7 / 2 == 3);
    assert(-7 / 2 == -3);
    assert(7 / -2 == -3);
    assert(7 % 3 == 1);
    assert(-7 % 3 == -1);
    assert(7 % -3 == 1);
    assert(-(5) == 0 - 5);
    let max = 9223372036854775807;
    let min = -9223372036854775808;
    assert(max - 1 + 1 == max);     // touches the bound without overflowing
    assert(min + 1 - 1 == min);
    assert(min % -1 == 0);
    assert(1 < 2 && 2 <= 2 && 3 > 2 && 3 >= 3 && 1 != 2 && 2 == 2);
    println("ok");
}
"#,
    );
}

#[test]
fn float_arithmetic() {
    check_ok(
        r#"
fn main() {
    assert(0.1 + 0.2 != 0.3);   // IEEE 754 is honest
    assert(1.5 * 2.0 == 3.0);
    assert(7.0 / 2.0 == 3.5);
    assert(1.0 - 0.5 == 0.5);
    assert(-1.5 < 0.0 && 2.5 >= 2.5 && 1.0 != 2.0 && 2.0 <= 2.0 && 3.0 > 1.0);
    assert(itof(3) == 3.0);
    assert(ftoi(2.9) == 2);
    assert(ftoi(-2.9) == -2);
    assert(itof(1) / 2.0 == 0.5);
    let inf = 1.0 / 0.0;
    assert(inf > 0.0);
    println("ok");
}
"#,
    );
}

#[test]
fn bools_and_short_circuit() {
    check_ok(
        r#"
fn note(log: [int], v: int, r: bool): bool { push(log, v); return r; }
fn main() {
    assert(true && true);
    assert(!(true && false));
    assert(false || true);
    assert(!false);
    assert(true == true && true != false);
    let log: [int] = [];
    if (note(log, 1, false) && note(log, 2, true)) { assert(false); }
    assert(len(log) == 1);                 // rhs of && not evaluated
    if (note(log, 3, true) || note(log, 4, true)) { } else { assert(false); }
    assert(len(log) == 2);                 // rhs of || not evaluated
    assert(log[0] == 1 && log[1] == 3);
    println("ok");
}
"#,
    );
}

// Bools are stored as single bytes: they pack with small ints inside a
// payload word and shift the offsets of later reference fields, so this
// doubles as a descriptor/refmap test under GC pressure.
#[test]
fn bool_storage_is_byte_sized() {
    check_ok(
        r#"
struct Flags { a: bool, b: bool, tag: u8, c: bool, name: string, d: bool, next: Option<Flags> }
fn flip(f: Flags) { f.a = !f.a; f.b = !f.b; f.c = !f.c; f.d = !f.d; }
fn main() {
    let f = Flags { a: true, b: false, tag: 7, c: true, name: "x", d: false, next: Option.None };
    assert(f.a && !f.b && f.c && !f.d && f.name == "x");
    flip(f);
    assert(!f.a && f.b && !f.c && f.d);
    assert(f.tag == 7);                    // neighbors in the same word untouched

    // A linked chain with byte-packed flags survives collection intact.
    let head: Option<Flags> = Option.None;
    for (let i = 0; i < 100; i = i + 1) {
        head = Option.Some(Flags {
            a: i % 2 == 0, b: i % 3 == 0, tag: 1, c: true,
            name: itos(i), d: false, next: head,
        });
    }
    gc_collect();
    let cur = head;
    let k = 99;
    let go = true;
    while go {
        match cur {
            Option.Some(n) => {
                assert(n.a == (k % 2 == 0) && n.b == (k % 3 == 0) && n.c && !n.d);
                assert(n.name == itos(k));
                cur = n.next;
                k = k - 1;
            }
            Option.None => { go = false; }
        }
    }
    assert(k == -1);

    // [bool] is a real byte buffer: literals, growth, index, set, pop.
    let xs = [true, false, true];
    assert(len(xs) == 3 && xs[0] && !xs[1] && xs[2]);
    let ys: [bool] = [];
    for (let i = 0; i < 100; i = i + 1) { push(ys, i % 3 == 0); }   // forces regrowth
    assert(len(ys) == 100);
    gc_collect();
    for (let i = 0; i < 100; i = i + 1) { assert(ys[i] == (i % 3 == 0)); }
    ys[50] = !ys[50];
    assert(ys[50]);                        // 50 % 3 != 0, now flipped on
    assert(pop(ys) == true && len(ys) == 99);   // 99 % 3 == 0
    assert(pop(ys) == false);                   // 98 % 3 != 0

    // Register form and closure captures stay full-width words.
    let flag = true;
    let get = fn(): bool { return flag; };
    assert(get());
    println("ok");
}
"#,
    );
}

#[test]
fn strings() {
    check_ok(
        r#"
fn main() {
    let s = "hello" + ", " + "world";
    assert(s == "hello, world");
    assert(len(s) == 12);
    assert(len("") == 0);
    assert("" + "" == "");
    assert("a" != "b");
    assert(len("héllo") == 6);             // byte length
    assert(len("\n\t\\\"") == 4);          // escapes
    assert(itos(0) == "0");
    assert(itos(-42) == "-42");
    assert(ftos(2.5) == "2.5");
    assert(ftos(1.0) == "1.0");
    let built = "";
    for (let i = 0; i < 3; i = i + 1) { built = built + itos(i); }
    assert(built == "012");
    println("ok");
}
"#,
    );
}

#[test]
fn multiline_strings_and_unicode_escapes() {
    check_ok(
        r#"
fn main() {
    let two = "line one
line two";
    assert(len(two) == 17);                    // embedded newline is kept
    assert(two == "line one" + "\n" + "line two");
    assert("\u{48}\u{69}" == "Hi");            // ASCII via \u
    assert("\u{e9}" == "é");                   // 2-byte UTF-8
    assert(len("\u{e9}") == 2);
    assert(len("\u{2192}") == 3);              // 3-byte UTF-8
    assert(len("\u{1F600}") == 4);             // 4-byte UTF-8 (emoji)
    assert("\u{1F600}" == "😀");               // escape and raw char agree
    println("ok");
}
"#,
    );
}

#[test]
fn parenless_conditions() {
    check_ok(
        r#"
struct Point { x: int, y: int }

fn is7(p: Point): bool { return p.x == 7; }

fn main() {
    let n = 0;
    let i = 0;
    while i < 10 {
        if i % 2 == 0 { n = n + 1; }
        else if i == 5 { n = n + 100; }
        i = i + 1;
    }
    assert(n == 105);
    if (n == 105) { n = 0; }                   // parens still plain grouping
    assert(n == 0);
    let p = Point { x: 7, y: 0 };
    if is7(Point { x: 7, y: 1 }) { n = 1; }    // struct lit fine in call args
    assert(n == 1);
    if (p == Point { x: 7, y: 0 }) { n = 2; }  // parenthesized: identity, no
    assert(n == 1);
    println("ok");
}
"#,
    );
}

#[test]
fn arrays() {
    check_ok(
        r#"
fn main() {
    let xs = [1, 2, 3];
    assert(len(xs) == 3 && xs[0] == 1 && xs[2] == 3);
    xs[1] = 20;
    assert(xs[1] == 20);
    for (let i = 0; i < 100; i = i + 1) { push(xs, i); }   // forces regrowth
    assert(len(xs) == 103);
    assert(xs[102] == 99);
    assert(pop(xs) == 99);
    assert(len(xs) == 102);
    let empty: [string] = [];
    assert(len(empty) == 0);
    push(empty, "x");
    assert(empty[0] == "x");
    assert(pop(empty) == "x");
    assert(len(empty) == 0);
    let grid = [[1, 2], [3, 4]];
    grid[0][1] = 9;
    assert(grid[0][1] == 9 && grid[1][0] == 3);
    let floats = [1.5, 2.5];
    push(floats, 3.5);
    assert(pop(floats) == 3.5);
    assert(floats[0] + floats[1] == 4.0);
    let bools = [true, false];
    assert(bools[0] && !bools[1]);
    let alias = xs;
    alias[0] = 111;
    assert(xs[0] == 111);          // reference semantics
    assert(xs == alias);           // identity equality
    println("ok");
}
"#,
    );
}

#[test]
fn structs() {
    check_ok(
        r#"
struct Point { x: int, y: int }
struct Seg { a: Point, b: Point, name: string }
struct Node { value: int, next: Option<Node> }
fn main() {
    let p = Point { x: 1, y: 2 };
    p.x = 10;
    assert(p.x == 10 && p.y == 2);
    let s = Seg { a: p, b: Point { x: 3, y: 4 }, name: "s1" };
    assert(s.a.x == 10 && s.b.y == 4 && s.name == "s1");
    s.a.y = 99;
    assert(p.y == 99);             // reference semantics
    let q = p;
    assert(p == q);                // identity...
    let r = Point { x: 10, y: 99 };
    assert(p != r);                // ...not structural equality
    let head = Node { value: 1, next: Option.Some(Node { value: 2, next: Option.None }) };
    let second = unwrap(head.next);
    assert(second.value == 2);
    second.next = Option.Some(Node { value: 3, next: Option.None });
    assert(unwrap(unwrap(head.next).next).value == 3);
    println("ok");
}
"#,
    );
}

#[test]
fn control_flow() {
    check_ok(
        r#"
fn main() {
    let total = 0;
    for (let i = 0; i < 10; i = i + 1) {
        if (i == 2) { continue; }
        if (i == 5) { break; }
        total = total + i;
    }
    assert(total == 0 + 1 + 3 + 4);
    let n = 0;
    for (;;) { n = n + 1; if (n == 7) { break; } }
    assert(n == 7);
    let m = 0;
    for (; m < 3;) { m = m + 1; }
    assert(m == 3);
    let count = 0;
    for (let i = 0; i < 3; i = i + 1) {
        for (let j = 0; j < 10; j = j + 1) {
            if (j == 2) { break; }     // breaks the inner loop only
            count = count + 1;
        }
    }
    assert(count == 6);
    let k = 99;
    for (k = 0; k < 3; k = k + 1) { }
    assert(k == 3);
    let w = 10;
    while (w > 0) { w = w - 2; }
    assert(w == 0);
    while (false) { assert(false); }
    let grade = 0;
    if (w == 1) { grade = 1; } else if (w == 0) { grade = 2; } else { grade = 3; }
    assert(grade == 2);
    { let scoped = 1; assert(scoped == 1); }
    println("ok");
}
"#,
    );
}

#[test]
fn functions() {
    check_ok(
        r#"
fn add(a: int, b: int): int { return a + b; }
fn fib(n: int): int { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); }
fn is_even(n: int): bool { if (n == 0) { return true; } return is_odd(n - 1); }
fn is_odd(n: int): bool { if (n == 0) { return false; } return is_even(n - 1); }
// Ten integer arguments overflow the register file and spill to the stack.
fn ten(a: int, b: int, c: int, d: int, e: int, f: int, g: int, h: int, i: int, j: int): int {
    return a + b + c + d + e + f + g + h + i + j;
}
// Ten reference arguments + an allocation: a safepoint while stack-passed
// references are live, exercising outgoing-args frame accounting.
fn glue(a: string, b: string, c: string, d: string, e: string,
        f: string, g: string, h: string, i: string, j: string): string {
    let tag = itos(len(a));
    return a + b + c + d + e + f + g + h + i + j + tag;
}
// Ten float arguments overflow the FP register file (8 regs on both x86-64
// and ARM64) and spill to the stack — a separate path from integer args.
fn tenf(a: float, b: float, c: float, d: float, e: float,
        f: float, g: float, h: float, i: float, j: float): float {
    return a + b + c + d + e + f + g + h + i + j;
}
// Nine ints + nine floats + refs overflow both register files at once; the
// allocation inside forces a safepoint while spilled args (including
// references) are live on the stack.
fn mixed(s1: string, a: int, x: float, b: int, y: float, c: int, z: float,
         s2: string, d: int, w: float, e: int, v: float, f: int, u: float,
         g: int, t: float, h: int, r: float, i: int, s3: string): string {
    let n = a + b + c + d + e + f + g + h + i;
    let fl = x + y + z + w + v + u + t + r;
    return s1 + s2 + s3 + itos(n) + ftos(fl);
}
fn nothing() { return; }
fn main() {
    assert(add(2, 40) == 42);
    assert(fib(15) == 610);
    assert(is_even(10) && is_odd(7));
    assert(ten(1, 2, 3, 4, 5, 6, 7, 8, 9, 10) == 55);
    assert(glue("a", "b", "c", "d", "e", "f", "g", "h", "i", "j") == "abcdefghij1");
    let indirect = glue;   // thunk: same call through a function value
    assert(indirect("a", "b", "c", "d", "e", "f", "g", "h", "i", "j") == "abcdefghij1");
    assert(tenf(0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0) == 27.5);
    let m = mixed("x", 1, 0.5, 2, 0.5, 3, 0.5, "y", 4, 0.5, 5, 0.5, 6, 0.5,
                  7, 0.5, 8, 0.5, 9, "z");
    assert(m == "xyz454.0");
    let im = mixed;        // same call through a function value
    assert(im("x", 1, 0.5, 2, 0.5, 3, 0.5, "y", 4, 0.5, 5, 0.5, 6, 0.5,
              7, 0.5, 8, 0.5, 9, "z") == m);
    nothing();
    println("ok");
}
"#,
    );
}

#[test]
fn closures_and_function_values() {
    check_ok(
        r#"
struct Handler { f: fn(int): int, tag: int }
fn double(x: int): int { return x * 2; }
fn apply(f: fn(int): int, v: int): int { return f(v); }
fn make_adder(n: int): fn(int): int { return fn(x: int): int { return x + n; }; }
fn compose(f: fn(int): int, g: fn(int): int): fn(int): int {
    return fn(x: int): int { return f(g(x)); };
}
fn main() {
    let d = double;
    assert(d(21) == 42);
    let d2 = d;
    assert(d == d2);                          // identity of the same value
    assert(double == double);                 // named functions are canonical...
    assert(d == double);
    let h2 = make_adder(5);
    assert(h2 != make_adder(5));              // ...but each closure is distinct
    assert(apply(double, 5) == 10);
    assert(apply(d, 5) == 10);
    let add5 = make_adder(5);
    assert(add5(1) == 6);
    assert(make_adder(7)(1) == 8);            // chained call
    assert((fn(x: int): int { return x + 1; })(41) == 42);   // immediately invoked
    let h = Handler { f: add5, tag: 1 };
    assert(h.f(10) == 15);                    // closure stored in a struct field
    let both = compose(double, add5);
    assert(both(3) == 16);                    // (3 + 5) * 2
    // capture is by value: later assignment is invisible to the closure
    let n = 1;
    let f = fn(): int { return n; };
    n = 2;
    assert(f() == 1);
    // but a captured reference sees mutation of the referenced object
    let xs = [1];
    let g = fn(): int { return xs[0]; };
    xs[0] = 9;
    assert(g() == 9);
    // captures across two lambda levels
    let mk = fn(a: int): fn(int): fn(int): int {
        return fn(b: int): fn(int): int {
            return fn(c: int): int { return a * 100 + b * 10 + c; };
        };
    };
    assert(mk(1)(2)(3) == 123);
    // array of closures, each with its own environment
    let fs: [fn(int): int] = [];
    for (let i = 0; i < 4; i = i + 1) {
        let k = i;
        push(fs, fn(x: int): int { return x + k; });
    }
    assert(fs[0](0) + fs[1](0) + fs[2](0) + fs[3](0) == 6);
    println("ok");
}
"#,
    );
}

#[test]
fn shadowing_and_scopes() {
    check_ok(
        r#"
fn main() {
    let x = 1;
    {
        let x = "two";
        assert(x == "two");
        {
            let x = 3.0;
            assert(x == 3.0);
        }
        assert(x == "two");
    }
    assert(x == 1);
    for (let x = 9; x < 10; x = x + 1) { assert(x == 9); }
    assert(x == 1);
    println("ok");
}
"#,
    );
}

#[test]
fn comments_and_whitespace() {
    check_ok(
        r#"
// line comment
/* block comment
   over lines */
/* nested /* block */ comment */
fn main() { // trailing
    let x /* inline */ = 1;
    assert(x == 1); // done
    println("ok");
}
"#,
    );
}

#[test]
fn print_formatting() {
    check_output(
        r#"
fn main() {
    println(0);
    println(-5);
    println(9223372036854775807);
    println(1.0);
    println(2.5);
    println(1.0 / 0.0);
    println(true);
    println(false);
    println("");
    println("line1\nline2");
    print("a");
    print(1);
    print(true);
    println("!");
}
"#,
        "0\n-5\n9223372036854775807\n1.0\n2.5\ninf\ntrue\nfalse\n\nline1\nline2\na1true!\n",
    );
}

#[test]
fn float_formatting_edge_cases() {
    // Pins format_float: integer-valued finite floats below 1e15 get a ".0"
    // suffix; everything else uses Rust's shortest round-trip form (which
    // never switches to scientific notation).
    check_output(
        r#"
fn main() {
    println(0.0 / 0.0);                 // NaN
    println(0.0 - 1.0 / 0.0);           // -inf
    println(-0.0);
    println(0.1);
    println(0.1 + 0.2);
    println(1.0 / 3.0);
    println(999999999999999.0);         // last value with the ".0" suffix
    println(1000000000000000.0);        // 1e15: prints like an integer
    println(0.000000015);
    println(ftos(0.0 / 0.0));           // ftos agrees with println
    println(ftos(-0.0));
}
"#,
        "NaN\n-inf\n-0.0\n0.1\n0.30000000000000004\n0.3333333333333333\n\
         999999999999999.0\n1000000000000000\n0.000000015\nNaN\n-0.0\n",
    );
}

#[test]
fn gc_object_graphs() {
    check_ok(
        r#"
// A struct with interleaved scalar and reference fields: its descriptor
// bitmap must be exactly right or the GC corrupts it.
struct Mix { a: int, s: string, f: float, next: Option<Mix>, ok: bool, xs: [int] }
fn build(n: int): Mix {
    let head: Option<Mix> = Option.None;
    for (let i = 0; i < n - 1; i = i + 1) {
        head = Option.Some(Mix {
            a: i, s: itos(i), f: itof(i) + 0.5,
            next: head, ok: i % 2 == 0, xs: [i, i + 1],
        });
    }
    let n1 = n - 1;
    Mix { a: n1, s: itos(n1), f: itof(n1) + 0.5, next: head, ok: n1 % 2 == 0, xs: [n1, n1 + 1] }
}
fn verify(m: Mix, n: int) {
    let i = n - 1;
    let cur = Option.Some(m);
    let go = true;
    while go {
        match cur {
            Option.Some(x) => {
                assert(x.a == i);
                assert(x.s == itos(i));
                assert(x.f == itof(i) + 0.5);
                assert(x.ok == (i % 2 == 0));
                assert(x.xs[0] == i && x.xs[1] == i + 1);
                cur = x.next;
                i = i - 1;
            }
            Option.None => { go = false; }
        }
    }
    assert(i == -1);
}
fn main() {
    let keep = build(300);
    for (let round = 0; round < 10; round = round + 1) {
        let junk = build(50);
        assert(junk.a == 49);
        gc_collect();
        verify(keep, 300);
    }
    // Old-to-young pointer stores exercise the write barrier.
    for (let i = 0; i < 200; i = i + 1) {
        let old = unwrap(keep.next);
        keep.next = Option.Some(Mix {
            a: old.a, s: old.s, f: old.f, next: old.next, ok: old.ok, xs: old.xs,
        });
    }
    verify(keep, 300);
    // A buffer far larger than the nursery takes the pretenuring path.
    let big: [int] = [];
    for (let i = 0; i < 50000; i = i + 1) { push(big, i); }
    assert(len(big) == 50000 && big[49999] == 49999);
    gc_collect();
    assert(big[12345] == 12345);
    println("ok");
}
"#,
    );
}

#[test]
fn gc_during_deep_recursion() {
    // Allocates at every recursion depth, so collections run with thousands
    // of compiled frames on the stack — a stress test for the stack walker.
    check_ok(
        r#"
struct Node { value: int, next: Option<Node> }
fn build(n: int): Option<Node> {
    let head: Option<Node> = Option.None;
    for (let i = 1; i <= n; i = i + 1) { head = Option.Some(Node { value: i, next: head }); }
    return head;
}
fn sum_alloc(n: Option<Node>): int {
    match n {
        Option.Some(node) => {
            assert(len(itos(node.value)) > 0);   // allocate at every depth
            return node.value + sum_alloc(node.next);
        }
        Option.None => { return 0; }
    }
}
fn main() {
    let list = build(8000);
    assert(sum_alloc(list) == 8000 * 8001 / 2);
    println("ok");
}
"#,
    );
}

#[test]
fn string_data_survives_relocation() {
    check_ok(
        r#"
fn main() {
    let words: [string] = [];
    for (let i = 0; i < 2000; i = i + 1) {
        push(words, itos(i) + "-" + itos(i * 2));
    }
    gc_collect();
    for (let i = 0; i < 2000; i = i + 1) {
        assert(words[i] == itos(i) + "-" + itos(i * 2));
    }
    println("ok");
}
"#,
    );
}

#[test]
fn nursery_env_var_edge_values() {
    // CROW_NURSERY_KB=0 clamps to the 16 KiB minimum; unparseable values fall
    // back to the default. Either way programs must run correctly, including
    // under the tiniest allowed nursery.
    let src = r#"
struct Node { value: int, next: Option<Node> }
fn main() {
    let head: Option<Node> = Option.None;
    for (let i = 0; i < 500; i = i + 1) {
        head = Option.Some(Node { value: i, next: head });
        assert(len(itos(i)) > 0);          // extra churn
    }
    let sum = 0;
    let go = true;
    while go {
        match head {
            Option.Some(n) => { sum = sum + n.value; head = n.next; }
            Option.None => { go = false; }
        }
    }
    assert(sum == 500 * 499 / 2);
    println("ok");
}
"#;
    for kb in ["0", "16", "not-a-number", ""] {
        let out = run_program(src, &[("CROW_NURSERY_KB", kb)]);
        assert_eq!(out.code, 0, "CROW_NURSERY_KB={kb}: stderr: {}", out.stderr);
        assert_eq!(out.stdout, "ok\n", "CROW_NURSERY_KB={kb}");
    }
}

// -- Runtime panics ---------------------------------------------------------

#[test]
fn panic_bounds_negative_index() {
    expect_panic(
        "fn main() { let xs = [1]; let i = 0 - 5; println(xs[i]); }",
        "index -5 out of bounds",
    );
}

#[test]
fn panic_bounds_at_len() {
    expect_panic("fn main() { let xs = [1, 2]; println(xs[2]); }", "index 2 out of bounds (len 2)");
}

#[test]
fn panic_rem_by_zero() {
    expect_panic("fn main() { let z = 0; println(1 % z); }", "division by zero");
}

#[test]
fn panic_assert_reports_line() {
    expect_panic("fn main() {\n    let x = 1;\n    assert(x == 2);\n}", "assertion failed at line 3");
}

#[test]
fn panic_ftoi_nan() {
    // ftoi has the same checked semantics as `expr as int`.
    expect_panic("fn main() { let nan = 0.0 / 0.0; println(ftoi(nan)); }", "cast out of range");
}

#[test]
fn panic_ftoi_too_big() {
    // 2^64, well past i64::MAX.
    expect_panic(
        "fn main() { let big = 65536.0 * 65536.0 * 65536.0 * 65536.0; println(ftoi(big)); }",
        "cast out of range",
    );
}

#[test]
fn panic_ftoi_too_small() {
    expect_panic(
        "fn main() { let big = 65536.0 * 65536.0 * 65536.0 * 65536.0; println(ftoi(0.0 - big)); }",
        "cast out of range",
    );
}

// -- Stack guard --------------------------------------------------------------

#[test]
fn panic_stack_overflow_reports_function_line() {
    // Runaway recursion trips the prologue stack check and dies with a clean
    // panic naming the recursing function's line — not a SIGSEGV.
    expect_panic(
        "fn down(n: int): int {\n    if (n == 0) { return 0; }\n    return 1 + down(n - 1);\n}\nfn main() { println(down(50000000)); }",
        "stack overflow at line 1",
    );
}

#[test]
fn panic_stack_overflow_through_function_values() {
    // Leaf functions skip the check, so recursion must still be caught when
    // it flows through mutual calls and indirect function-value calls.
    expect_panic(
        r#"
fn ping(n: int): int { return pong(n + 1); }
fn pong(n: int): int { let f = ping; return f(n + 1); }
fn main() { println(ping(0)); }
"#,
        "stack overflow",
    );
}

#[test]
fn panic_stack_overflow_while_allocating() {
    // Allocation at every recursion depth: collections (constant, under the
    // dual-run tiny nursery) must fit in the guard's slack even when the
    // check is about to fire, and the panic itself must still print cleanly.
    expect_panic(
        r#"
fn churn(n: int): int {
    let s = itos(n) + "-x";
    return len(s) + churn(n + 1);
}
fn main() { println(churn(0)); }
"#,
        "stack overflow",
    );
}

#[test]
fn stack_limit_env_knob() {
    // CROW_STACK_KB caps the usable stack: recursion that fits comfortably
    // in the default stack panics under a 256 KiB budget.
    let src = r#"
fn down(n: int): int { if (n == 0) { return 0; } return 1 + down(n - 1); }
fn main() { println(down(20000)); }
"#;
    let out = run_program(src, &[]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "20000\n");
    let out = run_program(src, &[("CROW_STACK_KB", "256")]);
    assert_eq!(out.code, 101, "stdout: {}\nstderr: {}", out.stdout, out.stderr);
    assert!(out.stderr.contains("stack overflow"), "stderr: {}", out.stderr);
}

#[test]
fn panic_preserves_prior_stdout() {
    // Output printed before a panic must be flushed on the panic path,
    // including a trailing `print` with no newline.
    let out = run_program(
        r#"fn main() { println("before"); print("partial"); let z = 0; println(1 / z); }"#,
        &[],
    );
    assert_eq!(out.code, 101, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "before\npartial");
    assert!(out.stderr.contains("division by zero"), "stderr: {}", out.stderr);
}

// -- Regression tests from the code audit -----------------------------------

#[test]
fn lambda_inside_bare_block() {
    // The lambda collector used to skip bare-block statements, leaving the
    // lambda undefined and crashing the compiler.
    check_ok(
        r#"
fn main() {
    {
        let f = fn(): int { return 7; };
        assert(f() == 7);
        {
            let g = fn(x: int): int { return x + f(); };
            assert(g(1) == 8);
        }
    }
    println("ok");
}
"#,
    );
}

#[test]
fn pretenured_allocation_crossing_major_threshold() {
    // A pretenured (larger-than-nursery/4) allocation used to be registered
    // for sweeping before its header was written; if it pushed the old
    // generation past the major-GC threshold, the immediate collection swept
    // a descriptor-less object and crashed.
    check_ok(
        r#"
fn main() {
    let big: [int] = [];
    for (let i = 0; i < 1100000; i = i + 1) { push(big, i); }
    assert(len(big) == 1100000);
    assert(big[0] == 0 && big[549999] == 549999 && big[1099999] == 1099999);
    gc_collect();
    assert(big[777777] == 777777);
    println("ok");
}
"#,
    );
}

// -- Sized integer types ------------------------------------------------------

#[test]
fn sized_integer_basics() {
    check_ok(
        r#"
fn main() {
    let a: u8 = 255;
    let b: i8 = -128;
    let c: u16 = 65535;
    let d: i16 = -32768;
    let e: u32 = 4294967295;
    let f: i32 = -2147483648;
    let g: u64 = 18446744073709551615;
    let h: i64 = 9223372036854775807;
    let i: int = h;                     // i64 is an alias of int
    assert(i == 9223372036854775807);
    assert(a > 0 && c > 0 && e > 0 && g > 0);
    // Unsigned comparison and division are unsigned: g has the top bit set.
    assert(g > 9223372036854775807);
    assert(g / 2 == 9223372036854775807);
    assert(g % 10 == 5);
    let x: u8 = 200;
    let y: u8 = 55;
    assert(x + y == 255);
    assert(x - y == 145);
    assert(y * 4 == 220);
    assert(x / y == 3 && x % y == 35);
    assert(b + 1 == -127 && d + 1 == -32767 && f + 1 == -2147483647);
    assert(b < 0 && b < b + 1);
    println("ok");
}
"#,
    );
}

#[test]
fn sized_integer_casts() {
    check_ok(
        r#"
fn main() {
    // Widening never checks; identity casts are free.
    let a: i8 = -128;
    assert(a as int == -128);
    assert(a as i16 + 0 == -128);
    let b: u8 = 200;
    assert(b as u32 == 200 && b as i16 == 200 && b as int == 200);
    // Narrowing succeeds when the value fits.
    let big = 200;
    assert(big as u8 == b);
    let neg = -100;
    assert(neg as i8 == -100);
    // Signed <-> unsigned round trip.
    let g: u64 = 9223372036854775807;
    assert(g as int == 9223372036854775807);
    assert((0 - 1) as int as i8 == -1);
    // int <-> float.
    assert(3 as float == 3.0);
    assert(2.9 as int == 2);
    assert(-2.9 as int == -2);
    assert((200 as u8) as float == 200.0);
    assert(255.9 as u8 == 255);
    // `as` binds tighter than arithmetic, looser than unary.
    let n = 100;
    assert(n as u8 + 1 == 101);
    assert(-n as i8 == -100);
    println("ok");
}
"#,
    );
}

#[test]
fn packed_structs_and_gc() {
    check_ok(
        r#"
// Mixed sized fields around references: field offsets and the descriptor
// refmap must agree exactly or the GC corrupts the object.
struct Mix { a: u8, s: string, b: i16, f: float, c: u32, next: Option<Mix>, d: i8, e: u64 }
fn build(n: int): Option<Mix> {
    let head: Option<Mix> = Option.None;
    for (let i = 0; i < n; i = i + 1) {
        head = Option.Some(Mix {
            a: (i % 256) as u8, s: itos(i), b: (0 - i) as i16, f: itof(i) + 0.5,
            c: i as u32, next: head, d: (i % 100) as i8, e: 18446744073709551615,
        });
    }
    return head;
}
fn main() {
    let keep = build(300);
    for (let round = 0; round < 5; round = round + 1) {
        let junk = build(50);
        assert(unwrap(junk).a == 49 as u8);
        gc_collect();
    }
    let i = 299;
    let cur = keep;
    let go = true;
    while go {
        match cur {
            Option.Some(x) => {
                assert(x.a as int == i % 256);
                assert(x.s == itos(i));
                assert(x.b as int == 0 - i);
                assert(x.f == itof(i) + 0.5);
                assert(x.c as int == i);
                assert(x.d as int == i % 100);
                assert(x.e == 18446744073709551615);
                cur = x.next;
                i = i - 1;
            }
            Option.None => { go = false; }
        }
    }
    assert(i == -1);
    println("ok");
}
"#,
    );
}

#[test]
fn packed_arrays() {
    check_ok(
        r#"
fn main() {
    // Byte arrays regrow across many pushes and keep their values.
    let bytes: [u8] = [];
    for (let i = 0; i < 1000; i = i + 1) { push(bytes, (i % 256) as u8); }
    assert(len(bytes) == 1000);
    for (let i = 0; i < 1000; i = i + 1) { assert(bytes[i] as int == i % 256); }
    assert(pop(bytes) as int == 999 % 256);
    bytes[0] = 255;
    assert(bytes[0] == 255);
    // Signed narrow elements sign-extend on load and pop.
    let small: [i8] = [-128, -1, 127];
    assert(small[0] == -128 && small[1] == -1 && small[2] == 127);
    push(small, -5);
    assert(pop(small) == -5);
    small[1] = -100;
    assert(small[1] == -100);
    let shorts: [i16] = [-32768, 32767];
    assert(shorts[0] == -32768 && shorts[1] == 32767);
    let words: [u32] = [4294967295, 0];
    assert(words[0] == 4294967295 && words[1] == 0);
    let wide: [u64] = [18446744073709551615];
    assert(wide[0] == 18446744073709551615);
    // Bounds checks still hold for packed elements.
    let grid: [[u8]] = [[1, 2], [3, 4]];
    grid[1][0] = 9;
    assert(grid[1][0] == 9 && grid[0][1] == 2);
    println("ok");
}
"#,
    );
}

#[test]
fn gc_old_to_young_packed_struct_writes() {
    check_ok(
        r#"
// An old-generation packed struct receiving young references: the write
// barrier must record edges through packed field offsets, and each minor
// collection must rewrite exactly those slots (and nothing around them).
struct Packed { tag: u8, s: string, n: i16, next: Option<Packed>, id: u32 }
fn main() {
    let old = Packed { tag: 7, s: "anchor", n: -300, next: Option.None, id: 123456789 };
    gc_collect();                        // promotes `old` out of the nursery
    for (let i = 0; i < 100; i = i + 1) {
        old.s = itos(i) + "-young";      // old -> young edges via packed offsets
        old.next = Option.Some(Packed {
            tag: (i % 256) as u8, s: itos(i), n: (0 - i) as i16,
            next: Option.None, id: i as u32,
        });
        gc_collect();                    // forwards the remembered slots
        assert(old.tag == 7 && old.n == -300 && old.id == 123456789);
        assert(old.s == itos(i) + "-young");
        let young = unwrap(old.next);
        assert(young.tag as int == i % 256);
        assert(young.s == itos(i));
        assert(young.n as int == 0 - i);
        assert(young.id as int == i);
    }
    println("ok");
}
"#,
    );
}

#[test]
fn gc_pretenured_byte_buffer() {
    check_ok(
        r#"
// A byte buffer larger than nursery/4 takes the pretenuring path directly
// into the old generation; its size accounting is in bytes, not words.
fn main() {
    let big: [u8] = [];
    for (let i = 0; i < 300000; i = i + 1) { push(big, (i % 251) as u8); }
    assert(len(big) == 300000);
    gc_collect();
    for (let i = 0; i < 300000; i = i + 1) { assert(big[i] as int == i % 251); }
    println("ok");
}
"#,
    );
}

#[test]
fn sized_integer_printing() {
    check_output(
        r#"
fn main() {
    let a: u64 = 18446744073709551615;
    println(a);
    let b: i8 = -128;
    println(b);
    let c: u8 = 255;
    println(c);
    println(itos(a));
    println(itos(b));
}
"#,
        "18446744073709551615\n-128\n255\n18446744073709551615\n-128\n",
    );
}

#[test]
fn panic_overflow_add() {
    expect_panic(
        "fn main() {\n    let max = 9223372036854775807;\n    println(max + 1);\n}",
        "integer overflow at line 3",
    );
}

#[test]
fn panic_overflow_u8() {
    expect_panic(
        "fn main() { let x: u8 = 255; let y: u8 = 1; println(x + y); }",
        "integer overflow",
    );
}

#[test]
fn panic_overflow_unsigned_sub() {
    expect_panic(
        "fn main() { let x: u32 = 0; let y: u32 = 1; println(x - y); }",
        "integer overflow",
    );
}

#[test]
fn panic_overflow_mul() {
    expect_panic(
        "fn main() { let x = 4294967296; println(x * x); }",
        "integer overflow",
    );
}

#[test]
fn panic_overflow_neg_min() {
    expect_panic(
        "fn main() { let m: i8 = -128; println(-m); }",
        "integer overflow",
    );
}

#[test]
fn panic_overflow_min_div_neg1() {
    expect_panic(
        "fn main() { let min = -9223372036854775808; let d = -1; println(min / d); }",
        "integer overflow",
    );
}

#[test]
fn panic_cast_out_of_range() {
    expect_panic(
        "fn main() {\n    let x = 300;\n    println(x as u8);\n}",
        "cast out of range at line 3",
    );
}

#[test]
fn panic_cast_negative_to_unsigned() {
    expect_panic("fn main() { let x = -1; println(x as u64); }", "cast out of range");
}

#[test]
fn panic_cast_float_nan() {
    expect_panic(
        "fn main() { let nan = 0.0 / 0.0; println(nan as int); }",
        "cast out of range",
    );
}

#[test]
fn panic_cast_float_too_big() {
    expect_panic("fn main() { let f = 1000.0; println(f as i8); }", "cast out of range");
}

#[test]
fn sized_integer_compile_errors() {
    expect_compile_error("fn main() { let x: u8 = 256; }", "out of range for u8");
    expect_compile_error("fn main() { let x: i8 = -129; }", "out of range for i8");
    expect_compile_error("fn main() { let x = 18446744073709551615; }", "out of range for int");
    expect_compile_error(
        "fn main() { let a: u8 = 1; let b: u16 = 2; let c = a + b; }",
        "arithmetic on mixed types u8 and u16",
    );
    expect_compile_error(
        "fn main() { let a: i32 = 1; let b: int = 2; let c = a + b; }",
        "arithmetic on mixed types i32 and int",
    );
    expect_compile_error(
        "fn main() { let a: u32 = 1; let b: i32 = 2; if (a < b) { } }",
        "comparison needs two ints or two floats",
    );
    expect_compile_error("fn main() { let a: u8 = 1; let b = -a; }", "needs a signed int");
    expect_compile_error("fn main() { let s = \"x\" as int; }", "cannot cast string to int");
    expect_compile_error("fn main() { let x = 1 as string; }", "cast target must be a numeric type");
    expect_compile_error("fn main() { let x = 300 as u8; }", "out of range for u8");
    expect_compile_error(
        "fn main() { let xs = [1]; let i: u8 = 0; println(xs[i]); }",
        "array index must be int",
    );
}

#[test]
fn named_function_value_identity() {
    check_ok(
        r#"
fn f(): int { return 1; }
fn g(): int { return 2; }
struct Holder { cb: fn(): int }
fn main() {
    assert(f == f);
    let a = f;
    let b = f;
    assert(a == b && a == f && b != g);
    let h = Holder { cb: f };
    assert(h.cb == f);            // stored callback compares against the name
    assert(h.cb() == 1);
    println("ok");
}
"#,
    );
}

// -- Generics ----------------------------------------------------------------

#[test]
fn generic_functions_across_shapes() {
    check_ok(
        r#"
struct Point { x: int, y: int }
fn id<T>(x: T): T { return x; }
fn first<T>(a: T, b: T): T { return b; return a; }
fn main() {
    assert(id(42) == 42);
    let a: i8 = -5;
    assert(id(a) == a);
    let b: u8 = 200;
    assert(id(b) == b);
    let c: i16 = -30000;
    assert(id(c) == c);
    let d: u32 = 4000000000;
    assert(id(d) == d);
    let e: u64 = 18446744073709551615;
    assert(id(e) == e);
    assert(id(1.5) == 1.5);
    assert(id(true) && !id(false));
    assert(id("hello") == "hello");
    let p = Point { x: 1, y: 2 };
    assert(id(p) == p);              // reference identity survives the call
    let xs = [1, 2, 3];
    assert(id(xs) == xs);
    let f = fn(x: int): int { return x + 1; };
    assert(id(f) == f && id(f)(1) == 2);
    assert(first(1, 2) == 2);
    assert(first("a", "b") == "b");
    println("ok");
}
"#,
    );
}

#[test]
fn generic_structs() {
    check_ok(
        r#"
struct Pair<T> { a: T, b: T }
fn swap<T>(p: Pair<T>) { let t = p.a; p.a = p.b; p.b = t; }
fn pair_of<T>(x: T): Pair<T> { return Pair { a: x, b: x }; }
fn main() {
    let p = Pair { a: 1, b: 2 };
    swap(p);
    assert(p.a == 2 && p.b == 1);
    let q = Pair { a: "x", b: "y" };
    swap(q);
    assert(q.a == "y" && q.b == "x");
    let r = Pair { a: 1.5, b: 2.5 };
    swap(r);
    assert(r.a == 2.5 && r.b == 1.5);
    // Nested instantiations: a pair of pairs, a pair of arrays.
    let n = Pair { a: pair_of(1), b: pair_of(2) };
    assert(n.a.a == 1 && n.b.b == 2);
    let s: Pair<[string]> = Pair { a: ["u"], b: [] };
    push(s.b, "v");
    assert(s.a[0] == "u" && s.b[0] == "v");
    let z = pair_of(9);
    assert(z.a == 9);
    // Annotation seeds inference of the literal's arguments.
    let lit: Pair<u8> = Pair { a: 1, b: 255 };
    assert(lit.b == 255);
    println("ok");
}
"#,
    );
}

// Different scalar shapes give a generic struct different layouts: with
// T = i8 the two bytes pack before the string field, with T = string every
// word is a reference. The GC descriptor per instantiation must match or
// collection corrupts the heap (the harness reruns under a 64 KiB nursery).
#[test]
fn generic_struct_layouts_and_gc() {
    check_ok(
        r#"
struct Mix<T> { t: T, s: string, u: T, n: int }
struct Node<T> { v: T, next: Option<Node<T>> }
fn build<T>(v: T, n: int): Option<Node<T>> {
    let head: Option<Node<T>> = Option.None;
    for (let i = 0; i < n; i = i + 1) {
        head = Option.Some(Node { v: v, next: head });
    }
    return head;
}
fn count<T>(head: Option<Node<T>>): int {
    let n = 0;
    let cur = head;
    let go = true;
    while go {
        match cur {
            Option.Some(node) => { n = n + 1; cur = node.next; }
            Option.None => { go = false; }
        }
    }
    return n;
}
fn main() {
    let a: Mix<i8> = Mix { t: -3, s: "packed", u: 7, n: 99 };
    let b: Mix<string> = Mix { t: "refs", s: "everywhere", u: "now", n: 42 };
    let c: Mix<float> = Mix { t: 0.5, s: "floats", u: 2.5, n: 7 };
    let keep = build("s", 200);
    let nums = build(11, 200);
    for (let round = 0; round < 10; round = round + 1) {
        build(0.5, 50);                // garbage
        gc_collect();
    }
    assert(a.t == -3 && a.s == "packed" && a.u == 7 && a.n == 99);
    assert(b.t == "refs" && b.s == "everywhere" && b.u == "now" && b.n == 42);
    assert(c.t == 0.5 && c.s == "floats" && c.u == 2.5 && c.n == 7);
    assert(count(keep) == 200 && count(nums) == 200);
    let cur = keep;
    let go = true;
    while go {
        match cur {
            Option.Some(node) => { assert(node.v == "s"); cur = node.next; }
            Option.None => { go = false; }
        }
    }
    println("ok");
}
"#,
    );
}

#[test]
fn generic_arrays() {
    check_ok(
        r#"
fn rev<T>(xs: [T]): [T] {
    let out: [T] = [];
    for (let i = len(xs) - 1; i >= 0; i = i - 1) { push(out, xs[i]); }
    return out;
}
fn take_last<T>(xs: [T]): T { return pop(xs); }
fn fill<T>(xs: [T], v: T, n: int) {
    for (let i = 0; i < n; i = i + 1) { push(xs, v); }
}
fn main() {
    let r = rev([1, 2, 3]);
    assert(len(r) == 3 && r[0] == 3 && r[1] == 2 && r[2] == 1);
    let s = rev(["a", "b", "c"]);
    assert(s[0] == "c" && s[2] == "a");
    let bs = rev([true, false]);
    assert(!bs[0] && bs[1]);                // [bool] stays a byte buffer
    let small: [i8] = [1, -2, 3];
    let sr = rev(small);
    assert(sr[0] == 3 && sr[1] == -2);      // packed elements keep their sign
    let fs = rev([1.5, 2.5]);
    assert(fs[0] == 2.5 && fs[1] == 1.5);
    assert(take_last([1, 2, 3]) == 3);
    assert(take_last(["x", "y"]) == "y");
    let grown: [string] = [];
    fill(grown, "g", 100);                  // regrowth through generic code
    gc_collect();
    assert(len(grown) == 100 && grown[99] == "g");
    let empty: [string] = rev([]);          // element type inferred from context
    assert(len(empty) == 0);
    println("ok");
}
"#,
    );
}

#[test]
fn generic_functions_with_function_values() {
    check_ok(
        r#"
struct Point { x: int, y: int }
fn map<T, U>(xs: [T], f: fn(T): U): [U] {
    let out: [U] = [];
    for (let i = 0; i < len(xs); i = i + 1) { push(out, f(xs[i])); }
    return out;
}
fn apply<T>(f: fn(T): T, x: T): T { return f(x); }
fn double(x: int): int { return x * 2; }
fn main() {
    let strs = map([1, 2, 3], fn(x: int): string { return itos(x); });
    assert(strs[0] == "1" && strs[2] == "3");
    let xs = map(["a", "bb", "ccc"], fn(s: string): int { return len(s); });
    assert(xs[0] == 1 && xs[2] == 3);
    let pts = map([1, 2], fn(v: int): Point { return Point { x: v, y: v }; });
    assert(pts[1].y == 2);
    assert(apply(double, 21) == 42);        // named function as a generic arg
    println("ok");
}
"#,
    );
}

// Lambdas inside a generic function mention the enclosing T; each
// instantiation compiles its own copies with the right captures and
// descriptors.
#[test]
fn lambdas_inside_generic_functions() {
    check_ok(
        r#"
fn make_getter<T>(x: T): fn(): T {
    return fn(): T { return x; };
}
fn twice<T>(x: T): [T] {
    let dup = fn(v: T): [T] {
        let out: [T] = [];
        push(out, v);
        push(out, v);
        return out;
    };
    return dup(x);
}
fn main() {
    let gi = make_getter(7);
    let gs = make_getter("s");
    let gf = make_getter(2.5);
    gc_collect();                     // captured T values are GC roots
    assert(gi() == 7 && gs() == "s" && gf() == 2.5);
    let ts = twice("q");
    assert(len(ts) == 2 && ts[0] == "q" && ts[1] == "q");
    let tn = twice(3);
    assert(tn[0] == 3 && tn[1] == 3);
    println("ok");
}
"#,
    );
}

// Polymorphic recursion: the type argument grows without bound
// (T, Pair<T>, Pair<Pair<T>>, ...) but every struct is Ref-shaped, so
// instantiations collapse to at most two compiled bodies and compilation
// terminates.
#[test]
fn polymorphic_recursion() {
    check_ok(
        r#"
struct Pair<T> { a: T, b: T }
fn depth<T>(x: T, n: int): int {
    if (n == 0) { return 0; }
    return 1 + depth(Pair { a: x, b: x }, n - 1);
}
fn main() {
    assert(depth(1, 8) == 8);
    assert(depth("s", 3) == 3);
    assert(depth(0.5, 4) == 4);
    println("ok");
}
"#,
    );
}

#[test]
fn generic_inference_from_context() {
    check_ok(
        r#"
struct Pair<T> { a: T, b: T }
fn empty<T>(): [T] { return []; }
fn none<T>(): Option<T> { return Option.None; }
fn main() {
    let xs: [string] = empty();       // solved from the annotation
    push(xs, "a");
    assert(len(xs) == 1);
    let ys: [int] = empty();
    push(ys, 5);
    assert(ys[0] == 5);
    let p: Option<int> = none();        // T solved from the annotation
    match p { Option.Some(v) => { assert(v != v); } Option.None => { } }
    // The expected type also flows through assignment targets.
    let zs: [[int]] = [];
    push(zs, empty());
    assert(len(zs[0]) == 0);
    println("ok");
}
"#,
    );
}

#[test]
fn generic_compile_errors() {
    expect_compile_error(
        r#"
fn f<T>(x: T): bool { return x == x; }
fn main() { assert(f(1)); }
"#,
        "cannot compare values of generic type T",
    );
    expect_compile_error(
        r#"
fn f<T>(): [T] { return []; }
fn main() { f(); }
"#,
        "cannot infer type parameter 'T'",
    );
    expect_compile_error(
        r#"
fn f<T>(x: T): T { return x; }
fn main() { let g = f; }
"#,
        "can only be called directly",
    );
    expect_compile_error(
        r#"
fn f<T>(x: T) { println(x); }
fn main() { f(1); }
"#,
        "cannot print a value of type T",
    );
    expect_compile_error(
        r#"
fn f<T>(x: T): T { return x + x; }
fn main() { f(1); }
"#,
        "invalid operand type T",
    );
    expect_compile_error(
        r#"
struct Pair<T> { a: T, b: T }
fn main() { let p = Pair { a: 1, b: "s" }; }
"#,
        "type mismatch",
    );
    expect_compile_error(
        r#"
struct Pair<T> { a: T, b: T }
fn main() { let p: Pair<int, int> = Pair { a: 1, b: 2 }; }
"#,
        "expects 1 type argument(s), got 2",
    );
    expect_compile_error(
        r#"
fn main<T>() { }
"#,
        "'main' cannot be generic",
    );
}

#[test]
fn bitwise_operators() {
    check_ok(
        r#"
fn main() {
    assert((6 & 3) == 2);
    assert((6 | 3) == 7);
    assert((6 ^ 3) == 5);
    assert(~0 == -1);
    assert(~5 == -6);
    assert(1 << 10 == 1024);
    assert(1024 >> 10 == 1);
    assert(1 << 63 == -9223372036854775808);   // i64 <<: bit into the sign
    assert(-1 >> 1 == -1);                     // signed >> is arithmetic
    assert(-8 >> 2 == -2);

    // Precedence: bitwise binds tighter than comparison, looser than shift;
    // shift binds looser than +.
    assert(6 & 1 == 0);
    assert(2 | 4 == 6);
    assert(1 | 2 ^ 3 & 4 == 3);        // 1 | (2 ^ (3 & 4))
    assert(1 << 2 + 1 == 8);           // 1 << (2 + 1)
    assert(3 & 1 << 1 == 2);           // 3 & (1 << 1)
    println("ok");
}
"#,
    );
}

#[test]
fn bitwise_sized_integers() {
    check_ok(
        r#"
fn main() {
    // Narrow unsigned: << discards high bits, ~ stays in width, >> is logical.
    let a: u8 = 200;
    assert(a << 1 == 144);             // 400 % 256
    assert(~a == 55);
    assert(a >> 3 == 25);
    let b: u8 = 255;
    assert(b << 7 == 128);
    assert((b ^ 15) == 240);

    // Narrow signed: << re-canonicalizes with sign, >> is arithmetic.
    let c: i8 = 64;
    assert(c << 1 == -128);
    let d: i8 = -1;
    assert(d >> 3 == -1);
    assert(~d == 0);
    let e: i8 = -128;
    assert(e >> 7 == -1);

    // u64 >> is logical even for the top bit.
    let f: u64 = 18446744073709551615;
    assert(f >> 63 == 1);
    assert(~f == 0);

    // Shift amount is checked, bits shifted out are not an overflow.
    let g: u16 = 65535;
    assert(g << 15 == 32768);
    println("ok");
}
"#,
    );
}

#[test]
fn panic_shift_amount_too_big() {
    expect_panic(
        r#"
fn main() {
    let x: u8 = 1;
    let n: u8 = 8;
    println(x << n);
}
"#,
        "invalid shift amount at line 5",
    );
}

#[test]
fn panic_shift_amount_negative() {
    expect_panic(
        r#"
fn main() {
    let x = 1;
    let n = -1;
    println(x >> n);
}
"#,
        "invalid shift amount at line 5",
    );
}

#[test]
fn string_indexing() {
    check_ok(
        r#"
fn main() {
    let s = "Hello";
    assert(s[0] == b'H');
    assert(s[4] == b'o');
    assert(s[1] - b'a' == 4);
    assert(len(s) == 5);

    // Bytes of a multi-byte character (é = 0xC3 0xA9 in UTF-8).
    let e = "é";
    assert(len(e) == 2);
    assert(e[0] == b'\xc3' && e[1] == b'\xa9');

    // Byte escapes.
    assert("\n"[0] == b'\n');
    assert("\0"[0] == b'\0');

    // A hand-rolled parser, now possible in-language.
    let src = "-437";
    let neg = src[0] == b'-';
    let v = 0;
    for (let i = 1; i < len(src); i = i + 1) {
        assert(src[i] >= b'0' && src[i] <= b'9');
        v = v * 10 + (src[i] - b'0') as int;
    }
    if neg { v = -v; }
    assert(v == -437);
    println("ok");
}
"#,
    );
}

#[test]
fn panic_string_index_out_of_bounds() {
    expect_panic(
        r#"
fn main() {
    let s = "abc";
    println(s[3]);
}
"#,
        "index 3 out of bounds (len 3) at line 4",
    );
}

#[test]
fn compound_assignment() {
    check_ok(
        r#"
struct Acc { n: int, s: string }
fn main() {
    let x = 10;
    x += 5; assert(x == 15);
    x -= 3; assert(x == 12);
    x *= 2; assert(x == 24);
    x /= 5; assert(x == 4);
    x %= 3; assert(x == 1);
    x <<= 4; assert(x == 16);
    x >>= 2; assert(x == 4);
    x |= 3; assert(x == 7);
    x &= 5; assert(x == 5);
    x ^= 1; assert(x == 4);

    let f = 1.5;
    f *= 4.0; assert(f == 6.0);

    let s = "a";
    s += "b"; s += "c";
    assert(s == "abc");

    let a = Acc { n: 1, s: "x" };
    a.n += 41; assert(a.n == 42);
    a.s += "y"; assert(a.s == "xy");

    let xs = [1, 2, 3];
    xs[1] += 10; assert(xs[1] == 12);
    xs[2] <<= 3; assert(xs[2] == 24);

    let bs: [u8] = [200];
    bs[0] += 55; assert(bs[0] == 255);
    println("ok");
}
"#,
    );
}

#[test]
fn compound_assignment_evaluates_target_once() {
    check_ok(
        r#"
fn hit(log: [int], i: int): int { push(log, i); return i; }
fn main() {
    // The index and object expressions run once per compound assignment.
    let log: [int] = [];
    let xs = [5, 7];
    xs[hit(log, 0)] += 1;
    assert(len(log) == 1);
    assert(xs[0] == 6);

    let grid = [[10], [20]];
    grid[hit(log, 1)][hit(log, 0)] *= 3;
    assert(len(log) == 3);
    assert(grid[1][0] == 60);
    println("ok");
}
"#,
    );
}

#[test]
fn compound_assignment_under_gc_pressure() {
    // String concat allocates mid-assignment; the element store must land in
    // the (possibly moved) buffer, not a stale address.
    check_ok(
        r#"
fn pad(n: int): string {
    let s = "";
    for (let i = 0; i < n; i = i + 1) { s += "x"; }
    return s;
}
fn main() {
    let ss = ["a", "b"];
    for (let i = 0; i < 200; i = i + 1) {
        ss[0] += "!";
        let junk = pad(i % 37);        // churn the nursery
    }
    assert(len(ss[0]) == 201 && ss[0][0] == b'a' && ss[0][200] == b'!');
    assert(ss[1] == "b");
    println("ok");
}
"#,
    );
}

#[test]
fn panic_compound_assignment_rechecks_bounds() {
    // The right-hand side shrinks the array, so the write is out of bounds
    // even though the read passed.
    expect_panic(
        r#"
fn shrink(xs: [int]): int { let v = pop(xs); return v; }
fn main() {
    let xs = [1, 2, 3];
    xs[2] += shrink(xs);
}
"#,
        "index 2 out of bounds (len 2) at line 5",
    );
}

#[test]
fn overflow_applies_to_compound_assignment() {
    expect_panic(
        r#"
fn main() {
    let x: u8 = 250;
    x += 10;
}
"#,
        "integer overflow at line 4",
    );
}

#[test]
fn if_expressions() {
    check_ok(
        r#"
struct P { v: int }
fn note(log: [int], v: int): int { push(log, v); return v; }
fn main() {
    assert(if true { 1 } else { 2 } == 1);
    assert(if false { 1 } else { 2 } == 2);
    let grade = if 87 >= 90 { "A" } else if 87 >= 80 { "B" } else { "C" };
    assert(grade == "B");

    // Only the taken branch evaluates.
    let log: [int] = [];
    let x = if true { note(log, 1) } else { note(log, 2) };
    assert(x == 1 && len(log) == 1 && log[0] == 1);

    // Reference-typed branches.
    let p = P { v: 7 };
    let q = if p.v == 7 { p } else { P { v: 0 } };
    assert(q == p && q.v == 7);
    let r = if false { Option.Some(P { v: 1 }) } else { Option.None };
    match r { Option.Some(w) => { assert(w != w); } Option.None => { } }

    // Floats and nesting.
    let f = if true { if false { 1.5 } else { 2.5 } } else { 0.0 };
    assert(f == 2.5);
    println("ok");
}
"#,
    );
}

#[test]
fn stoi_stof() {
    check_ok(
        r#"
fn main() {
    assert(stoi("0") == 0);
    assert(stoi("42") == 42);
    assert(stoi("-7") == -7);
    assert(stoi("9223372036854775807") == 9223372036854775807);
    assert(stoi("-9223372036854775808") == -9223372036854775808);
    assert(stoi(itos(123456)) == 123456);

    assert(stof("1.5") == 1.5);
    assert(stof("-0.25") == -0.25);
    assert(stof("3") == 3.0);
    assert(stof("1e3") == 1000.0);
    assert(stof(ftos(2.5)) == 2.5);
    println("ok");
}
"#,
    );
}

#[test]
fn panic_stoi_invalid() {
    expect_panic(
        r#"
fn main() {
    println(stoi("12x"));
}
"#,
        "stoi: invalid integer \"12x\" at line 3",
    );
}

#[test]
fn panic_stoi_rejects_padding_and_plus() {
    expect_panic(r#"fn main() { println(stoi(" 1")); }"#, "invalid integer");
    expect_panic(r#"fn main() { println(stoi("+1")); }"#, "invalid integer");
    expect_panic(r#"fn main() { println(stoi("")); }"#, "invalid integer");
    expect_panic(r#"fn main() { println(stoi("1 ")); }"#, "invalid integer");
}

#[test]
fn panic_stoi_out_of_range() {
    expect_panic(
        r#"
fn main() {
    println(stoi("9223372036854775808"));
}
"#,
        "out of range at line 3",
    );
}

#[test]
fn panic_stof_invalid() {
    expect_panic(r#"fn main() { println(stof("abc")); }"#, "stof: invalid float");
    expect_panic(r#"fn main() { println(stof("inf")); }"#, "stof: invalid float");
    expect_panic(r#"fn main() { println(stof("+1.0")); }"#, "stof: invalid float");
    expect_panic(r#"fn main() { println(stof("1e999")); }"#, "out of range");
}

#[test]
fn stob_btos() {
    check_ok(
        r#"
fn upper(s: string): string {
    let bs = stob(s);
    for (let i = 0; i < len(bs); i = i + 1) {
        if bs[i] >= b'a' && bs[i] <= b'z' { bs[i] -= 32; }
    }
    return btos(bs);
}
fn main() {
    let bs = stob("abc");
    assert(len(bs) == 3 && bs[0] == b'a' && bs[2] == b'c');

    // The array is a copy: mutating it leaves the string alone.
    let s = "hello";
    let cs = stob(s);
    cs[0] = b'H';
    assert(s == "hello" && btos(cs) == "Hello");

    // Arrays built by hand round-trip, and push/pop work on the copy.
    push(cs, b'!');
    assert(btos(cs) == "Hello!");
    assert(btos([104, 105]) == "hi");
    assert(btos(stob("")) == "" && len(stob("")) == 0);

    // Multi-byte characters survive the round trip (é = 2 bytes).
    assert(upper("héllo, crow") == "HéLLO, CROW");

    // Under GC pressure: build many strings from bytes.
    let acc: [string] = [];
    for (let i = 0; i < 100; i = i + 1) {
        let b: [u8] = [];
        push(b, b'a' + (i % 26) as u8);
        push(acc, btos(b));
    }
    assert(len(acc) == 100 && acc[0] == "a" && acc[25] == "z" && acc[26] == "a");
    println("ok");
}
"#,
    );
}

#[test]
fn panic_btos_invalid_utf8() {
    expect_panic(
        r#"
fn main() {
    let bs: [u8] = [104, 255, 105];
    println(btos(bs));
}
"#,
        "btos: invalid UTF-8 at line 4",
    );
}

#[test]
fn tail_expressions() {
    check_ok(
        r#"
fn double(x: int): int { x * 2 }
fn classify(x: int): string { (if x < 0 { "neg" } else if x == 0 { "zero" } else { "pos" }) }
fn fact(n: int): int {
    if n <= 1 { return 1; }
    n * fact(n - 1)
}
fn main() {
    assert(double(21) == 42);
    assert(classify(-3) == "neg" && classify(0) == "zero" && classify(9) == "pos");
    assert(fact(5) == 120);

    // Lambdas: single expression, statements + tail, and captures.
    let inc = fn(x: int): int { x + 1 };
    assert(inc(41) == 42);
    let squares = fn(n: int): [int] {
        let out: [int] = [];
        for (let i = 1; i <= n; i = i + 1) { push(out, i * i); }
        out
    };
    let sq = squares(4);
    assert(sq[3] == 16);
    let base = 100;
    let offset = fn(x: int): int { base + x };
    assert(offset(1) == 101);

    // Unit bodies may omit the final ';' too.
    let hello = fn() { print("") };
    hello();
    println("ok")
}
"#,
    );
}

// A live working set larger than the nursery floor makes the collector grow
// the nursery (visible in the GC log); pinning the size disables adaptation.
#[test]
fn nursery_adapts_to_working_set() {
    let src = r#"
enum Tree { Branch { left: Tree, right: Tree, value: int }, Leaf }
fn build(depth: int, value: int): Tree {
    if depth == 0 {
        return Tree.Branch { left: Tree.Leaf, right: Tree.Leaf, value: value };
    }
    Tree.Branch {
        left: build(depth - 1, value * 2),
        right: build(depth - 1, value * 2 + 1),
        value: value,
    }
}
fn sum(t: Tree): int {
    match t {
        Tree.Branch { left, right, value } => { return value + sum(left) + sum(right); }
        Tree.Leaf => { return 0; }
    }
}
fn main() {
    let total = 0;
    for (let i = 0; i < 10; i += 1) { total += sum(build(14, 1)); }
    println(total);
}
"#;
    let out = run_program(src, &[("CROW_GC_LOG", "1")]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("nursery resized to 1024 KiB"),
        "expected the nursery to grow:\n{}",
        out.stderr
    );
    // A pinned nursery never resizes, no matter the workload.
    let out = run_program(src, &[("CROW_GC_LOG", "1"), ("CROW_NURSERY_KB", "64")]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(
        !out.stderr.contains("resized"),
        "pinned nursery must not adapt:\n{}",
        out.stderr
    );
}

// After the big-live-set phase ends, sustained near-zero survival shrinks
// the nursery back down.
#[test]
fn nursery_shrinks_after_allocation_phase() {
    let src = r#"
enum Tree { Branch { left: Tree, right: Tree, value: int }, Leaf }
fn build(depth: int, value: int): Tree {
    if depth == 0 {
        return Tree.Branch { left: Tree.Leaf, right: Tree.Leaf, value: value };
    }
    Tree.Branch {
        left: build(depth - 1, value * 2),
        right: build(depth - 1, value * 2 + 1),
        value: value,
    }
}
fn sum(t: Tree): int {
    match t {
        Tree.Branch { left, right, value } => { return value + sum(left) + sum(right); }
        Tree.Leaf => { return 0; }
    }
}
fn main() {
    let total = 0;
    for (let i = 0; i < 10; i += 1) { total += sum(build(14, 1)); }
    for (let i = 0; i < 8000000; i += 1) { let s = itos(i); }
    println(total);
}
"#;
    let out = run_program(src, &[("CROW_GC_LOG", "1")]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let sizes: Vec<u64> = out
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[crow-gc] nursery resized to "))
        .filter_map(|l| l.strip_suffix(" KiB"))
        .filter_map(|n| n.parse().ok())
        .collect();
    let peak = sizes.iter().copied().max().unwrap_or(0);
    assert!(peak >= 1024, "expected growth first: {sizes:?}");
    assert!(
        *sizes.last().unwrap() < peak,
        "expected a shrink after the garbage phase: {sizes:?}"
    );
}

// ---------------------------------------------------------------------------
// Enums and match
// ---------------------------------------------------------------------------

#[test]
fn enum_basics() {
    check_ok(
        r#"
struct Rect { w: float, h: float }
enum Shape { Circle(float), Box(Rect), Empty }
fn area(s: Shape): float {
    // A statement starting with `match` is the match statement, so a tail
    // match-expression is parenthesized (same rule as `if`).
    (match s {
        Shape.Circle(r) => 3.0 * r * r,
        Shape.Box(b) => b.w * b.h,
        Shape.Empty => 0.0,
    })
}
fn describe(s: Shape): string {
    return match s {
        Shape.Circle(r) => "circle " + ftos(r),
        _ => "other",
    };
}
fn main() {
    assert(area(Shape.Circle(2.0)) == 12.0);
    assert(area(Shape.Box(Rect { w: 2.0, h: 3.0 })) == 6.0);
    assert(area(Shape.Empty) == 0.0);
    assert(describe(Shape.Circle(1.5)) == "circle 1.5");
    assert(describe(Shape.Empty) == "other");

    // Match statement: arms are blocks, comma after a block is optional.
    let hits = 0;
    match Shape.Box(Rect { w: 1.0, h: 1.0 }) {
        Shape.Circle(r) => { assert(false); assert(r == r); }
        Shape.Box(b) => { hits += 1; assert(b.w == 1.0); },
        Shape.Empty => { assert(false); }
    }
    assert(hits == 1);

    // The payload is copied out by value at the binder...
    let s = Shape.Circle(5.0);
    match s {
        Shape.Circle(r) => {
            let r2 = r + 1.0;
            assert(r2 == 6.0);
        }
        _ => { assert(false); }
    }
    // ...and a struct payload still aliases the shared object.
    let rect = Rect { w: 1.0, h: 1.0 };
    match Shape.Box(rect) {
        Shape.Box(b) => { b.w = 9.0; }
        _ => { assert(false); }
    }
    assert(rect.w == 9.0);
    println("ok");
}
"#,
    );
}

#[test]
fn enum_bare_singletons_and_equality() {
    check_ok(
        r#"
enum Color { Red, Green, Blue }
fn pick(n: int): Color {
    return match n { 0 => Color.Red, 1 => Color.Green, _ => Color.Blue };
}
fn main() {
    // Bare variants are shared static singletons, so identity == is
    // structural equality — including across separate constructions.
    assert(Color.Red == Color.Red);
    assert(Color.Red != Color.Green);
    assert(pick(1) == Color.Green);
    let c = pick(2);
    assert(c == Color.Blue && c != Color.Red);

    // They survive being stored, collected, and reloaded.
    let all = [Color.Red, Color.Green, Color.Blue];
    gc_collect();
    assert(all[0] == Color.Red && all[1] == pick(1) && all[2] == c);
    println("ok");
}
"#,
    );
}

#[test]
fn enum_generic_option() {
    check_ok(
        r#"
fn find(xs: [int], want: int): Option<int> {
    for (let i = 0; i < len(xs); i += 1) {
        if xs[i] == want { return Option.Some(i); }
    }
    Option.None
}
fn wrap<T>(x: T): Option<T> { Option.Some(x) }
fn or_else<T>(o: Option<T>, d: T): T {
    return match o { Option.Some(v) => v, Option.None => d };
}
fn main() {
    // Inference: from the payload, from the expected type, from returns.
    let a = Option.Some(42);
    let b: Option<string> = Option.None;
    assert(unwrap(a) == 42);
    assert(or_else(b, "d") == "d");
    assert(unwrap(find([5, 6, 7], 6)) == 1);
    match find([5], 9) {
        Option.Some(i) => { assert(i != i); }
        Option.None => { }
    }

    // Payload shapes: float, narrow ints, strings, structs — and the
    // generic body is shared per shape.
    assert(unwrap(wrap(2.5)) == 2.5);
    let u: u8 = 200;
    assert(unwrap(wrap(u)) == 200);
    assert(unwrap(wrap("hi")) == "hi");

    // Nesting: Some(None) and Some(Some(v)) stay distinct.
    let inner: Option<int> = Option.None;
    let nested = Option.Some(inner);
    match nested {
        Option.Some(o) => {
            match o {
                Option.Some(v) => { assert(v != v); }
                Option.None => { }
            }
        }
        Option.None => { assert(false); }
    }
    assert(unwrap(unwrap(Option.Some(Option.Some(7)))) == 7);
    println("ok");
}
"#,
    );
}

#[test]
fn enum_recursive_and_gc_relocation() {
    check_ok(
        r#"
enum List { Cons(Node), End }
struct Node { head: int, tail: List }
fn range_list(n: int): List {
    let l = List.End;
    for (let i = n; i > 0; i -= 1) {
        l = List.Cons(Node { head: i, tail: l });
    }
    l
}
fn sum(l: List): int {
    let total = 0;
    let cur = l;
    let go = true;
    while go {
        match cur {
            List.Cons(n) => { total += n.head; cur = n.tail; }
            List.End => { go = false; }
        }
    }
    total
}
fn main() {
    // Long chains of enum objects survive evacuation, and matching keeps
    // working on relocated objects.
    let l = range_list(2000);
    gc_collect();
    assert(sum(l) == 2001000);

    // Options holding fresh strings under allocation pressure.
    let kept: [Option<string>] = [];
    for (let i = 0; i < 2000; i += 1) {
        push(kept, Option.Some("v" + itos(i)));
        if i % 3 == 0 { push(kept, Option.None); }
    }
    gc_collect();
    assert(unwrap(kept[0]) == "v0");
    match kept[1] {                        // the None pushed at i == 0
        Option.Some(v) => { assert(v != v); }
        Option.None => { }
    }
    assert(unwrap(kept[2]) == "v1");
    println("ok");
}
"#,
    );
}

#[test]
fn match_on_ints_bools_and_bytes() {
    check_ok(
        r#"
fn name(n: int): string {
    return match n { 0 => "zero", 7 => "seven", -1 => "neg", _ => "other" };
}
fn main() {
    assert(name(0) == "zero" && name(7) == "seven" && name(-1) == "neg");
    assert(name(100) == "other");

    // bool: true/false cover everything; wildcard also allowed.
    let big = match 5 > 3 { true => 1, false => 0 };
    assert(big == 1);
    match false {
        true => { assert(false); }
        _ => { }
    }

    // Byte patterns against string bytes; literals adopt the u8 scrutinee.
    let s = "c0";
    assert((match s[0] { b'a' => 1, b'c' => 2, _ => 3 }) == 2);
    assert((match s[1] { 48 => "digit", _ => "no" }) == "digit");

    // Sized scrutinee: literals range-check at its type.
    let u: u8 = 255;
    assert((match u { 255 => true, _ => false }));

    // Statement form on ints, with side effects and control flow.
    let total = 0;
    for (let i = 0; i < 5; i += 1) {
        match i % 3 {
            0 => { total += 100; }
            1 => { continue; }
            _ => { total += 1; }
        }
    }
    assert(total == 201);   // i=0,3 add 100 each; i=2 adds 1; i=1,4 skip
    println("ok");
}
"#,
    );
}

#[test]
fn match_interactions() {
    check_ok(
        r#"
enum Op { Add(int), Mul(int), Reset }
fn apply(acc: int, op: Op): int {
    return match op {
        Op.Add(n) => acc + n,
        Op.Mul(n) => acc * n,
        Op.Reset => 0,
    };
}
fn main() {
    // Enums in arrays, driven through a fold.
    let ops = [Op.Add(5), Op.Mul(3), Op.Add(1), Op.Reset, Op.Add(2)];
    let acc = 0;
    for (let i = 0; i < len(ops); i += 1) { acc = apply(acc, ops[i]); }
    assert(acc == 2);

    // Match arms returning early from the enclosing function count as
    // terminated paths; nested matches; match in argument position.
    assert(apply(apply(0, Op.Add(4)), Op.Mul(10)) == 40);
    let o = Option.Some(Op.Mul(6));
    let v = match o {
        Option.Some(op) => apply(7, op),
        Option.None => -1,
    };
    assert(v == 42);

    // Binders shadow like ordinary locals and scope to their arm.
    let n = 10;
    match Op.Add(1) {
        Op.Add(n) => { assert(n == 1); }
        _ => { }
    }
    assert(n == 10);

    // Closures capture match binders by value.
    let f = match Op.Mul(3) {
        Op.Mul(m) => fn(x: int): int { x * m },
        _ => fn(x: int): int { x },
    };
    assert(f(7) == 21);
    println("ok");
}
"#,
    );
}

#[test]
fn match_returns_on_all_paths() {
    check_ok(
        r#"
enum Sign { Neg, Zero, Pos }
fn classify(n: int): Sign {
    // A match whose arms all return satisfies the all-paths-return check.
    match n {
        0 => { return Sign.Zero; }
        _ => {
            if n < 0 { return Sign.Neg; }
            return Sign.Pos;
        }
    }
}
fn main() {
    assert(classify(0) == Sign.Zero);
    assert(classify(-5) == Sign.Neg);
    assert(classify(9) == Sign.Pos);
    println("ok");
}
"#,
    );
}

#[test]
fn unwrap_none_panics() {
    expect_panic(
        r#"
fn main() {
    let o: Option<int> = Option.None;
    println(unwrap(o));
}
"#,
        "unwrap of None at line 4",
    );
}

#[test]
fn enum_compile_errors() {
    // Exhaustiveness.
    expect_compile_error(
        "enum E { A, B } fn main() { match E.A { E.A => { } } }",
        "missing variant(s) B",
    );
    expect_compile_error(
        "fn main() { match 1 { 0 => { } } }",
        "needs a final '_' arm",
    );
    expect_compile_error(
        "fn main() { match true { true => { } } }",
        "must cover both 'true' and 'false'",
    );
    expect_compile_error(
        "enum E { A, B } fn main() { match E.A { _ => { } E.A => { } } }",
        "unreachable pattern after '_'",
    );
    expect_compile_error(
        "enum E { A, B } fn main() { match E.A { E.A => { } E.B => { } _ => { } } }",
        "unreachable pattern",
    );
    expect_compile_error(
        "enum E { A, B } fn main() { match E.A { E.A => { } E.A => { } E.B => { } } }",
        "duplicate pattern",
    );
    // Patterns must fit the scrutinee.
    expect_compile_error(
        "enum E { A, B } fn main() { match E.A { 1 => { } _ => { } } }",
        "pattern does not match the scrutinee type E",
    );
    expect_compile_error(
        "enum E { A } enum F { X } fn main() { match E.A { F.X => { } } }",
        "pattern does not match the scrutinee type E",
    );
    expect_compile_error(
        "enum E { A } fn main() { match E.A { E.Y => { } } }",
        "enum 'E' has no variant 'Y'",
    );
    expect_compile_error(
        "fn main() { match \"s\" { _ => { } } }",
        "cannot match on a value of type string",
    );
    expect_compile_error(
        "fn f<T>(x: T) { match x { _ => { } } } fn main() { f(1); }",
        "cannot match on a value of generic type T",
    );
    expect_compile_error(
        "fn main() { let u: u8 = 1; match u { 300 => { } _ => { } } }",
        "out of range for u8",
    );
    // Binder arity.
    expect_compile_error(
        "enum E { A(int) } fn main() { match E.A(1) { E.A => { } } }",
        "wraps a value; bind it",
    );
    expect_compile_error(
        "enum E { A } fn main() { match E.A { E.A(x) => { } } }",
        "bare and has no value to bind",
    );
    // Construction arity.
    expect_compile_error(
        "enum E { A } fn main() { let e = E.A(); }",
        "variant 'A' is bare",
    );
    expect_compile_error(
        "enum E { A(int) } fn main() { let e = E.A; }",
        "wraps a value; construct it as",
    );
    expect_compile_error(
        "enum E { A(int) } fn main() { let e = E.A(1, 2); }",
        "wraps exactly one value, got 2",
    );
    expect_compile_error(
        "enum E { A(int) } fn main() { let e = E.A(\"s\"); }",
        "type mismatch",
    );
    // Equality is only for bare-only enums.
    expect_compile_error(
        "enum E { A(int), B } fn main() { assert(E.B == E.B); }",
        "'==' is not available on enum 'E'",
    );
    // Inference limits.
    expect_compile_error(
        "fn main() { let o = Option.None; }",
        "cannot infer type parameter 'T' of enum 'Option'",
    );
    // Variants are qualified; a bare name is not in scope.
    expect_compile_error(
        "fn main() { let o: Option<int> = Some(5); }",
        "unknown function 'Some'",
    );
    // Declarations.
    expect_compile_error("enum E { } fn main() { }", "has no variants");
    expect_compile_error(
        "enum E { A, A } fn main() { }",
        "duplicate variant 'A'",
    );
    expect_compile_error(
        "enum E { A } enum E { B } fn main() { }",
        "duplicate enum 'E'",
    );
    expect_compile_error(
        "struct E { x: int } enum E { A } fn main() { }",
        "same name as a struct",
    );
    expect_compile_error("enum u8 { A } fn main() { }", "shadows a primitive type");
    // Field access and unwrap misuse.
    expect_compile_error(
        "enum E { A(int) } fn main() { let e = E.A(1); println(e.value); }",
        "field access on non-struct type E",
    );
    expect_compile_error(
        "fn main() { let x = unwrap(1); }",
        "unwrap() needs an Option, found int",
    );
    // Match expressions need one common arm type and a value.
    expect_compile_error(
        "fn main() { let x = match 1 { 0 => 1, _ => \"s\" }; }",
        "match arms have different types",
    );
    expect_compile_error(
        "fn f() { } fn main() { let x = match 1 { 0 => f(), _ => f() }; }",
        "match arms must produce a value",
    );
}

#[test]
fn enum_shadowing_rules() {
    // A local shadows an enum name; the enum is unreachable in that scope.
    expect_compile_error(
        "enum E { A } fn main() { let E = 1; let e = E.A; }",
        "field access on non-struct type int",
    );
    // A user Option shadows the prelude, which disables unwrap...
    expect_compile_error(
        "enum Option { Something } fn main() { let x = unwrap(Option.Something); }",
        "unwrap() needs an Option",
    );
    // ...but the user type itself works normally.
    check_ok(
        r#"
enum Option { Something, Nothing }
fn main() {
    let o = Option.Something;
    assert(o == Option.Something && o != Option.Nothing);
    println("ok");
}
"#,
    );
    // A user function named unwrap shadows the builtin.
    check_ok(
        r#"
fn unwrap(x: int): int { x + 1 }
fn main() {
    assert(unwrap(1) == 2);
    println("ok");
}
"#,
    );
}

#[test]
fn nil_is_gone() {
    // `nil` is no longer part of the language; it is just an unbound name.
    expect_compile_error(
        "struct P { v: int } fn main() { let p: P = nil; }",
        "unknown variable 'nil'",
    );
    expect_compile_error(
        "fn main() { let s: string = nil; }",
        "unknown variable 'nil'",
    );
}

#[test]
fn enum_inline_field_variants() {
    check_ok(
        r#"
enum Tree { Branch { left: Tree, right: Tree, value: int }, Leaf }
enum Mixed<T> { Pack { tag: u8, name: string, item: T, weight: float }, Nothing }
fn build(depth: int, value: int): Tree {
    if depth == 0 {
        return Tree.Branch { left: Tree.Leaf, right: Tree.Leaf, value: value };
    }
    Tree.Branch {
        left: build(depth - 1, value * 2),
        right: build(depth - 1, value * 2 + 1),
        value: value,
    }
}
fn sum(t: Tree): int {
    match t {
        Tree.Branch { left, right, value } => { return value + sum(left) + sum(right); }
        Tree.Leaf => { return 0; }
    }
}
fn main() {
    // Inline fields live in the enum object itself; deep trees of them
    // survive relocation (the harness reruns with a 64 KiB nursery).
    let t = build(10, 1);
    gc_collect();
    let expected = sum(t);
    assert(expected == sum(build(10, 1)));

    // Packed narrow fields around references inside a variant payload:
    // the per-variant descriptor must be exact or the GC corrupts it.
    let m = Mixed.Pack { tag: 7, name: "box", item: [1, 2, 3], weight: 2.5 };
    gc_collect();
    match m {
        Mixed.Pack { tag: g, name: n, item: xs, weight: w } => {
            assert(g == 7 && n == "box" && xs[2] == 3 && w == 2.5);
        }
        Mixed.Nothing => { assert(false); }
    }

    // Field order in literals and patterns is free; `field` alone is
    // shorthand for `field: field`; `_` ignores a field's value.
    let m2 = Mixed.Pack { weight: 1.0, item: "s", name: "n", tag: 2 };
    match m2 {
        Mixed.Pack { item, tag: _, weight: _, name: _ } => { assert(item == "s"); }
        Mixed.Nothing => { }
    }

    // A bound reference field aliases the shared object.
    let shared = [9];
    match (Mixed.Pack { tag: 1, name: "a", item: shared, weight: 0.0 }) {
        Mixed.Pack { item, tag: _, name: _, weight: _ } => { push(item, 10); }
        Mixed.Nothing => { }
    }
    assert(len(shared) == 2 && shared[1] == 10);
    println("ok");
}
"#,
    );
}

#[test]
fn enum_inline_field_errors() {
    expect_compile_error(
        "enum E { R { w: int } } fn main() { let e = E.R { w: 1, h: 2 }; }",
        "variant 'R' has no field 'h'",
    );
    expect_compile_error(
        "enum E { R { w: int, h: int } } fn main() { let e = E.R { w: 1 }; }",
        "missing field 'h'",
    );
    expect_compile_error(
        "enum E { R { w: int } } fn main() { let e = E.R { w: 1, w: 2 }; }",
        "duplicate field 'w'",
    );
    expect_compile_error(
        "enum E { R { w: int } } fn main() { let e = E.R(1); }",
        "has named fields; construct it as",
    );
    expect_compile_error(
        "enum E { R { w: int } } fn main() { let e = E.R; }",
        "has named fields; construct it as",
    );
    expect_compile_error(
        "enum E { A(int) } fn main() { let e = E.A { v: 1 }; }",
        "wraps a value; construct it as",
    );
    expect_compile_error(
        "enum E { A } fn main() { let e = E.A { v: 1 }; }",
        "is bare; write",
    );
    expect_compile_error(
        "enum E { R { w: int } } fn main() { match (E.R { w: 1 }) { E.R(x) => { } } }",
        "has named fields; match it with",
    );
    expect_compile_error(
        "enum E { R { w: int, h: int } } fn main() { match (E.R { w: 1, h: 2 }) { E.R { w: a } => { } } }",
        "missing field 'h'",
    );
    expect_compile_error(
        "enum E { R { w: int } } fn main() { match (E.R { w: 1 }) { E.R { q: a } => { } } }",
        "variant 'R' has no field 'q'",
    );
    expect_compile_error(
        "enum E { R { w: int, w: int } } fn main() { }",
        "duplicate field 'w' in variant 'R'",
    );
    expect_compile_error("enum E { R { } } fn main() { }", "empty field list");
    expect_compile_error(
        "fn main() { let e = Nope.R { w: 1 }; }",
        "unknown enum 'Nope'",
    );
    // == stays banned: field variants are not bare.
    expect_compile_error(
        "enum E { R { w: int }, B } fn main() { assert(E.B == E.B); }",
        "'==' is not available on enum 'E'",
    );
}
