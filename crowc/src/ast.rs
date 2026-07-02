//! AST for Crow. The type checker resolves names and fills in the
//! `ty` / resolution fields in place; codegen reads the annotated tree.

use crate::types::Type;

#[derive(Debug)]
pub struct Program {
    pub structs: Vec<StructDef>,
    pub funcs: Vec<FuncDef>,
}

#[derive(Debug)]
pub struct StructDef {
    pub name: String,
    pub type_params: Vec<String>,
    pub fields: Vec<(String, TypeExpr)>,
    pub line: u32,
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
    Ftoi,
    Stoi,
    Stof,
    Stob,
    Btos,
    Assert,
    GcCollect,
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
    Nil,
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
    Lambda(Box<LambdaDef>),
}
