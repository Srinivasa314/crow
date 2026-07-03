//! AST for Crow. The type checker resolves names and fills in the
//! `ty` / resolution fields in place; codegen reads the annotated tree.

use crate::types::Type;

#[derive(Debug)]
pub struct Program {
    pub structs: Vec<StructDef>,
    pub enums: Vec<EnumDef>,
    pub funcs: Vec<FuncDef>,
    /// One record per function in an `impl` block. The parser flattens each
    /// method into `funcs` under the name `Type.method` (unspellable as an
    /// identifier, so it cannot collide with user functions); the checker
    /// resolves `type_name` and builds the method lookup table.
    pub methods: Vec<MethodDef>,
}

/// A method or associated function, as declared in `impl Type { ... }`.
#[derive(Debug)]
pub struct MethodDef {
    /// The impl block's target type name (unresolved).
    pub type_name: String,
    /// The method's own name (`funcs[func].name` is `Type.method`).
    pub name: String,
    /// Index of the flattened definition in `Program::funcs`.
    pub func: u32,
    /// True when the first parameter is `self` (a method); false for an
    /// associated function called as `Type.name(...)`.
    pub has_self: bool,
    /// How many type parameters the impl header declared (`impl Pair<T>` →
    /// 1); must match the target type's arity.
    pub impl_type_params: u32,
    pub line: u32,
}

#[derive(Debug)]
pub struct StructDef {
    pub name: String,
    pub type_params: Vec<String>,
    pub fields: Vec<(String, TypeExpr)>,
    pub line: u32,
}

#[derive(Debug)]
pub struct EnumDef {
    pub name: String,
    pub type_params: Vec<String>,
    pub variants: Vec<(String, VariantPayloadExpr)>,
    pub line: u32,
}

/// Syntactic variant payload: bare, `(type)`, or `{ name: type, ... }`.
#[derive(Debug)]
pub enum VariantPayloadExpr {
    Bare,
    Single(TypeExpr),
    Fields(Vec<(String, TypeExpr)>),
}

/// `Clone` exists on function bodies so codegen can instantiate a generic
/// function: each instantiation clones the checked body and substitutes the
/// type arguments into every annotation.
#[derive(Debug, Clone)]
pub struct FuncDef {
    pub name: String,
    pub type_params: Vec<String>,
    pub params: Vec<(String, TypeExpr)>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    pub line: u32,
    /// Total number of locals (params + lets) in this function, assigned by
    /// the checker. Lambdas inside the body have their own numbering.
    pub num_locals: u32,
    /// True for a function flattened out of an `impl` block. Methods are
    /// never plain values (bound methods build their own closures), so they
    /// get no thunk or static closure object.
    pub is_method: bool,
}

/// Syntactic type, before resolution.
#[derive(Debug, Clone)]
#[allow(dead_code)] // line fields kept for future diagnostics
pub enum TypeExpr {
    Named(String, Vec<TypeExpr>, u32),
    Array(Box<TypeExpr>),
    Fn(Vec<TypeExpr>, Option<Box<TypeExpr>>, u32),
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let {
        name: String,
        ann: Option<TypeExpr>,
        init: Expr,
        line: u32,
        /// Local index within the enclosing function, assigned by the checker.
        local: u32,
        ty: Type,
    },
    Assign {
        target: Expr,
        /// `Some(op)` for compound assignment (`x += v` etc.); the target's
        /// object/index subexpressions are evaluated once.
        op: Option<BinOp>,
        value: Expr,
        line: u32,
    },
    Expr(Expr),
    If {
        cond: Expr,
        then: Block,
        els: Option<Block>,
    },
    While {
        cond: Expr,
        body: Block,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Box<Stmt>>,
        body: Block,
    },
    Return {
        value: Option<Expr>,
        line: u32,
    },
    Break(u32),
    Continue(u32),
    Block(Block),
    /// `match expr { pat => { ... }, ... }` in statement position; arms are
    /// blocks and must be exhaustive.
    Match {
        scrutinee: Expr,
        arms: Vec<(Pattern, Block)>,
        line: u32,
    },
}

/// A match pattern. One constructor deep: a qualified enum variant with its
/// payload binders, a literal, or the wildcard.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// `Enum.Variant`, `Enum.Variant(binder)`, or
    /// `Enum.Variant { field: binder, ... }`.
    Variant {
        enum_name: String,
        variant: String,
        args: PatArgs,
        /// Variant tag (declaration index), set by the checker.
        tag: u32,
        line: u32,
    },
    /// Integer literal, optionally negated. `value` is the canonical
    /// two's-complement pattern, folded by the checker at the scrutinee type.
    IntLit {
        neg: bool,
        digits: u64,
        value: u64,
        line: u32,
    },
    /// `b'X'` byte literal; requires a u8 scrutinee.
    ByteLit { value: u8, line: u32 },
    BoolLit { value: bool, line: u32 },
    Wildcard { line: u32 },
}

/// How a variant pattern binds the payload; must mirror the variant's
/// declaration shape.
#[derive(Debug, Clone)]
pub enum PatArgs {
    Bare,
    Single(PatBinder),
    /// (field name, binder, field index set by the checker). Every declared
    /// field must appear; bind a field to `_` to ignore it.
    Fields(Vec<(String, PatBinder, u32)>),
}

impl PatArgs {
    pub fn binders_mut(&mut self) -> Vec<&mut PatBinder> {
        match self {
            PatArgs::Bare => Vec::new(),
            PatArgs::Single(b) => vec![b],
            PatArgs::Fields(fs) => fs.iter_mut().map(|(_, b, _)| b).collect(),
        }
    }
}

/// One payload binding of a variant pattern, scoped to its arm.
#[derive(Debug, Clone)]
pub struct PatBinder {
    pub name: String,
    /// Local index, assigned by the checker.
    pub local: u32,
    /// Bound value's type in the scrutinee's instantiation, set by the checker.
    pub ty: Type,
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: u32,
    /// Filled by the checker.
    pub ty: Type,
}

impl Expr {
    pub fn new(kind: ExprKind, line: u32) -> Expr {
        Expr { kind, line, ty: Type::Unknown }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VarRes {
    /// Local variable (params and lets share one index space per function).
    Local(u32),
    /// Variable captured from an enclosing function (closure field index).
    Captured(u32),
    /// Top-level function used as a value.
    Func(u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
    /// `~`, bitwise complement (integers only).
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    /// Operator text, for diagnostics and token display.
    pub fn sym(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Builtin {
    Println,
    Print,
    Len,
    Push,
    Pop,
    Itos,
    Ftos,
    Itof,
    Stoi,
    Stof,
    Stob,
    Btos,
    Assert,
    GcCollect,
    Unwrap,
}

#[derive(Debug, Clone)]
pub struct LambdaDef {
    pub params: Vec<(String, TypeExpr)>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    pub line: u32,
    /// Assigned by the checker.
    pub id: u32,
    pub num_locals: u32,
    pub captures: Vec<Capture>,
}

#[derive(Debug, Clone)]
pub struct Capture {
    pub name: String,
    pub ty: Type,
    /// Where the captured value lives in the *enclosing* function.
    pub src: VarRes,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    /// The value is the literal's two's-complement bit pattern. Digits lex to
    /// their unsigned value; the checker folds `-literal` into a negative
    /// pattern and records the integer kind in `Expr::ty`.
    Int(u64),
    /// A `b'X'` byte literal. Unlike `Int`, its type is always `u8`.
    Byte(u8),
    Float(f64),
    Bool(bool),
    Str(String),
    Var {
        name: String,
        res: Option<VarRes>,
    },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `if cond { a } else { b }` in expression position. `else` is
    /// mandatory and each branch is a single expression.
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        /// Set by the checker when the callee is a direct top-level function.
        direct: Option<u32>,
        /// Inferred type arguments when the callee is generic (empty
        /// otherwise), set by the checker. In a generic caller these may
        /// mention the caller's own type parameters; codegen substitutes
        /// them per instantiation.
        inst: Vec<Type>,
    },
    Builtin(Builtin, Vec<Expr>),
    Field {
        obj: Box<Expr>,
        name: String,
        /// Field index within the struct, set by the checker.
        index: u32,
    },
    Index(Box<Expr>, Box<Expr>),
    /// `expr as Type`; the resolved target type lands in `Expr::ty`.
    Cast(Box<Expr>, TypeExpr),
    ArrayLit(Vec<Expr>),
    StructLit {
        name: String,
        /// (field name, value, resolved field index)
        fields: Vec<(String, Expr, u32)>,
        struct_id: u32,
    },
    /// `Enum.Variant { field: value, ... }`: construction of an
    /// inline-fields variant. Unlike the bare/single forms this is
    /// syntactically unambiguous, so the parser produces it directly (the
    /// enum name resolves in the type namespace, like a struct literal);
    /// the checker rewrites it into `VariantLit`.
    VariantStructLit {
        enum_name: String,
        variant: String,
        /// (field name, value, resolved field index)
        fields: Vec<(String, Expr, u32)>,
    },
    /// A checked enum variant construction. Parsed as field access / call
    /// (or `VariantStructLit`) and rewritten into this by the checker.
    VariantLit {
        args: VariantArgs,
        enum_id: u32,
        /// The variant's declaration index, which is also its runtime tag.
        tag: u32,
    },
    /// `match expr { pat => expr, ... }` in expression position; arms are
    /// single expressions of one common type and must be exhaustive.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<(Pattern, Expr)>,
    },
    Lambda(Box<LambdaDef>),
    /// `expr.method` without a call: a bound method. Evaluates the receiver
    /// and builds a fresh closure capturing it, whose code adapts the
    /// closure calling convention to the method. Parsed as field access and
    /// rewritten into this by the checker.
    BoundMethod {
        obj: Box<Expr>,
        /// The method's flattened function id.
        fid: u32,
        /// Inferred type arguments when the method is generic (all solved
        /// from the receiver type), like `Call::inst`.
        inst: Vec<Type>,
    },
}

/// A checked variant construction's payload values.
#[derive(Debug, Clone)]
pub enum VariantArgs {
    Bare,
    Single(Box<Expr>),
    /// (field name, value, resolved field index)
    Fields(Vec<(String, Expr, u32)>),
}
