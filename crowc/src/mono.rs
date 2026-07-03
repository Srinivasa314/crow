//! Shape-keyed monomorphization.
//!
//! Generic functions are compiled once per *shape* of their type arguments,
//! not once per type. A shape is exactly what codegen and the GC care about:
//! the ABI register class (float vs word), whether the value is a GC
//! reference (stack maps, write barriers, descriptor refmaps), and the
//! packed storage width and signedness (struct layout, array elements).
//! Every reference type — strings, structs, arrays, functions — shares the
//! `Ref` shape, so `id<string>` and `id<Point>` share one compiled body and
//! generic structs instantiated at reference types share one GC descriptor.
//!
//! Instantiation substitutes each type argument's *canonical type* (a
//! representative of its shape) into a clone of the checked AST, so codegen
//! runs on fully concrete types and never sees `Type::Param`. This keying
//! also makes polymorphically recursive programs compile: `f<Pair<T>>`
//! collapses to the `Ref` shape no matter how deep the nesting, so the set
//! of instantiations is finite.

use crate::ast::*;
use crate::types::{IntKind, Type};
use crate::typeck::Checked;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shape {
    /// Any GC reference: string, struct, enum, array, function.
    Ref,
    /// 64-bit float (its own ABI register class).
    F64,
    /// Full-width word: i64/u64 (sign is irrelevant at full width).
    W64,
    I32,
    U32,
    I16,
    U16,
    I8,
    /// u8 and bool (both one zero-extended byte).
    U8,
}

pub fn shape_of(t: &Type) -> Shape {
    match t {
        Type::Float => Shape::F64,
        Type::Bool => Shape::U8,
        Type::Int(IntKind::I64) | Type::Int(IntKind::U64) => Shape::W64,
        Type::Int(IntKind::I32) => Shape::I32,
        Type::Int(IntKind::U32) => Shape::U32,
        Type::Int(IntKind::I16) => Shape::I16,
        Type::Int(IntKind::U16) => Shape::U16,
        Type::Int(IntKind::I8) => Shape::I8,
        Type::Int(IntKind::U8) => Shape::U8,
        Type::Str | Type::Struct(..) | Type::Enum(..) | Type::Array(_) | Type::Fn(..) => {
            Shape::Ref
        }
        Type::Unit | Type::Unknown | Type::Param(_) => {
            unreachable!("checker excluded {t:?} as a type argument")
        }
    }
}

/// The representative type substituted into an instantiation's body. Codegen
/// decisions (size, signedness, refness, register class) depend only on the
/// shape, so compiling with the representative yields code that is correct
/// for every type of that shape.
pub fn canonical(s: Shape) -> Type {
    match s {
        Shape::Ref => Type::Str,
        Shape::F64 => Type::Float,
        Shape::W64 => Type::Int(IntKind::I64),
        Shape::I32 => Type::Int(IntKind::I32),
        Shape::U32 => Type::Int(IntKind::U32),
        Shape::I16 => Type::Int(IntKind::I16),
        Shape::U16 => Type::Int(IntKind::U16),
        Shape::I8 => Type::Int(IntKind::I8),
        Shape::U8 => Type::Int(IntKind::U8),
    }
}

/// Symbol-name suffix for an instantiation, e.g. `$r_w` for `<string, int>`.
/// Empty for a non-generic function.
pub fn suffix(shapes: &[Shape]) -> String {
    if shapes.is_empty() {
        return String::new();
    }
    let codes: Vec<&str> = shapes
        .iter()
        .map(|s| match s {
            Shape::Ref => "r",
            Shape::F64 => "f",
            Shape::W64 => "w",
            Shape::I32 => "i32",
            Shape::U32 => "u32",
            Shape::I16 => "i16",
            Shape::U16 => "u16",
            Shape::I8 => "i8",
            Shape::U8 => "u8",
        })
        .collect();
    format!("${}", codes.join("_"))
}

/// Side-table data for one lambda occurring in an instantiated body.
pub struct LambdaInst {
    pub locals: Vec<Type>,
    pub params: Vec<Type>,
    pub ret: Type,
}

/// A function body ready for codegen: a clone of the checked AST with every
/// type annotation substituted, plus substituted copies of the checker's
/// side tables. Contains no `Type::Param`.
pub struct Instance {
    pub def: FuncDef,
    pub locals: Vec<Type>,
    pub params: Vec<Type>,
    pub ret: Type,
    pub shapes: Vec<Shape>,
    /// Keyed by the checker-assigned (global) lambda id.
    pub lambdas: HashMap<u32, LambdaInst>,
    /// Display name for diagnostics, e.g. `map$r_w`.
    pub name: String,
}

pub fn instantiate(fid: u32, def: &FuncDef, checked: &Checked, shapes: Vec<Shape>) -> Instance {
    let args: Vec<Type> = shapes.iter().map(|s| canonical(*s)).collect();
    let mut def = def.clone();
    let mut w = Subst { args: &args, checked, lambdas: HashMap::new() };
    w.block(&mut def.body);
    let sig = &checked.funcs[fid as usize];
    Instance {
        locals: checked.func_locals[fid as usize].iter().map(|t| t.subst(&args)).collect(),
        params: sig.params.iter().map(|t| t.subst(&args)).collect(),
        ret: sig.ret.subst(&args),
        name: format!("{}{}", def.name, suffix(&shapes)),
        lambdas: w.lambdas,
        shapes,
        def,
    }
}

/// Rewrites every type annotation in a cloned body. Matches are exhaustive
/// so a new AST node cannot silently escape substitution: any `Type::Param`
/// reaching codegen panics in `shape_of`/`size_bytes`.
struct Subst<'a> {
    args: &'a [Type],
    checked: &'a Checked,
    lambdas: HashMap<u32, LambdaInst>,
}

impl Subst<'_> {
    fn block(&mut self, b: &mut Block) {
        for s in &mut b.stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &mut Stmt) {
        match s {
            Stmt::Let { init, ty, .. } => {
                *ty = ty.subst(self.args);
                self.expr(init);
            }
            Stmt::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            Stmt::Expr(e) => self.expr(e),
            Stmt::If { cond, then, els } => {
                self.expr(cond);
                self.block(then);
                if let Some(els) = els {
                    self.block(els);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            Stmt::For { init, cond, step, body } => {
                if let Some(init) = init {
                    self.stmt(init);
                }
                if let Some(cond) = cond {
                    self.expr(cond);
                }
                if let Some(step) = step {
                    self.stmt(step);
                }
                self.block(body);
            }
            Stmt::Return { value: Some(v), .. } => self.expr(v),
            Stmt::Return { value: None, .. } | Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Block(b) => self.block(b),
            Stmt::Match { scrutinee, arms, .. } => {
                self.expr(scrutinee);
                for (pat, body) in arms {
                    self.pattern(pat);
                    self.block(body);
                }
            }
        }
    }

    fn pattern(&mut self, pat: &mut Pattern) {
        if let Pattern::Variant { args, .. } = pat {
            for b in args.binders_mut() {
                b.ty = b.ty.subst(self.args);
            }
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        e.ty = e.ty.subst(self.args);
        match &mut e.kind {
            ExprKind::Unary(_, a) | ExprKind::Cast(a, _) | ExprKind::Field { obj: a, .. } => {
                self.expr(a)
            }
            ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            ExprKind::If { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            ExprKind::Call { callee, args, inst, .. } => {
                for t in inst.iter_mut() {
                    *t = t.subst(self.args);
                }
                self.expr(callee);
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::Builtin(_, args) | ExprKind::ArrayLit(args) => {
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::StructLit { fields, .. } => {
                for (_, v, _) in fields {
                    self.expr(v);
                }
            }
            ExprKind::VariantLit { args, .. } => match args {
                VariantArgs::Bare => {}
                VariantArgs::Single(a) => self.expr(a),
                VariantArgs::Fields(fields) => {
                    for (_, v, _) in fields {
                        self.expr(v);
                    }
                }
            },
            ExprKind::VariantStructLit { .. } => {
                unreachable!("rewritten into VariantLit by the checker")
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for (pat, body) in arms {
                    self.pattern(pat);
                    self.expr(body);
                }
            }
            ExprKind::Lambda(lam) => {
                for cap in &mut lam.captures {
                    cap.ty = cap.ty.subst(self.args);
                }
                self.block(&mut lam.body);
                let id = lam.id as usize;
                let (params, ret) = &self.checked.lambda_sigs[id];
                self.lambdas.insert(
                    lam.id,
                    LambdaInst {
                        locals: self.checked.lambda_locals[id]
                            .iter()
                            .map(|t| t.subst(self.args))
                            .collect(),
                        params: params.iter().map(|t| t.subst(self.args)).collect(),
                        ret: ret.subst(self.args),
                    },
                );
            }
            ExprKind::Int(_)
            | ExprKind::Byte(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Var { .. } => {}
        }
    }
}
