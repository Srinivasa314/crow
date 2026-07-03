//! Type checker: resolves names and types, infers local types, analyses
//! closure captures, and rewrites builtin calls. Annotates the AST in place.

use crate::ast::*;
use crate::types::{EnumInfo, IntKind, StructInfo, Type, VariantPayload, INT};
use std::collections::HashMap;

pub struct FuncSig {
    #[allow(dead_code)]
    pub name: String,
    /// Type parameter names; `params`/`ret` may contain `Type::Param`
    /// indices into this list. Empty for a non-generic function.
    pub type_params: Vec<String>,
    pub params: Vec<Type>,
    pub ret: Type,
}

pub struct Checked {
    pub structs: Vec<StructInfo>,
    pub enums: Vec<EnumInfo>,
    pub funcs: Vec<FuncSig>,
    /// Types of each function's locals, indexed by `VarRes::Local`.
    pub func_locals: Vec<Vec<Type>>,
    /// Same for lambdas, indexed by lambda id.
    pub lambda_locals: Vec<Vec<Type>>,
    /// (param types, return type) per lambda id.
    pub lambda_sigs: Vec<(Vec<Type>, Type)>,
}

pub fn check(program: &mut Program) -> Result<Checked, String> {
    let mut ck = Checker {
        structs: Vec::new(),
        struct_ids: HashMap::new(),
        enums: Vec::new(),
        enum_ids: HashMap::new(),
        option_enum: None,
        funcs: Vec::new(),
        func_ids: HashMap::new(),
        methods: HashMap::new(),
        ctxs: Vec::new(),
        loop_depth: 0,
        func_locals: Vec::new(),
        lambda_locals: Vec::new(),
        lambda_sigs: Vec::new(),
        type_params: Vec::new(),
    };
    // Names first (structs, enums, the prelude), then member types: struct
    // fields and enum payloads may reference any type in any order.
    ck.collect_struct_names(program)?;
    ck.collect_enum_names(program)?;
    ck.resolve_struct_fields(program)?;
    ck.resolve_enum_variants(program)?;
    ck.collect_methods(program)?;
    ck.collect_funcs(program)?;
    for (i, f) in program.funcs.iter_mut().enumerate() {
        ck.check_func(i as u32, f)?;
    }
    match ck.func_ids.get("main") {
        None => return Err("no 'main' function defined".to_string()),
        Some(&id) => {
            let sig = &ck.funcs[id as usize];
            if !sig.type_params.is_empty() {
                return Err(format!(
                    "{}: 'main' cannot be generic",
                    program.funcs[id as usize].line
                ));
            }
            if !sig.params.is_empty() || sig.ret != Type::Unit {
                return Err(format!(
                    "{}: 'main' must take no parameters and return nothing",
                    program.funcs[id as usize].line
                ));
            }
        }
    }
    Ok(Checked {
        structs: ck.structs,
        enums: ck.enums,
        funcs: ck.funcs,
        func_locals: ck.func_locals,
        lambda_locals: ck.lambda_locals.into_iter().map(Option::unwrap).collect(),
        lambda_sigs: ck.lambda_sigs.into_iter().map(Option::unwrap).collect(),
    })
}

/// Free-function builtins: only the ones with no natural receiver.
/// Everything else is a *method* on its receiver type (§ builtin methods).
const BUILTINS: &[(&str, Builtin)] = &[
    ("println", Builtin::Println),
    ("print", Builtin::Print),
    ("assert", Builtin::Assert),
    ("gc_collect", Builtin::GcCollect),
];

/// Former free-function builtins that became methods, with the new
/// spelling; used purely for a helpful "unknown function" diagnostic.
const RETIRED_BUILTINS: &[(&str, &str)] = &[
    ("len", "x.len()"),
    ("push", "arr.push(value)"),
    ("pop", "arr.pop()"),
    ("itos", "i.to_string()"),
    ("ftos", "f.to_string()"),
    ("itof", "i.to_float()"),
    ("ftoi", "f as int"),
    ("stoi", "s.to_int()"),
    ("stof", "s.to_float()"),
    ("stob", "s.to_bytes()"),
    ("btos", "bytes.to_string()"),
    ("unwrap", "opt.unwrap()"),
];

fn retired_hint(name: &str) -> Option<&'static str> {
    RETIRED_BUILTINS.iter().find(|(n, _)| *n == name).map(|(_, h)| *h)
}

/// Method-table key: a struct or enum definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TypeKey {
    Struct(u32),
    Enum(u32),
}

/// Per-function (or per-lambda) checking context.
struct FnCtx {
    /// Lexical scopes of (name, local index, type).
    scopes: Vec<Vec<(String, u32, Type)>>,
    locals: Vec<Type>,
    /// None for a top-level function; Some(lambda) for a lambda context.
    captures: Option<Vec<Capture>>,
    ret: Type,
}

struct Checker {
    structs: Vec<StructInfo>,
    struct_ids: HashMap<String, u32>,
    enums: Vec<EnumInfo>,
    enum_ids: HashMap<String, u32>,
    /// Id of the prelude `Option<T>`, unless shadowed by a user type.
    option_enum: Option<u32>,
    funcs: Vec<FuncSig>,
    func_ids: HashMap<String, u32>,
    /// Methods and associated functions from impl blocks, keyed by target
    /// type and name; the value is (flattened function id, has self).
    methods: HashMap<(TypeKey, String), (u32, bool)>,
    ctxs: Vec<FnCtx>,
    loop_depth: u32,
    func_locals: Vec<Vec<Type>>,
    lambda_locals: Vec<Option<Vec<Type>>>,
    lambda_sigs: Vec<Option<(Vec<Type>, Type)>>,
    /// Type parameter names of the function or struct currently being
    /// checked; `Type::Param` indices point into this list.
    type_params: Vec<String>,
}

type CResult<T> = Result<T, String>;

/// How a variant construction was written at the use site.
enum VariantCtor {
    Bare,
    Call(Vec<Expr>),
    Fields(Vec<(String, Expr, u32)>),
}

/// Match a declared (possibly parameter-containing) type against an actual
/// argument type, binding unbound parameters in `solved` and checking
/// already-bound ones for equality.
fn unify(decl: &Type, actual: &Type, solved: &mut [Option<Type>]) -> Result<(), ()> {
    match (decl, actual) {
        (Type::Param(i), _) => match actual {
            Type::Unit | Type::Unknown => Err(()),
            _ => match &solved[*i as usize] {
                Some(t) if t == actual => Ok(()),
                Some(_) => Err(()),
                None => {
                    solved[*i as usize] = Some(actual.clone());
                    Ok(())
                }
            },
        },
        (Type::Array(a), Type::Array(b)) => unify(a, b, solved),
        (Type::Enum(i, ax), Type::Enum(j, bx)) if i == j => {
            for (a, b) in ax.iter().zip(bx) {
                unify(a, b, solved)?;
            }
            Ok(())
        }
        (Type::Struct(i, ax), Type::Struct(j, bx)) if i == j => {
            for (a, b) in ax.iter().zip(bx) {
                unify(a, b, solved)?;
            }
            Ok(())
        }
        (Type::Fn(ap, ar), Type::Fn(bp, br)) if ap.len() == bp.len() => {
            for (a, b) in ap.iter().zip(bp) {
                unify(a, b, solved)?;
            }
            unify(ar, br, solved)
        }
        _ => {
            if decl == actual {
                Ok(())
            } else {
                Err(())
            }
        }
    }
}

/// Substitute the parameters solved so far, leaving unsolved ones in place.
fn subst_partial(t: &Type, solved: &[Option<Type>]) -> Type {
    let args: Vec<Type> = solved
        .iter()
        .enumerate()
        .map(|(i, s)| s.clone().unwrap_or(Type::Param(i as u32)))
        .collect();
    t.subst(&args)
}

impl Checker {
    fn show(&self, t: &Type) -> String {
        format!("{}", t.display(&self.structs, &self.enums, &self.type_params))
    }

    /// Validate a declaration's type parameter list.
    fn check_type_params(&self, params: &[String], line: u32) -> CResult<()> {
        for (i, p) in params.iter().enumerate() {
            if params[..i].contains(p) {
                return Err(format!("{line}: duplicate type parameter '{p}'"));
            }
            if matches!(p.as_str(), "float" | "bool" | "string")
                || IntKind::from_name(p).is_some()
            {
                return Err(format!(
                    "{line}: type parameter '{p}' shadows a primitive type"
                ));
            }
        }
        Ok(())
    }

    // -- Declarations -------------------------------------------------------

    fn collect_struct_names(&mut self, program: &Program) -> CResult<()> {
        for s in &program.structs {
            if self.struct_ids.contains_key(&s.name) {
                return Err(format!("{}: duplicate struct '{}'", s.line, s.name));
            }
            if matches!(s.name.as_str(), "float" | "bool" | "string")
                || IntKind::from_name(&s.name).is_some()
            {
                return Err(format!("{}: struct name '{}' shadows a primitive type", s.line, s.name));
            }
            self.check_type_params(&s.type_params, s.line)?;
            self.struct_ids.insert(s.name.clone(), self.structs.len() as u32);
            self.structs.push(StructInfo {
                name: s.name.clone(),
                type_params: s.type_params.clone(),
                fields: Vec::new(),
                line: s.line,
            });
        }
        Ok(())
    }

    fn resolve_struct_fields(&mut self, program: &Program) -> CResult<()> {
        for s in &program.structs {
            self.type_params = s.type_params.clone();
            let mut fields = Vec::new();
            for (fname, fty) in &s.fields {
                if fields.iter().any(|(n, _)| n == fname) {
                    return Err(format!("{}: duplicate field '{}' in struct '{}'", s.line, fname, s.name));
                }
                fields.push((fname.clone(), self.resolve_type(fty)?));
            }
            self.type_params.clear();
            if fields.len() > 64 {
                return Err(format!("{}: struct '{}' has more than 64 fields", s.line, s.name));
            }
            let id = self.struct_ids[&s.name];
            self.structs[id as usize].fields = fields;
        }
        Ok(())
    }

    fn collect_enum_names(&mut self, program: &Program) -> CResult<()> {
        for e in &program.enums {
            if self.enum_ids.contains_key(&e.name) {
                return Err(format!("{}: duplicate enum '{}'", e.line, e.name));
            }
            if self.struct_ids.contains_key(&e.name) {
                return Err(format!(
                    "{}: enum '{}' has the same name as a struct",
                    e.line, e.name
                ));
            }
            if matches!(e.name.as_str(), "float" | "bool" | "string")
                || IntKind::from_name(&e.name).is_some()
            {
                return Err(format!("{}: enum name '{}' shadows a primitive type", e.line, e.name));
            }
            self.check_type_params(&e.type_params, e.line)?;
            self.enum_ids.insert(e.name.clone(), self.enums.len() as u32);
            self.enums.push(EnumInfo {
                name: e.name.clone(),
                type_params: e.type_params.clone(),
                variants: Vec::new(),
                line: e.line,
            });
        }
        // The prelude `Option<T>`, unless a user type shadows it.
        if !self.enum_ids.contains_key("Option") && !self.struct_ids.contains_key("Option") {
            let id = self.enums.len() as u32;
            self.enum_ids.insert("Option".to_string(), id);
            self.enums.push(EnumInfo {
                name: "Option".to_string(),
                type_params: vec!["T".to_string()],
                variants: vec![
                    ("Some".to_string(), VariantPayload::Single(Type::Param(0))),
                    ("None".to_string(), VariantPayload::Bare),
                ],
                line: 0,
            });
            self.option_enum = Some(id);
        }
        Ok(())
    }

    fn resolve_enum_variants(&mut self, program: &Program) -> CResult<()> {
        for e in &program.enums {
            if e.variants.is_empty() {
                return Err(format!("{}: enum '{}' has no variants", e.line, e.name));
            }
            self.type_params = e.type_params.clone();
            let mut variants = Vec::new();
            for (vname, payload) in &e.variants {
                if variants.iter().any(|(n, _): &(String, _)| n == vname) {
                    return Err(format!(
                        "{}: duplicate variant '{}' in enum '{}'",
                        e.line, vname, e.name
                    ));
                }
                let pty = match payload {
                    VariantPayloadExpr::Bare => VariantPayload::Bare,
                    VariantPayloadExpr::Single(te) => {
                        VariantPayload::Single(self.resolve_type(te)?)
                    }
                    VariantPayloadExpr::Fields(fs) => {
                        if fs.is_empty() {
                            return Err(format!(
                                "{}: variant '{}' has an empty field list; \
                                 write a bare variant instead",
                                e.line, vname
                            ));
                        }
                        if fs.len() > 64 {
                            return Err(format!(
                                "{}: variant '{}' has more than 64 fields",
                                e.line, vname
                            ));
                        }
                        let mut fields = Vec::new();
                        for (fname, fty) in fs {
                            if fields.iter().any(|(n, _): &(String, _)| n == fname) {
                                return Err(format!(
                                    "{}: duplicate field '{}' in variant '{}'",
                                    e.line, fname, vname
                                ));
                            }
                            fields.push((fname.clone(), self.resolve_type(fty)?));
                        }
                        VariantPayload::Fields(fields)
                    }
                };
                variants.push((vname.clone(), pty));
            }
            self.type_params.clear();
            let id = self.enum_ids[&e.name];
            self.enums[id as usize].variants = variants;
        }
        Ok(())
    }

    /// Validate impl blocks and build the method lookup table. Runs before
    /// `collect_funcs` (which resolves the flattened signatures, including
    /// the synthesized `self` parameter) purely for error-message order;
    /// bodies are checked later, when both tables exist.
    fn collect_methods(&mut self, program: &Program) -> CResult<()> {
        for m in &program.methods {
            let key = if let Some(&sid) = self.struct_ids.get(&m.type_name) {
                let info = &self.structs[sid as usize];
                if info.fields.iter().any(|(n, _)| n == &m.name) {
                    return Err(format!(
                        "{}: method '{}' has the same name as a field of struct '{}'",
                        m.line, m.name, m.type_name
                    ));
                }
                if info.type_params.len() != m.impl_type_params as usize {
                    return Err(format!(
                        "{}: impl block for '{}' must declare {} type parameter(s), got {}",
                        m.line,
                        m.type_name,
                        info.type_params.len(),
                        m.impl_type_params
                    ));
                }
                TypeKey::Struct(sid)
            } else if let Some(&eid) = self.enum_ids.get(&m.type_name) {
                let info = &self.enums[eid as usize];
                if info.variants.iter().any(|(n, _)| n == &m.name) {
                    return Err(format!(
                        "{}: method '{}' has the same name as a variant of enum '{}'",
                        m.line, m.name, m.type_name
                    ));
                }
                if info.type_params.len() != m.impl_type_params as usize {
                    return Err(format!(
                        "{}: impl block for '{}' must declare {} type parameter(s), got {}",
                        m.line,
                        m.type_name,
                        info.type_params.len(),
                        m.impl_type_params
                    ));
                }
                TypeKey::Enum(eid)
            } else {
                return Err(format!(
                    "{}: cannot write an impl block for '{}': impl blocks are only for \
                     structs and enums",
                    m.line, m.type_name
                ));
            };
            if self
                .methods
                .insert((key, m.name.clone()), (m.func, m.has_self))
                .is_some()
            {
                return Err(format!(
                    "{}: duplicate method '{}' on '{}'",
                    m.line, m.name, m.type_name
                ));
            }
        }
        Ok(())
    }

    fn lookup_method(&self, recv: &Type, name: &str) -> Option<(u32, bool)> {
        let key = match recv {
            Type::Struct(id, _) => TypeKey::Struct(*id),
            Type::Enum(id, _) => TypeKey::Enum(*id),
            _ => return None,
        };
        self.methods.get(&(key, name.to_string())).copied()
    }

    fn collect_funcs(&mut self, program: &Program) -> CResult<()> {
        for f in &program.funcs {
            if self.func_ids.contains_key(&f.name) {
                // Duplicate methods are caught (with a better message) in
                // `collect_methods`; this covers plain functions.
                return Err(format!("{}: duplicate function '{}'", f.line, f.name));
            }
            self.check_type_params(&f.type_params, f.line)?;
            self.type_params = f.type_params.clone();
            let mut params = Vec::new();
            for (_, pty) in &f.params {
                params.push(self.resolve_type(pty)?);
            }
            let ret = match &f.ret {
                Some(t) => self.resolve_type(t)?,
                None => Type::Unit,
            };
            self.type_params.clear();
            self.func_ids.insert(f.name.clone(), self.funcs.len() as u32);
            self.funcs.push(FuncSig {
                name: f.name.clone(),
                type_params: f.type_params.clone(),
                params,
                ret,
            });
        }
        Ok(())
    }

    fn resolve_type(&self, te: &TypeExpr) -> CResult<Type> {
        match te {
            TypeExpr::Named(name, args, line) => {
                // Type parameters in scope shadow struct names.
                if let Some(i) = self.type_params.iter().position(|p| p == name) {
                    if !args.is_empty() {
                        return Err(format!(
                            "{line}: type parameter '{name}' takes no type arguments"
                        ));
                    }
                    return Ok(Type::Param(i as u32));
                }
                let primitive = match name.as_str() {
                    "float" => Some(Type::Float),
                    "bool" => Some(Type::Bool),
                    "string" => Some(Type::Str),
                    _ => IntKind::from_name(name).map(Type::Int),
                };
                if let Some(t) = primitive {
                    if !args.is_empty() {
                        return Err(format!("{line}: type '{name}' takes no type arguments"));
                    }
                    return Ok(t);
                }
                if let Some(&id) = self.struct_ids.get(name) {
                    let want = self.structs[id as usize].type_params.len();
                    if args.len() != want {
                        return Err(format!(
                            "{line}: struct '{name}' expects {want} type argument(s), got {}",
                            args.len()
                        ));
                    }
                    let targs =
                        args.iter().map(|a| self.resolve_type(a)).collect::<CResult<_>>()?;
                    return Ok(Type::Struct(id, targs));
                }
                if let Some(&id) = self.enum_ids.get(name) {
                    let want = self.enums[id as usize].type_params.len();
                    if args.len() != want {
                        return Err(format!(
                            "{line}: enum '{name}' expects {want} type argument(s), got {}",
                            args.len()
                        ));
                    }
                    let targs =
                        args.iter().map(|a| self.resolve_type(a)).collect::<CResult<_>>()?;
                    return Ok(Type::Enum(id, targs));
                }
                Err(format!("{line}: unknown type '{name}'"))
            }
            TypeExpr::Array(elem) => Ok(Type::Array(Box::new(self.resolve_type(elem)?))),
            TypeExpr::Fn(params, ret, _) => {
                let params = params.iter().map(|p| self.resolve_type(p)).collect::<CResult<_>>()?;
                let ret = match ret {
                    Some(r) => self.resolve_type(r)?,
                    None => Type::Unit,
                };
                Ok(Type::Fn(params, Box::new(ret)))
            }
        }
    }

    // -- Function bodies ----------------------------------------------------

    fn check_func(&mut self, id: u32, f: &mut FuncDef) -> CResult<()> {
        // A generic body is checked once, with its parameters opaque.
        self.type_params = f.type_params.clone();
        let sig_params = self.funcs[id as usize].params.clone();
        let ret = self.funcs[id as usize].ret.clone();
        self.ctxs.push(FnCtx {
            scopes: vec![Vec::new()],
            locals: Vec::new(),
            captures: None,
            ret: ret.clone(),
        });
        for ((pname, _), pty) in f.params.iter().zip(&sig_params) {
            self.declare_local(pname, pty.clone(), f.line)?;
        }
        self.check_block(&mut f.body)?;
        if ret != Type::Unit && !always_returns(&f.body) {
            return Err(format!(
                "{}: function '{}' must return a value on all paths",
                f.line, f.name
            ));
        }
        let ctx = self.ctxs.pop().unwrap();
        f.num_locals = ctx.locals.len() as u32;
        debug_assert_eq!(self.func_locals.len(), id as usize);
        self.func_locals.push(ctx.locals);
        self.type_params.clear();
        Ok(())
    }

    fn declare_local(&mut self, name: &str, ty: Type, line: u32) -> CResult<u32> {
        if name == "_" {
            // Still allocate a slot so codegen stays uniform.
        }
        let ctx = self.ctxs.last_mut().unwrap();
        let idx = ctx.locals.len() as u32;
        if idx >= 4096 {
            return Err(format!("{line}: too many locals in one function"));
        }
        ctx.locals.push(ty.clone());
        ctx.scopes.last_mut().unwrap().push((name.to_string(), idx, ty));
        Ok(idx)
    }

    /// Resolve a variable, inserting captures into intervening lambdas.
    /// Returns the resolution *in the innermost context* plus its type.
    fn resolve_var(&mut self, name: &str) -> Option<(VarRes, Type)> {
        self.resolve_var_at(self.ctxs.len() - 1, name)
    }

    fn resolve_var_at(&mut self, ctx_idx: usize, name: &str) -> Option<(VarRes, Type)> {
        // Local in this context?
        {
            let ctx = &self.ctxs[ctx_idx];
            for scope in ctx.scopes.iter().rev() {
                for (n, idx, ty) in scope.iter().rev() {
                    if n == name {
                        return Some((VarRes::Local(*idx), ty.clone()));
                    }
                }
            }
            // Already captured by this lambda?
            if let Some(caps) = &ctx.captures {
                for (i, c) in caps.iter().enumerate() {
                    if c.name == name {
                        return Some((VarRes::Captured(i as u32), c.ty.clone()));
                    }
                }
            }
        }
        // Not found here. If this is a lambda, try enclosing contexts and
        // record a capture; top-level functions have no enclosing scope.
        if self.ctxs[ctx_idx].captures.is_some() && ctx_idx > 0 {
            if let Some((src, ty)) = self.resolve_var_at(ctx_idx - 1, name) {
                let caps = self.ctxs[ctx_idx].captures.as_mut().unwrap();
                let cap_idx = caps.len() as u32;
                caps.push(Capture { name: name.to_string(), ty: ty.clone(), src });
                return Some((VarRes::Captured(cap_idx), ty));
            }
        }
        None
    }

    fn check_block(&mut self, block: &mut Block) -> CResult<()> {
        self.ctxs.last_mut().unwrap().scopes.push(Vec::new());
        for stmt in &mut block.stmts {
            self.check_stmt(stmt)?;
        }
        self.ctxs.last_mut().unwrap().scopes.pop();
        Ok(())
    }

    fn check_stmt(&mut self, stmt: &mut Stmt) -> CResult<()> {
        match stmt {
            Stmt::Let { name, ann, init, line, local, ty } => {
                let want = match ann {
                    Some(te) => Some(self.resolve_type(te)?),
                    None => None,
                };
                self.check_expr(init, want.as_ref())?;
                let final_ty = match want {
                    Some(t) => {
                        self.require_assignable(&t, &init.ty, *line)?;
                        t
                    }
                    None => match &init.ty {
                        Type::Unit => {
                            return Err(format!("{line}: initializer has no value"))
                        }
                        t => t.clone(),
                    },
                };
                *local = self.declare_local(name, final_ty.clone(), *line)?;
                *ty = final_ty;
            }
            Stmt::Assign { target, op, value, line } => {
                self.check_expr(target, None)?;
                match &target.kind {
                    ExprKind::Var { name, res } => match res {
                        Some(VarRes::Local(_)) => {}
                        Some(VarRes::Captured(_)) => {
                            return Err(format!(
                                "{line}: cannot assign to '{name}': closures capture by value"
                            ))
                        }
                        _ => return Err(format!("{line}: cannot assign to function '{name}'")),
                    },
                    ExprKind::Index(obj, _) if obj.ty == Type::Str => {
                        return Err(format!(
                            "{line}: strings are immutable; cannot assign to an element"
                        ))
                    }
                    ExprKind::Field { .. } | ExprKind::Index(..) => {}
                    _ => return Err(format!("{line}: invalid assignment target")),
                }
                let want = target.ty.clone();
                self.check_expr(value, Some(&want))?;
                self.require_assignable(&want, &value.ty, *line)?;
                if let Some(op) = op {
                    // Compound assignment follows the binary operator's rules
                    // on the target's type.
                    self.check_arith_op(*op, &want, *line)?;
                }
            }
            Stmt::Expr(e) => {
                self.check_expr(e, None)?;
            }
            Stmt::If { cond, then, els } => {
                self.check_cond(cond)?;
                self.check_block(then)?;
                if let Some(els) = els {
                    self.check_block(els)?;
                }
            }
            Stmt::While { cond, body } => {
                self.check_cond(cond)?;
                self.loop_depth += 1;
                self.check_block(body)?;
                self.loop_depth -= 1;
            }
            Stmt::For { init, cond, step, body } => {
                self.ctxs.last_mut().unwrap().scopes.push(Vec::new());
                if let Some(init) = init {
                    self.check_stmt(init)?;
                }
                if let Some(cond) = cond {
                    self.check_cond(cond)?;
                }
                if let Some(step) = step {
                    self.check_stmt(step)?;
                }
                self.loop_depth += 1;
                self.check_block(body)?;
                self.loop_depth -= 1;
                self.ctxs.last_mut().unwrap().scopes.pop();
            }
            Stmt::Return { value, line } => {
                let ret = self.ctxs.last().unwrap().ret.clone();
                match value {
                    None => {
                        if ret != Type::Unit {
                            return Err(format!(
                                "{}: this function must return a value of type {}",
                                line,
                                self.show(&ret)
                            ));
                        }
                    }
                    Some(v) => {
                        if ret == Type::Unit {
                            return Err(format!("{line}: this function does not return a value"));
                        }
                        self.check_expr(v, Some(&ret))?;
                        self.require_assignable(&ret, &v.ty, *line)?;
                    }
                }
            }
            Stmt::Break(line) | Stmt::Continue(line) => {
                if self.loop_depth == 0 {
                    return Err(format!("{line}: 'break'/'continue' outside of a loop"));
                }
            }
            Stmt::Block(b) => self.check_block(b)?,
            Stmt::Match { scrutinee, arms, line } => {
                self.check_expr(scrutinee, None)?;
                let scrut_ty = scrutinee.ty.clone();
                {
                    let mut pats: Vec<&mut Pattern> = arms.iter_mut().map(|(p, _)| p).collect();
                    self.check_patterns(&scrut_ty, &mut pats, *line)?;
                }
                for (pat, body) in arms.iter_mut() {
                    // Binders scope to exactly their arm.
                    self.ctxs.last_mut().unwrap().scopes.push(Vec::new());
                    self.declare_pattern_binders(pat)?;
                    self.check_block(body)?;
                    self.ctxs.last_mut().unwrap().scopes.pop();
                }
            }
        }
        Ok(())
    }

    /// Declare a pattern's binders as locals in the current (arm) scope.
    fn declare_pattern_binders(&mut self, pat: &mut Pattern) -> CResult<()> {
        if let Pattern::Variant { args, line, .. } = pat {
            let line = *line;
            for b in args.binders_mut() {
                b.local = self.declare_local(&b.name, b.ty.clone(), line)?;
            }
        }
        Ok(())
    }

    /// Check a match's patterns against the scrutinee type: resolves variant
    /// tags and binder types in place, rejects foreign or duplicate
    /// patterns, and enforces exhaustiveness.
    fn check_patterns(
        &self,
        scrut_ty: &Type,
        pats: &mut [&mut Pattern],
        line: u32,
    ) -> CResult<()> {
        let mismatch = |pline: u32| -> String {
            format!(
                "{}: pattern does not match the scrutinee type {}",
                pline,
                self.show(scrut_ty)
            )
        };
        let mut wildcard_at: Option<u32> = None;
        // Everything after a `_` arm is unreachable.
        let check_reachable = |wildcard_at: Option<u32>, pline: u32| -> CResult<()> {
            match wildcard_at {
                Some(w) => Err(format!("{pline}: unreachable pattern after '_' at line {w}")),
                None => Ok(()),
            }
        };
        match scrut_ty {
            Type::Enum(eid, targs) => {
                let info = &self.enums[*eid as usize];
                let mut seen = vec![false; info.variants.len()];
                for pat in pats.iter_mut() {
                    match &mut **pat {
                        Pattern::Variant { enum_name, variant, args, tag, line: pline } => {
                            check_reachable(wildcard_at, *pline)?;
                            if self.enum_ids.get(enum_name.as_str()) != Some(eid) {
                                return Err(mismatch(*pline));
                            }
                            let i = match info.variants.iter().position(|(n, _)| n == variant) {
                                Some(i) => i,
                                None => {
                                    return Err(format!(
                                        "{}: enum '{}' has no variant '{}'",
                                        pline, info.name, variant
                                    ))
                                }
                            };
                            if seen[i] {
                                return Err(format!(
                                    "{}: duplicate pattern for variant '{}'",
                                    pline, variant
                                ));
                            }
                            seen[i] = true;
                            *tag = i as u32;
                            // The pattern's shape must mirror the variant's.
                            match (&info.variants[i].1, args) {
                                (VariantPayload::Bare, PatArgs::Bare) => {}
                                (VariantPayload::Bare, _) => {
                                    return Err(format!(
                                        "{}: variant '{}' is bare and has no value to bind",
                                        pline, variant
                                    ))
                                }
                                (VariantPayload::Single(pty), PatArgs::Single(b)) => {
                                    b.ty = pty.subst(targs);
                                }
                                (VariantPayload::Single(_), _) => {
                                    return Err(format!(
                                        "{}: variant '{}' wraps a value; bind it with \
                                         '{}.{}(name)' (use '_' to ignore it)",
                                        pline, variant, info.name, variant
                                    ))
                                }
                                (VariantPayload::Fields(decl), PatArgs::Fields(fields)) => {
                                    let mut fseen = vec![false; decl.len()];
                                    for (fname, b, index) in fields.iter_mut() {
                                        let j = match decl.iter().position(|(n, _)| n == fname)
                                        {
                                            Some(j) => j,
                                            None => {
                                                return Err(format!(
                                                    "{}: variant '{}' has no field '{}'",
                                                    pline, variant, fname
                                                ))
                                            }
                                        };
                                        if fseen[j] {
                                            return Err(format!(
                                                "{}: duplicate field '{}' in pattern",
                                                pline, fname
                                            ));
                                        }
                                        fseen[j] = true;
                                        *index = j as u32;
                                        b.ty = decl[j].1.subst(targs);
                                    }
                                    if let Some(j) = fseen.iter().position(|x| !x) {
                                        return Err(format!(
                                            "{}: pattern for variant '{}' is missing field \
                                             '{}' (bind it to '_' to ignore it)",
                                            pline, variant, decl[j].0
                                        ));
                                    }
                                }
                                (VariantPayload::Fields(_), _) => {
                                    return Err(format!(
                                        "{}: variant '{}' has named fields; match it with \
                                         '{}.{} {{ field: name, ... }}'",
                                        pline, variant, info.name, variant
                                    ))
                                }
                            }
                        }
                        Pattern::Wildcard { line: pline } => {
                            check_reachable(wildcard_at, *pline)?;
                            if seen.iter().all(|s| *s) {
                                return Err(format!(
                                    "{pline}: unreachable pattern: every variant is \
                                     already covered"
                                ));
                            }
                            wildcard_at = Some(*pline);
                        }
                        Pattern::IntLit { line: pline, .. }
                        | Pattern::ByteLit { line: pline, .. }
                        | Pattern::BoolLit { line: pline, .. } => return Err(mismatch(*pline)),
                    }
                }
                if wildcard_at.is_none() {
                    let missing: Vec<&str> = info
                        .variants
                        .iter()
                        .zip(&seen)
                        .filter(|(_, s)| !**s)
                        .map(|((n, _), _)| n.as_str())
                        .collect();
                    if !missing.is_empty() {
                        return Err(format!(
                            "{}: match on enum '{}' is not exhaustive; missing variant(s) {} \
                             (or add a '_' arm)",
                            line,
                            info.name,
                            missing.join(", ")
                        ));
                    }
                }
                Ok(())
            }
            Type::Int(k) => {
                let mut seen: Vec<u64> = Vec::new();
                for pat in pats.iter_mut() {
                    match &mut **pat {
                        Pattern::IntLit { neg, digits, value, line: pline } => {
                            check_reachable(wildcard_at, *pline)?;
                            let v = if *neg { -(*digits as i128) } else { *digits as i128 };
                            if !k.contains(v) {
                                return Err(format!(
                                    "{}: integer literal {} out of range for {}",
                                    pline,
                                    v,
                                    k.name()
                                ));
                            }
                            *value = v as i64 as u64;
                            if seen.contains(value) {
                                return Err(format!("{pline}: duplicate pattern {v}"));
                            }
                            seen.push(*value);
                        }
                        Pattern::ByteLit { value, line: pline } => {
                            check_reachable(wildcard_at, *pline)?;
                            if *k != IntKind::U8 {
                                return Err(mismatch(*pline));
                            }
                            let v = *value as u64;
                            if seen.contains(&v) {
                                return Err(format!("{pline}: duplicate pattern"));
                            }
                            seen.push(v);
                        }
                        Pattern::Wildcard { line: pline } => {
                            check_reachable(wildcard_at, *pline)?;
                            wildcard_at = Some(*pline);
                        }
                        Pattern::Variant { line: pline, .. }
                        | Pattern::BoolLit { line: pline, .. } => return Err(mismatch(*pline)),
                    }
                }
                if wildcard_at.is_none() {
                    return Err(format!(
                        "{}: match on {} needs a final '_' arm",
                        line,
                        k.name()
                    ));
                }
                Ok(())
            }
            Type::Bool => {
                let (mut t, mut f) = (false, false);
                for pat in pats.iter_mut() {
                    match &mut **pat {
                        Pattern::BoolLit { value, line: pline } => {
                            check_reachable(wildcard_at, *pline)?;
                            let seen = if *value { &mut t } else { &mut f };
                            if *seen {
                                return Err(format!("{pline}: duplicate pattern {value}"));
                            }
                            *seen = true;
                        }
                        Pattern::Wildcard { line: pline } => {
                            check_reachable(wildcard_at, *pline)?;
                            if t && f {
                                return Err(format!(
                                    "{pline}: unreachable pattern: both 'true' and 'false' \
                                     are already covered"
                                ));
                            }
                            wildcard_at = Some(*pline);
                        }
                        Pattern::Variant { line: pline, .. }
                        | Pattern::IntLit { line: pline, .. }
                        | Pattern::ByteLit { line: pline, .. } => return Err(mismatch(*pline)),
                    }
                }
                if wildcard_at.is_none() && !(t && f) {
                    return Err(format!(
                        "{line}: match on bool must cover both 'true' and 'false' \
                         (or add a '_' arm)"
                    ));
                }
                Ok(())
            }
            Type::Param(_) => Err(format!(
                "{}: cannot match on a value of generic type {}",
                line,
                self.show(scrut_ty)
            )),
            other => Err(format!(
                "{}: cannot match on a value of type {}; match works on enums, \
                 integers, and bools",
                line,
                self.show(other)
            )),
        }
    }

    fn check_cond(&mut self, cond: &mut Expr) -> CResult<()> {
        self.check_expr(cond, Some(&Type::Bool))?;
        if cond.ty != Type::Bool {
            return Err(format!(
                "{}: condition must be 'bool', found {}",
                cond.line,
                self.show(&cond.ty)
            ));
        }
        Ok(())
    }

    /// Operand admissibility for the arithmetic / bitwise / concatenation
    /// operators, given that both operands already have type `ty`. Shared by
    /// binary expressions and compound assignment.
    fn check_arith_op(&self, op: BinOp, ty: &Type, line: u32) -> CResult<()> {
        let ok = match op {
            BinOp::Add => matches!(ty, Type::Int(_) | Type::Float | Type::Str),
            BinOp::Sub | BinOp::Mul | BinOp::Div => matches!(ty, Type::Int(_) | Type::Float),
            BinOp::Rem
            | BinOp::BitAnd
            | BinOp::BitOr
            | BinOp::BitXor
            | BinOp::Shl
            | BinOp::Shr => matches!(ty, Type::Int(_)),
            _ => unreachable!("not an arithmetic operator"),
        };
        if ok {
            Ok(())
        } else if matches!(
            op,
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr
        ) {
            Err(format!(
                "{}: operator '{}' needs integer operands, found {}",
                line,
                op.sym(),
                self.show(ty)
            ))
        } else {
            Err(format!(
                "{}: invalid operand type {} for arithmetic",
                line,
                self.show(ty)
            ))
        }
    }

    fn require_assignable(&self, target: &Type, value: &Type, line: u32) -> CResult<()> {
        if target == value {
            Ok(())
        } else {
            Err(format!(
                "{}: type mismatch: expected {}, found {}",
                line,
                self.show(target),
                self.show(value)
            ))
        }
    }

    // -- Expressions --------------------------------------------------------

    fn check_expr(&mut self, e: &mut Expr, expected: Option<&Type>) -> CResult<()> {
        let line = e.line;
        match &mut e.kind {
            ExprKind::Int(digits) => {
                // Literals adopt the expected integer type (with a
                // compile-time range check); without context they are `int`.
                let kind = match expected {
                    Some(Type::Int(k)) => *k,
                    _ => IntKind::I64,
                };
                let v = *digits as i128;
                if !kind.contains(v) {
                    return Err(format!(
                        "{}: integer literal {} out of range for {}",
                        line,
                        v,
                        kind.name()
                    ));
                }
                e.ty = Type::Int(kind);
            }
            // Byte literals are exactly u8, regardless of context.
            ExprKind::Byte(_) => e.ty = Type::Int(IntKind::U8),
            ExprKind::Float(_) => e.ty = Type::Float,
            ExprKind::Bool(_) => e.ty = Type::Bool,
            ExprKind::Str(_) => e.ty = Type::Str,
            ExprKind::Var { name, res } => {
                let name = name.clone();
                if let Some((r, ty)) = self.resolve_var(&name) {
                    *res = Some(r);
                    e.ty = ty;
                } else if let Some(&fid) = self.func_ids.get(&name) {
                    let sig = &self.funcs[fid as usize];
                    if !sig.type_params.is_empty() {
                        return Err(format!(
                            "{line}: generic function '{name}' can only be called directly, \
                             not used as a value"
                        ));
                    }
                    *res = Some(VarRes::Func(fid));
                    e.ty = Type::Fn(sig.params.clone(), Box::new(sig.ret.clone()));
                } else if BUILTINS.iter().any(|(n, _)| *n == name) {
                    return Err(format!(
                        "{line}: builtin '{name}' can only be called, not used as a value"
                    ));
                } else if name == "self" {
                    return Err(format!(
                        "{line}: 'self' is only available inside impl-block methods"
                    ));
                } else if let Some(hint) = retired_hint(&name) {
                    return Err(format!("{line}: '{name}' is now a method: write {hint}"));
                } else {
                    return Err(format!("{line}: unknown variable '{name}'"));
                }
            }
            ExprKind::Unary(op, sub) => {
                // Fold `-literal` into a single literal so negative values
                // range-check against the expected type as a whole (this is
                // also the only way to write the most negative value of a
                // signed type, e.g. `let x: i8 = -128;`).
                if *op == UnOp::Neg {
                    if let ExprKind::Int(digits) = sub.kind {
                        let kind = match expected {
                            Some(Type::Int(k)) => *k,
                            _ => IntKind::I64,
                        };
                        let v = -(digits as i128);
                        if !kind.contains(v) {
                            return Err(format!(
                                "{}: integer literal {} out of range for {}",
                                line,
                                v,
                                kind.name()
                            ));
                        }
                        e.kind = ExprKind::Int(v as i64 as u64);
                        e.ty = Type::Int(kind);
                        return Ok(());
                    }
                }
                // `~`'s operand adopts the expected type, like arithmetic:
                // `let x: u8 = ~1;` checks the `1` at u8.
                let sub_expected = if *op == UnOp::BitNot { expected } else { None };
                self.check_expr(sub, sub_expected)?;
                match op {
                    UnOp::Neg => {
                        let ok = sub.ty == Type::Float
                            || sub.ty.int_kind().is_some_and(|k| k.signed());
                        if !ok {
                            return Err(format!(
                                "{}: operator '-' needs a signed int or float, found {}",
                                line,
                                self.show(&sub.ty)
                            ));
                        }
                        e.ty = sub.ty.clone();
                    }
                    UnOp::Not => {
                        if sub.ty != Type::Bool {
                            return Err(format!(
                                "{}: operator '!' needs bool, found {}",
                                line,
                                self.show(&sub.ty)
                            ));
                        }
                        e.ty = Type::Bool;
                    }
                    UnOp::BitNot => {
                        if sub.ty.int_kind().is_none() {
                            return Err(format!(
                                "{}: operator '~' needs an integer, found {}",
                                line,
                                self.show(&sub.ty)
                            ));
                        }
                        e.ty = sub.ty.clone();
                    }
                }
            }
            ExprKind::Binary(op, lhs, rhs) => {
                let op = *op;
                match op {
                    BinOp::And | BinOp::Or => {
                        self.check_expr(lhs, Some(&Type::Bool))?;
                        self.check_expr(rhs, Some(&Type::Bool))?;
                        for side in [&*lhs, &*rhs] {
                            if side.ty != Type::Bool {
                                return Err(format!(
                                    "{}: logical operator needs bool operands, found {}",
                                    line,
                                    self.show(&side.ty)
                                ));
                            }
                        }
                        e.ty = Type::Bool;
                    }
                    BinOp::Eq | BinOp::Ne => {
                        self.check_expr(lhs, None)?;
                        let lty = lhs.ty.clone();
                        self.check_expr(rhs, Some(&lty))?;
                        // Equality is semantically type-directed (strings
                        // compare by content, references by identity), so it
                        // cannot compile for an opaque type parameter.
                        if matches!(lhs.ty, Type::Param(_)) || matches!(rhs.ty, Type::Param(_)) {
                            return Err(format!(
                                "{}: cannot compare values of generic type {}; \
                                 '==' is not available on type parameters",
                                line,
                                self.show(if matches!(lhs.ty, Type::Param(_)) {
                                    &lhs.ty
                                } else {
                                    &rhs.ty
                                })
                            ));
                        }
                        if lhs.ty != rhs.ty || lhs.ty == Type::Unit {
                            return Err(format!(
                                "{}: cannot compare {} with {}",
                                line,
                                self.show(&lhs.ty),
                                self.show(&rhs.ty)
                            ));
                        }
                        // Bare-only enum values are shared static singletons,
                        // so reference identity *is* structural equality.
                        // Payload-carrying variants are freshly allocated,
                        // where identity would be a footgun.
                        for side in [&lhs.ty, &rhs.ty] {
                            if let Type::Enum(eid, _) = side {
                                let info = &self.enums[*eid as usize];
                                if !info.is_bare() {
                                    return Err(format!(
                                        "{}: '==' is not available on enum '{}' because \
                                         some variants carry values; use match",
                                        line, info.name
                                    ));
                                }
                            }
                        }
                        e.ty = Type::Bool;
                    }
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        self.check_expr(lhs, None)?;
                        self.check_expr(rhs, Some(&lhs.ty.clone()))?;
                        if lhs.ty != rhs.ty
                            || !matches!(lhs.ty, Type::Int(_) | Type::Float)
                        {
                            return Err(format!(
                                "{}: comparison needs two ints or two floats, found {} and {}",
                                line,
                                self.show(&lhs.ty),
                                self.show(&rhs.ty)
                            ));
                        }
                        e.ty = Type::Bool;
                    }
                    BinOp::Add
                    | BinOp::Sub
                    | BinOp::Mul
                    | BinOp::Div
                    | BinOp::Rem
                    | BinOp::BitAnd
                    | BinOp::BitOr
                    | BinOp::BitXor
                    | BinOp::Shl
                    | BinOp::Shr => {
                        // The expected type flows into the left operand (and
                        // from there to the right) so literals adopt the
                        // context: `let x: u8 = a + 1;` checks at u8.
                        self.check_expr(lhs, expected)?;
                        self.check_expr(rhs, Some(&lhs.ty.clone()))?;
                        if lhs.ty != rhs.ty {
                            return Err(format!(
                                "{}: arithmetic on mixed types {} and {}",
                                line,
                                self.show(&lhs.ty),
                                self.show(&rhs.ty)
                            ));
                        }
                        self.check_arith_op(op, &lhs.ty, line)?;
                        e.ty = lhs.ty.clone();
                    }
                }
            }
            ExprKind::If { cond, then, els } => {
                self.check_cond(cond)?;
                self.check_expr(then, expected)?;
                self.check_expr(els, Some(&then.ty.clone()))?;
                let ty = if then.ty == els.ty {
                    then.ty.clone()
                } else {
                    return Err(format!(
                        "{}: if-expression branches have different types: {} and {}",
                        line,
                        self.show(&then.ty),
                        self.show(&els.ty)
                    ));
                };
                if ty == Type::Unit {
                    return Err(format!(
                        "{line}: if-expression branches must produce a value"
                    ));
                }
                e.ty = ty;
            }
            ExprKind::Call { callee, args, direct, inst } => {
                // `Enum.Variant(value)` / `Type.assoc(...)`: the base name
                // resolves in the type namespace. Values shadow type names,
                // so this fires only when the base name resolves to no
                // local, capture, or function.
                let type_target = match &callee.kind {
                    ExprKind::Field { obj, name: mname, .. } => match &obj.kind {
                        ExprKind::Var { name: ename, .. }
                            if self.resolve_var(ename).is_none()
                                && !self.func_ids.contains_key(ename)
                                && (self.enum_ids.contains_key(ename)
                                    || self.struct_ids.contains_key(ename)) =>
                        {
                            Some((ename.clone(), mname.clone()))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((tname, mname)) = type_target {
                    if let Some(&eid) = self.enum_ids.get(&tname) {
                        // Variants take priority (and cannot collide with
                        // methods, checker-enforced).
                        if self.enums[eid as usize].variants.iter().any(|(n, _)| n == &mname) {
                            let args = std::mem::take(args);
                            return self.check_variant_lit(
                                e,
                                eid,
                                tname,
                                mname,
                                VariantCtor::Call(args),
                                expected,
                            );
                        }
                    }
                    let key = match self.struct_ids.get(&tname) {
                        Some(&sid) => TypeKey::Struct(sid),
                        None => TypeKey::Enum(self.enum_ids[&tname]),
                    };
                    let Some(&(fid, has_self)) = self.methods.get(&(key, mname.clone()))
                    else {
                        return Err(match key {
                            TypeKey::Enum(_) => format!(
                                "{line}: enum '{tname}' has no variant or associated \
                                 function '{mname}'"
                            ),
                            TypeKey::Struct(_) => format!(
                                "{line}: struct '{tname}' has no associated function \
                                 '{mname}'"
                            ),
                        });
                    };
                    if has_self {
                        return Err(format!(
                            "{line}: '{mname}' is a method; call it as 'value.{mname}(...)'"
                        ));
                    }
                    let what = format!("{tname}.{mname}");
                    let (fty, targs, rty) =
                        self.check_call_sig(&what, fid, args, expected, line, 0)?;
                    *direct = Some(fid);
                    *inst = targs;
                    callee.ty = fty;
                    e.ty = rty;
                    return Ok(());
                }
                // A call to a bare name may be a direct call or a builtin.
                if let ExprKind::Var { name, res } = &mut callee.kind {
                    let name = name.clone();
                    if let Some((r, ty)) = self.resolve_var(&name) {
                        *res = Some(r);
                        callee.ty = ty;
                    } else if let Some(&fid) = self.func_ids.get(&name) {
                        let (fty, targs, rty) =
                            self.check_call_sig(&name, fid, args, expected, line, 0)?;
                        *direct = Some(fid);
                        *inst = targs;
                        callee.ty = fty;
                        e.ty = rty;
                        return Ok(());
                    } else if let Some((_, b)) = BUILTINS.iter().find(|(n, _)| *n == name) {
                        let b = *b;
                        let args = std::mem::take(args);
                        let ty = self.check_builtin(b, &mut e.kind, args, line)?;
                        e.ty = ty;
                        return Ok(());
                    } else if let Some(hint) = retired_hint(&name) {
                        return Err(format!(
                            "{line}: '{name}' is now a method: write {hint}"
                        ));
                    } else {
                        return Err(format!("{line}: unknown function '{name}'"));
                    }
                } else if let ExprKind::Field { obj, name: mname, .. } = &mut callee.kind {
                    // `expr.name(args)`: a method call, a builtin method, or
                    // a call through a function-valued field. The receiver
                    // is checked exactly once, here.
                    self.check_expr(obj, None)?;
                    let recv_ty = obj.ty.clone();
                    if let Some((fid, has_self)) = self.lookup_method(&recv_ty, mname) {
                        if !has_self {
                            return Err(format!(
                                "{line}: '{mname}' is an associated function; call it \
                                 as '{}.{mname}(...)'",
                                self.type_base_name(&recv_ty)
                            ));
                        }
                        let mname = mname.clone();
                        let recv =
                            std::mem::replace(&mut **obj, Expr::new(ExprKind::Bool(false), line));
                        args.insert(0, recv);
                        let (fty, targs, rty) =
                            self.check_call_sig(&mname, fid, args, expected, line, 1)?;
                        // The callee expression is dead for a direct call;
                        // leave a resolution-free placeholder.
                        callee.kind = ExprKind::Var { name: mname, res: None };
                        callee.ty = fty;
                        *direct = Some(fid);
                        *inst = targs;
                        e.ty = rty;
                        return Ok(());
                    }
                    if let Some(b) = builtin_method(&recv_ty, mname, self.option_enum) {
                        let recv =
                            std::mem::replace(&mut **obj, Expr::new(ExprKind::Bool(false), line));
                        let mut margs = std::mem::take(args);
                        margs.insert(0, recv);
                        let ty = self.check_builtin_method(b, &mut e.kind, margs, line)?;
                        e.ty = ty;
                        return Ok(());
                    }
                    // Fall through: an ordinary field access (the field may
                    // hold a function value). Reports no-such-method for
                    // everything else.
                    self.check_field_after_obj(callee)?;
                } else {
                    self.check_expr(callee, None)?;
                }
                // Indirect call through a function value.
                match callee.ty.clone() {
                    Type::Fn(params, ret) => {
                        self.check_args("function value", &params, args, line, 0)?;
                        e.ty = *ret;
                    }
                    other => {
                        return Err(format!(
                            "{}: cannot call a value of type {}",
                            line,
                            self.show(&other)
                        ))
                    }
                }
            }
            ExprKind::Builtin(..) => unreachable!("builtins are created by the checker"),
            ExprKind::BoundMethod { .. } => {
                unreachable!("bound methods are created by the checker")
            }
            ExprKind::VariantLit { .. } => {
                unreachable!("variant literals are created by the checker")
            }
            ExprKind::VariantStructLit { enum_name, variant, fields } => {
                // Like a struct literal, the enum name resolves in the type
                // namespace directly.
                let Some(&eid) = self.enum_ids.get(enum_name.as_str()) else {
                    return Err(format!("{line}: unknown enum '{enum_name}'"));
                };
                let (enum_name, variant) = (enum_name.clone(), variant.clone());
                let fields = std::mem::take(fields);
                return self.check_variant_lit(
                    e,
                    eid,
                    enum_name,
                    variant,
                    VariantCtor::Fields(fields),
                    expected,
                );
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_expr(scrutinee, None)?;
                let scrut_ty = scrutinee.ty.clone();
                {
                    let mut pats: Vec<&mut Pattern> = arms.iter_mut().map(|(p, _)| p).collect();
                    self.check_patterns(&scrut_ty, &mut pats, line)?;
                }
                // Arms must agree on one result type; the first arm sees
                // the outer expectation and pins it for the rest.
                let mut rty: Option<Type> = None;
                for (pat, body) in arms.iter_mut() {
                    self.ctxs.last_mut().unwrap().scopes.push(Vec::new());
                    self.declare_pattern_binders(pat)?;
                    let want = match &rty {
                        Some(t) => Some(t.clone()),
                        None => expected.cloned(),
                    };
                    self.check_expr(body, want.as_ref())?;
                    self.ctxs.last_mut().unwrap().scopes.pop();
                    rty = Some(match rty {
                        None => body.ty.clone(),
                        Some(t) if t == body.ty => t,
                        Some(t) => {
                            return Err(format!(
                                "{}: match arms have different types: {} and {}",
                                body.line,
                                self.show(&t),
                                self.show(&body.ty)
                            ))
                        }
                    });
                }
                let ty = rty.expect("exhaustiveness guarantees at least one arm");
                if ty == Type::Unit {
                    return Err(format!("{line}: match arms must produce a value"));
                }
                e.ty = ty;
            }
            ExprKind::Field { obj, name, .. } => {
                // `Enum.Variant` (a bare variant) or a type-name diagnostic,
                // unless a value shadows the type name (then it is an
                // ordinary field access).
                let type_target = match &obj.kind {
                    ExprKind::Var { name: ename, .. }
                        if self.resolve_var(ename).is_none()
                            && !self.func_ids.contains_key(ename)
                            && (self.enum_ids.contains_key(ename)
                                || self.struct_ids.contains_key(ename)) =>
                    {
                        Some(ename.clone())
                    }
                    _ => None,
                };
                if let Some(tname) = type_target {
                    if let Some(&eid) = self.enum_ids.get(&tname) {
                        if self.enums[eid as usize].variants.iter().any(|(n, _)| n == name) {
                            let vname = name.clone();
                            return self.check_variant_lit(
                                e,
                                eid,
                                tname,
                                vname,
                                VariantCtor::Bare,
                                expected,
                            );
                        }
                    }
                    let key = match self.struct_ids.get(&tname) {
                        Some(&sid) => TypeKey::Struct(sid),
                        None => TypeKey::Enum(self.enum_ids[&tname]),
                    };
                    // A type-qualified name in value position: methods and
                    // associated functions are call-only.
                    return Err(if self.methods.contains_key(&(key, name.clone())) {
                        format!(
                            "{line}: '{tname}.{name}' can only be called, not used as a value"
                        )
                    } else if matches!(key, TypeKey::Enum(_)) {
                        format!("{line}: enum '{tname}' has no variant '{name}'")
                    } else {
                        format!("{line}: struct '{tname}' has no associated function '{name}'")
                    });
                }
                self.check_expr(obj, None)?;
                // A method named in value position becomes a *bound method*:
                // a closure capturing the receiver. Field names cannot
                // collide with method names (checker-enforced), so order is
                // immaterial; fields are tried first as the common case.
                if let Type::Struct(sid, _) = &obj.ty {
                    let has_field = self.structs[*sid as usize]
                        .fields
                        .iter()
                        .any(|(n, _)| n == name);
                    if !has_field {
                        if let Some((fid, has_self)) = self.lookup_method(&obj.ty, name) {
                            if !has_self {
                                return Err(format!(
                                    "{line}: '{}.{name}' can only be called, not used as \
                                     a value",
                                    self.type_base_name(&obj.ty)
                                ));
                            }
                            return self.check_bound_method(e, fid, line);
                        }
                    }
                } else if matches!(obj.ty, Type::Enum(..)) {
                    if let Some((fid, has_self)) = self.lookup_method(&obj.ty, name) {
                        if !has_self {
                            return Err(format!(
                                "{line}: '{}.{name}' can only be called, not used as a value",
                                self.type_base_name(&obj.ty)
                            ));
                        }
                        return self.check_bound_method(e, fid, line);
                    }
                }
                if builtin_method(&obj.ty, name, self.option_enum).is_some() {
                    return Err(format!(
                        "{line}: builtin method '{name}' can only be called, not used as \
                         a value"
                    ));
                }
                self.check_field_after_obj(e)?;
            }
            ExprKind::Index(obj, idx) => {
                self.check_expr(obj, None)?;
                self.check_expr(idx, Some(&INT))?;
                if idx.ty != INT {
                    return Err(format!(
                        "{}: array index must be int, found {}",
                        line,
                        self.show(&idx.ty)
                    ));
                }
                match obj.ty.clone() {
                    Type::Array(elem) => e.ty = *elem,
                    // Byte access into a string: `s[i]` is the i-th byte, u8.
                    Type::Str => e.ty = Type::Int(IntKind::U8),
                    other => {
                        return Err(format!(
                            "{}: cannot index a value of type {}",
                            line,
                            self.show(&other)
                        ))
                    }
                }
            }
            ExprKind::Cast(sub, te) => {
                let target = self.resolve_type(te)?;
                if !matches!(target, Type::Int(_) | Type::Float) {
                    return Err(format!(
                        "{}: cast target must be a numeric type, found {}",
                        line,
                        self.show(&target)
                    ));
                }
                // A directly cast literal adopts the target (so `300 as u8`
                // is a compile-time range error); compound expressions are
                // inferred on their own and range-checked at runtime.
                let literal = matches!(sub.kind, ExprKind::Int(_))
                    || matches!(&sub.kind, ExprKind::Unary(UnOp::Neg, inner)
                        if matches!(inner.kind, ExprKind::Int(_)));
                let want = if literal { Some(&target) } else { None };
                self.check_expr(sub, want)?;
                if !matches!(sub.ty, Type::Int(_) | Type::Float) {
                    return Err(format!(
                        "{}: cannot cast {} to {}",
                        line,
                        self.show(&sub.ty),
                        self.show(&target)
                    ));
                }
                e.ty = target;
            }
            ExprKind::ArrayLit(elems) => {
                let want_elem = match expected {
                    Some(Type::Array(elem)) => Some((**elem).clone()),
                    _ => None,
                };
                let mut elem_ty = want_elem;
                for el in elems.iter_mut() {
                    self.check_expr(el, elem_ty.as_ref())?;
                    match &elem_ty {
                        None => elem_ty = Some(el.ty.clone()),
                        Some(t) => self.require_assignable(t, &el.ty, el.line)?,
                    }
                }
                match elem_ty {
                    Some(t) => e.ty = Type::Array(Box::new(t)),
                    None => {
                        return Err(format!(
                            "{line}: cannot infer the type of an empty array; add a type annotation"
                        ))
                    }
                }
            }
            ExprKind::StructLit { name, fields, struct_id } => {
                let sid = match self.struct_ids.get(name) {
                    Some(&id) => id,
                    None => return Err(format!("{line}: unknown struct '{name}'")),
                };
                *struct_id = sid;
                let decl: Vec<(String, Type)> = self.structs[sid as usize].fields.clone();
                let tparams = self.structs[sid as usize].type_params.clone();
                // Type arguments are inferred from the field values, seeded
                // from the expected type (e.g. a `let` annotation).
                let mut solved: Vec<Option<Type>> = vec![None; tparams.len()];
                if let Some(Type::Struct(esid, etargs)) = expected {
                    if *esid == sid && etargs.len() == tparams.len() {
                        solved = etargs.iter().map(|t| Some(t.clone())).collect();
                    }
                }
                let mut seen = vec![false; decl.len()];
                for (fname, value, index) in fields.iter_mut() {
                    let i = match decl.iter().position(|(n, _)| n == fname) {
                        Some(i) => i,
                        None => {
                            return Err(format!(
                                "{}: struct '{}' has no field '{}'",
                                value.line, name, fname
                            ))
                        }
                    };
                    if seen[i] {
                        return Err(format!("{}: duplicate field '{}'", value.line, fname));
                    }
                    seen[i] = true;
                    *index = i as u32;
                    let want = subst_partial(&decl[i].1, &solved);
                    if !want.has_param() {
                        self.check_expr(value, Some(&want))?;
                        self.require_assignable(&want, &value.ty, value.line)?;
                    } else {
                        self.check_expr(value, None)?;
                        if unify(&decl[i].1, &value.ty, &mut solved).is_err() {
                            return Err(format!(
                                "{}: type mismatch: expected {}, found {}",
                                value.line,
                                self.show(&subst_partial(&decl[i].1, &solved)),
                                self.show(&value.ty)
                            ));
                        }
                    }
                }
                if let Some(i) = seen.iter().position(|s| !s) {
                    return Err(format!(
                        "{}: missing field '{}' in struct literal '{}'",
                        line, decl[i].0, name
                    ));
                }
                if let Some(i) = solved.iter().position(Option::is_none) {
                    return Err(format!(
                        "{line}: cannot infer type parameter '{}' of struct '{name}'; \
                         annotate the target type",
                        tparams[i]
                    ));
                }
                e.ty = Type::Struct(sid, solved.into_iter().map(Option::unwrap).collect());
            }
            ExprKind::Lambda(lam) => {
                let mut params = Vec::new();
                for (_, pty) in &lam.params {
                    params.push(self.resolve_type(pty)?);
                }
                let ret = match &lam.ret {
                    Some(t) => self.resolve_type(t)?,
                    None => Type::Unit,
                };
                lam.id = self.lambda_sigs.len() as u32;
                self.lambda_sigs.push(None);
                self.lambda_locals.push(None);
                let id = lam.id as usize;

                self.ctxs.push(FnCtx {
                    scopes: vec![Vec::new()],
                    locals: Vec::new(),
                    captures: Some(Vec::new()),
                    ret: ret.clone(),
                });
                let saved_loop_depth = std::mem::take(&mut self.loop_depth);
                for ((pname, _), pty) in lam.params.iter().zip(&params) {
                    self.declare_local(pname, pty.clone(), lam.line)?;
                }
                self.check_block(&mut lam.body)?;
                self.loop_depth = saved_loop_depth;
                if ret != Type::Unit && !always_returns(&lam.body) {
                    return Err(format!(
                        "{}: lambda must return a value on all paths",
                        lam.line
                    ));
                }
                let ctx = self.ctxs.pop().unwrap();
                lam.num_locals = ctx.locals.len() as u32;
                lam.captures = ctx.captures.unwrap();
                if lam.captures.len() > 63 {
                    return Err(format!("{}: lambda captures too many variables", lam.line));
                }
                self.lambda_locals[id] = Some(ctx.locals);
                self.lambda_sigs[id] = Some((params.clone(), ret.clone()));
                e.ty = Type::Fn(params, Box::new(ret));
            }
        }
        Ok(())
    }

    /// Check one payload value against its declared (possibly generic)
    /// type, contributing type-argument bindings to `solved`.
    fn check_payload_value(
        &mut self,
        decl: &Type,
        value: &mut Expr,
        solved: &mut [Option<Type>],
    ) -> CResult<()> {
        let want = subst_partial(decl, solved);
        if !want.has_param() {
            self.check_expr(value, Some(&want))?;
            self.require_assignable(&want, &value.ty, value.line)?;
        } else {
            self.check_expr(value, None)?;
            if unify(decl, &value.ty, solved).is_err() {
                return Err(format!(
                    "{}: type mismatch: expected {}, found {}",
                    value.line,
                    self.show(&subst_partial(decl, solved)),
                    self.show(&value.ty)
                ));
            }
        }
        Ok(())
    }

    /// Check a variant construction — bare, `(value)`, or
    /// `{ field: value, ... }` — rewriting `e` into a `VariantLit`. The
    /// construction's shape must mirror the declaration's, and type
    /// arguments are inferred from the payload values, seeded from the
    /// expected type.
    fn check_variant_lit(
        &mut self,
        e: &mut Expr,
        eid: u32,
        enum_name: String,
        variant: String,
        ctor: VariantCtor,
        expected: Option<&Type>,
    ) -> CResult<()> {
        let line = e.line;
        let info = &self.enums[eid as usize];
        let tag = match info.variants.iter().position(|(n, _)| n == &variant) {
            Some(i) => i,
            None => {
                return Err(format!(
                    "{}: enum '{}' has no variant '{}'",
                    line, info.name, variant
                ))
            }
        };
        let payload = info.variants[tag].1.clone();
        let tparams = info.type_params.clone();
        let mut solved: Vec<Option<Type>> = vec![None; tparams.len()];
        if let Some(Type::Enum(esid, etargs)) = expected {
            if *esid == eid && etargs.len() == tparams.len() {
                solved = etargs.iter().map(|t| Some(t.clone())).collect();
            }
        }
        let args = match (payload, ctor) {
            (VariantPayload::Bare, VariantCtor::Bare) => VariantArgs::Bare,
            (VariantPayload::Bare, _) => {
                return Err(format!(
                    "{line}: variant '{variant}' is bare; write '{enum_name}.{variant}' \
                     without arguments"
                ))
            }
            (VariantPayload::Single(pty), VariantCtor::Call(mut a)) => {
                if a.len() != 1 {
                    return Err(format!(
                        "{}: variant '{}' wraps exactly one value, got {}",
                        line,
                        variant,
                        a.len()
                    ));
                }
                let mut arg = a.pop().unwrap();
                self.check_payload_value(&pty, &mut arg, &mut solved)?;
                VariantArgs::Single(Box::new(arg))
            }
            (VariantPayload::Single(_), _) => {
                return Err(format!(
                    "{line}: variant '{variant}' wraps a value; construct it as \
                     '{enum_name}.{variant}(value)'"
                ))
            }
            (VariantPayload::Fields(decl), VariantCtor::Fields(mut fields)) => {
                let mut seen = vec![false; decl.len()];
                for (fname, value, index) in fields.iter_mut() {
                    let i = match decl.iter().position(|(n, _)| n == fname) {
                        Some(i) => i,
                        None => {
                            return Err(format!(
                                "{}: variant '{}' has no field '{}'",
                                value.line, variant, fname
                            ))
                        }
                    };
                    if seen[i] {
                        return Err(format!("{}: duplicate field '{}'", value.line, fname));
                    }
                    seen[i] = true;
                    *index = i as u32;
                    let pty = decl[i].1.clone();
                    self.check_payload_value(&pty, value, &mut solved)?;
                }
                if let Some(i) = seen.iter().position(|s| !s) {
                    return Err(format!(
                        "{}: missing field '{}' in variant literal '{}.{}'",
                        line, decl[i].0, enum_name, variant
                    ));
                }
                VariantArgs::Fields(fields)
            }
            (VariantPayload::Fields(_), _) => {
                return Err(format!(
                    "{line}: variant '{variant}' has named fields; construct it as \
                     '{enum_name}.{variant} {{ field: value, ... }}'"
                ))
            }
        };
        if let Some(i) = solved.iter().position(Option::is_none) {
            return Err(format!(
                "{line}: cannot infer type parameter '{}' of enum '{enum_name}'; \
                 annotate the target type",
                tparams[i]
            ));
        }
        e.ty = Type::Enum(eid, solved.into_iter().map(Option::unwrap).collect());
        e.kind = ExprKind::VariantLit { args, enum_id: eid, tag: tag as u32 };
        Ok(())
    }

    /// The bare declaration name of a struct or enum type, for diagnostics.
    fn type_base_name(&self, t: &Type) -> &str {
        match t {
            Type::Struct(id, _) => &self.structs[*id as usize].name,
            Type::Enum(id, _) => &self.enums[*id as usize].name,
            _ => unreachable!("only nominal types have methods"),
        }
    }

    /// Resolve `e` (a `Field` whose object is already checked) as a plain
    /// struct field access, or report that no field or method exists.
    fn check_field_after_obj(&mut self, e: &mut Expr) -> CResult<()> {
        let line = e.line;
        let ExprKind::Field { obj, name, index } = &mut e.kind else {
            unreachable!("caller matched a field access")
        };
        match obj.ty.clone() {
            Type::Struct(sid, targs) => {
                let info = &self.structs[sid as usize];
                match info.fields.iter().position(|(n, _)| n == name) {
                    Some(i) => {
                        *index = i as u32;
                        // Field types live in the struct's own parameter
                        // space; map into the caller's.
                        e.ty = info.fields[i].1.subst(&targs);
                    }
                    None => {
                        return Err(format!(
                            "{}: struct '{}' has no field or method '{}'",
                            line, info.name, name
                        ))
                    }
                }
            }
            Type::Enum(eid, _) => {
                return Err(format!(
                    "{}: enum '{}' has no method '{}'",
                    line, self.enums[eid as usize].name, name
                ))
            }
            other => {
                return Err(format!(
                    "{}: type {} has no method '{}'",
                    line,
                    self.show(&other),
                    name
                ))
            }
        }
        Ok(())
    }

    /// Rewrite `e` (a `Field` naming a method, object already checked) into
    /// a bound method: a first-class closure capturing the receiver. Every
    /// type parameter must be determined by the receiver type, so a method
    /// with its own type parameters cannot be bound.
    fn check_bound_method(&mut self, e: &mut Expr, fid: u32, line: u32) -> CResult<()> {
        let ExprKind::Field { obj, name, .. } = &mut e.kind else {
            unreachable!("caller matched a field access")
        };
        let (tparams, params, ret) = {
            let sig = &self.funcs[fid as usize];
            (sig.type_params.clone(), sig.params.clone(), sig.ret.clone())
        };
        let mut solved: Vec<Option<Type>> = vec![None; tparams.len()];
        unify(&params[0], &obj.ty, &mut solved)
            .expect("receiver type matches the method's impl type by construction");
        if let Some(i) = solved.iter().position(Option::is_none) {
            return Err(format!(
                "{}: cannot infer type parameter '{}' of method '{}'; a method with its \
                 own type parameters can only be called, not bound as a value",
                line, tparams[i], name
            ));
        }
        let targs: Vec<Type> = solved.into_iter().map(Option::unwrap).collect();
        let bound_params: Vec<Type> = params[1..].iter().map(|t| t.subst(&targs)).collect();
        let bound_ret = ret.subst(&targs);
        let obj = std::mem::replace(&mut **obj, Expr::new(ExprKind::Bool(false), line));
        e.ty = Type::Fn(bound_params, Box::new(bound_ret));
        e.kind = ExprKind::BoundMethod { obj: Box::new(obj), fid, inst: targs };
        Ok(())
    }

    /// Check a builtin method call. `args[0]` is the receiver, already
    /// checked and already matched to `b` by `builtin_method`; the remaining
    /// arguments are checked here. Rewrites `kind` into `Builtin`.
    fn check_builtin_method(
        &mut self,
        b: Builtin,
        kind: &mut ExprKind,
        mut args: Vec<Expr>,
        line: u32,
    ) -> CResult<Type> {
        let surface = match b {
            Builtin::Len => "len",
            Builtin::Push => "push",
            Builtin::Pop => "pop",
            Builtin::Itos | Builtin::Ftos | Builtin::Btos => "to_string",
            Builtin::Itof | Builtin::Stof => "to_float",
            Builtin::Stoi => "to_int",
            Builtin::Stob => "to_bytes",
            Builtin::Unwrap => "unwrap",
            _ => unreachable!("not a builtin method"),
        };
        let argc = |n: usize| -> CResult<()> {
            if args.len() - 1 != n {
                Err(format!(
                    "{line}: method '{surface}' expects {n} argument(s), got {}",
                    args.len() - 1
                ))
            } else {
                Ok(())
            }
        };
        let ty = match b {
            Builtin::Len => {
                argc(0)?;
                INT
            }
            Builtin::Push => {
                argc(1)?;
                let elem = match args[0].ty.clone() {
                    Type::Array(elem) => *elem,
                    _ => unreachable!("dispatched on an array receiver"),
                };
                self.check_expr(&mut args[1], Some(&elem))?;
                self.require_assignable(&elem, &args[1].ty, line)?;
                Type::Unit
            }
            Builtin::Pop => {
                argc(0)?;
                match args[0].ty.clone() {
                    Type::Array(elem) => *elem,
                    _ => unreachable!("dispatched on an array receiver"),
                }
            }
            Builtin::Itos | Builtin::Ftos => {
                argc(0)?;
                Type::Str
            }
            Builtin::Itof => {
                argc(0)?;
                Type::Float
            }
            Builtin::Stoi => {
                argc(0)?;
                INT
            }
            Builtin::Stof => {
                argc(0)?;
                Type::Float
            }
            Builtin::Stob => {
                argc(0)?;
                Type::Array(Box::new(Type::Int(IntKind::U8)))
            }
            Builtin::Btos => {
                argc(0)?;
                if args[0].ty != Type::Array(Box::new(Type::Int(IntKind::U8))) {
                    return Err(format!(
                        "{}: to_string() needs a [u8], found {}",
                        line,
                        self.show(&args[0].ty)
                    ));
                }
                Type::Str
            }
            Builtin::Unwrap => {
                argc(0)?;
                match &args[0].ty {
                    Type::Enum(_, targs) => targs[0].clone(),
                    _ => unreachable!("dispatched on an Option receiver"),
                }
            }
            _ => unreachable!("not a builtin method"),
        };
        *kind = ExprKind::Builtin(b, args);
        Ok(ty)
    }

    fn check_args(
        &mut self,
        what: &str,
        params: &[Type],
        args: &mut [Expr],
        line: u32,
        checked_prefix: usize,
    ) -> CResult<()> {
        if params.len() != args.len() {
            return Err(format!(
                "{}: '{}' expects {} argument(s), got {}",
                line,
                what,
                params.len(),
                args.len()
            ));
        }
        for (i, (arg, pty)) in args.iter_mut().zip(params).enumerate() {
            // The first `checked_prefix` arguments (a method receiver) were
            // already checked; re-checking would re-run checker rewrites.
            if i >= checked_prefix {
                self.check_expr(arg, Some(pty))?;
            }
            self.require_assignable(pty, &arg.ty, arg.line)?;
        }
        Ok(())
    }

    /// Finish a call to the top-level function `fid`: check the (full)
    /// argument list against its signature, inferring type arguments when it
    /// is generic. `skip` hides a method receiver from arity diagnostics and
    /// marks it as already checked. Returns the callee's concrete function
    /// type, the inferred type arguments, and the result type.
    fn check_call_sig(
        &mut self,
        what: &str,
        fid: u32,
        args: &mut [Expr],
        expected: Option<&Type>,
        line: u32,
        skip: usize,
    ) -> CResult<(Type, Vec<Type>, Type)> {
        let (tparams, params, ret) = {
            let sig = &self.funcs[fid as usize];
            (sig.type_params.clone(), sig.params.clone(), sig.ret.clone())
        };
        if params.len() != args.len() {
            return Err(format!(
                "{}: '{}' expects {} argument(s), got {}",
                line,
                what,
                params.len() - skip,
                args.len() - skip
            ));
        }
        if tparams.is_empty() {
            self.check_args(what, &params, args, line, skip)?;
            let rty = ret.clone();
            Ok((Type::Fn(params, Box::new(ret)), Vec::new(), rty))
        } else {
            let targs =
                self.infer_call(what, &tparams, &params, args, expected, &ret, line, skip)?;
            let fty = Type::Fn(
                params.iter().map(|p| p.subst(&targs)).collect(),
                Box::new(ret.subst(&targs)),
            );
            let rty = ret.subst(&targs);
            Ok((fty, targs, rty))
        }
    }

    /// Check a call to a generic function, inferring its type arguments.
    /// The expected return type (if any) is unified first, so parameters
    /// that only occur in the return type can still be solved; arguments are
    /// then processed left to right, and each one either checks against the
    /// already-solved expectation or contributes new bindings.
    #[allow(clippy::too_many_arguments)]
    fn infer_call(
        &mut self,
        what: &str,
        tparams: &[String],
        params: &[Type],
        args: &mut [Expr],
        expected: Option<&Type>,
        ret: &Type,
        line: u32,
        checked_prefix: usize,
    ) -> CResult<Vec<Type>> {
        if params.len() != args.len() {
            return Err(format!(
                "{}: '{}' expects {} argument(s), got {}",
                line,
                what,
                params.len(),
                args.len()
            ));
        }
        let mut solved: Vec<Option<Type>> = vec![None; tparams.len()];
        if let Some(exp) = expected {
            // Best effort: an unrelated expected type must not poison the
            // bindings, so keep them only when the whole return type unifies.
            let mut trial = solved.clone();
            if unify(ret, exp, &mut trial).is_ok() {
                solved = trial;
            }
        }
        for (i, (arg, pty)) in args.iter_mut().zip(params).enumerate() {
            let want = subst_partial(pty, &solved);
            if !want.has_param() {
                if i >= checked_prefix {
                    self.check_expr(arg, Some(&want))?;
                }
                self.require_assignable(&want, &arg.ty, arg.line)?;
            } else {
                if i >= checked_prefix {
                    self.check_expr(arg, None)?;
                }
                if unify(pty, &arg.ty, &mut solved).is_err() {
                    return Err(format!(
                        "{}: type mismatch: expected {}, found {}",
                        arg.line,
                        self.show(&subst_partial(pty, &solved)),
                        self.show(&arg.ty)
                    ));
                }
            }
        }
        if let Some(i) = solved.iter().position(Option::is_none) {
            return Err(format!(
                "{line}: cannot infer type parameter '{}' of '{what}'; \
                 annotate the surrounding context",
                tparams[i]
            ));
        }
        Ok(solved.into_iter().map(Option::unwrap).collect())
    }

    /// Check a call to one of the remaining free-function builtins
    /// (`println`, `print`, `assert`, `gc_collect`); everything with a
    /// receiver is a builtin *method* (`check_builtin_method`).
    fn check_builtin(
        &mut self,
        b: Builtin,
        kind: &mut ExprKind,
        mut args: Vec<Expr>,
        line: u32,
    ) -> CResult<Type> {
        let argc = |n: usize| -> CResult<()> {
            if args.len() != n {
                Err(format!("{line}: builtin expects {n} argument(s), got {}", args.len()))
            } else {
                Ok(())
            }
        };
        let ty = match b {
            Builtin::Println | Builtin::Print => {
                argc(1)?;
                self.check_expr(&mut args[0], None)?;
                match args[0].ty {
                    Type::Int(_) | Type::Float | Type::Bool | Type::Str => {}
                    _ => {
                        return Err(format!(
                            "{}: cannot print a value of type {}",
                            line,
                            self.show(&args[0].ty)
                        ))
                    }
                }
                Type::Unit
            }
            Builtin::Assert => {
                argc(1)?;
                self.check_expr(&mut args[0], Some(&Type::Bool))?;
                self.require_assignable(&Type::Bool, &args[0].ty, line)?;
                Type::Unit
            }
            Builtin::GcCollect => {
                argc(0)?;
                Type::Unit
            }
            _ => unreachable!("receiver builtins are resolved as methods"),
        };
        *kind = ExprKind::Builtin(b, args);
        Ok(ty)
    }
}

/// Which builtin (if any) the method name `name` denotes on a receiver of
/// type `recv`. User-defined methods are looked up first by the caller, so
/// e.g. a user `unwrap` on a shadowing `Option` wins.
fn builtin_method(recv: &Type, name: &str, option_enum: Option<u32>) -> Option<Builtin> {
    Some(match (recv, name) {
        (Type::Array(_), "len") => Builtin::Len,
        (Type::Array(_), "push") => Builtin::Push,
        (Type::Array(_), "pop") => Builtin::Pop,
        (Type::Array(_), "to_string") => Builtin::Btos,
        (Type::Str, "len") => Builtin::Len,
        (Type::Str, "to_int") => Builtin::Stoi,
        (Type::Str, "to_float") => Builtin::Stof,
        (Type::Str, "to_bytes") => Builtin::Stob,
        (Type::Int(_), "to_string") => Builtin::Itos,
        (Type::Int(_), "to_float") => Builtin::Itof,
        (Type::Float, "to_string") => Builtin::Ftos,
        (Type::Enum(eid, _), "unwrap") if Some(*eid) == option_enum => Builtin::Unwrap,
        _ => return None,
    })
}

/// Conservative "all paths return" analysis.
fn always_returns(block: &Block) -> bool {
    block.stmts.iter().any(stmt_always_returns)
}

fn stmt_always_returns(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return { .. } => true,
        Stmt::If { then, els: Some(els), .. } => always_returns(then) && always_returns(els),
        // Match arms are exhaustive (checker-enforced), so all arms
        // returning means every path returns.
        Stmt::Match { arms, .. } => {
            !arms.is_empty() && arms.iter().all(|(_, b)| always_returns(b))
        }
        Stmt::Block(b) => always_returns(b),
        _ => false,
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;

    pub fn check_src(src: &str) -> Result<(Program, Checked), String> {
        let toks = crate::lexer::lex(src).expect("lex error in test");
        let mut program = crate::parser::parse(toks).expect("parse error in test");
        let checked = check(&mut program)?;
        Ok((program, checked))
    }

    pub fn err(src: &str) -> String {
        check_src(src).map(|_| ()).expect_err("expected a type error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::tests_support::*;

    /// Wrap a snippet in `fn main() { ... }`.
    fn err_in_main(body: &str) -> String {
        err(&format!("fn main() {{ {body} }}"))
    }

    #[test]
    fn local_inference() {
        let (_, checked) = check_src(
            r#"fn main() {
                let a = 1;
                let b = 1.5;
                let c = true;
                let d = "s";
                let e = [1, 2];
                let f = fn(x: int): int { return x; };
                let g: [string] = [];
            }"#,
        )
        .unwrap();
        let main_locals = &checked.func_locals[0];
        assert_eq!(main_locals[0], INT);
        assert_eq!(main_locals[1], Type::Float);
        assert_eq!(main_locals[2], Type::Bool);
        assert_eq!(main_locals[3], Type::Str);
        assert_eq!(main_locals[4], Type::Array(Box::new(INT)));
        assert_eq!(main_locals[5], Type::Fn(vec![INT], Box::new(INT)));
        assert_eq!(main_locals[6], Type::Array(Box::new(Type::Str)));
    }

    #[test]
    fn sized_int_inference() {
        let (_, checked) = check_src(
            r#"fn main() {
                let a: u8 = 255;
                let b: i8 = -128;
                let c: i64 = 1;
                let d: int = c;
                let e: u64 = 18446744073709551615;
                let f = a as u32;
                let g: [u16] = [1, 2];
            }"#,
        )
        .unwrap();
        let locals = &checked.func_locals[0];
        assert_eq!(locals[0], Type::Int(IntKind::U8));
        assert_eq!(locals[1], Type::Int(IntKind::I8));
        assert_eq!(locals[2], INT); // i64 is an alias of int
        assert_eq!(locals[3], INT);
        assert_eq!(locals[4], Type::Int(IntKind::U64));
        assert_eq!(locals[5], Type::Int(IntKind::U32));
        assert_eq!(locals[6], Type::Array(Box::new(Type::Int(IntKind::U16))));
    }

    #[test]
    fn sized_int_errors() {
        assert!(err_in_main("let x: u8 = 256;").contains("out of range for u8"));
        assert!(err_in_main("let x: i8 = -129;").contains("out of range for i8"));
        assert!(err_in_main("let x = 18446744073709551615;").contains("out of range for int"));
        assert!(err_in_main("let a: u8 = 1; let b: u16 = 2; let c = a + b;")
            .contains("arithmetic on mixed types u8 and u16"));
        assert!(err_in_main("let a: u8 = 1; let b = -a;").contains("needs a signed int"));
        assert!(err_in_main("let s = \"x\" as int;").contains("cannot cast string to int"));
        assert!(err_in_main("let x = 1 as string;").contains("cast target must be a numeric"));
        assert!(err_in_main("let x = 300 as u8;").contains("out of range for u8"));
        assert!(err("struct u8 { x: int } fn main() { }").contains("shadows a primitive type"));
        assert!(err("struct i64 { x: int } fn main() { }").contains("shadows a primitive type"));
    }

    #[test]
    fn capture_analysis() {
        let (program, _) = check_src(
            r#"fn main() {
                let a = 1;
                let s = "x";
                let f = fn(): int { assert(s == "x"); return a; };
                f();
            }"#,
        )
        .unwrap();
        let Stmt::Let { init, .. } = &program.funcs[0].body.stmts[2] else { panic!() };
        let ExprKind::Lambda(lam) = &init.kind else { panic!() };
        assert_eq!(lam.captures.len(), 2);
        assert_eq!(lam.captures[0].name, "s");
        assert_eq!(lam.captures[1].name, "a");
        assert_eq!(lam.captures[1].ty, INT);
    }

    #[test]
    fn name_errors() {
        assert!(err_in_main("let x: Foo = 1;").contains("unknown type 'Foo'"));
        assert!(err_in_main("println(x);").contains("unknown variable 'x'"));
        assert!(err_in_main("foo();").contains("unknown function 'foo'"));
        assert!(err_in_main("let p = P { x: 1 };").contains("unknown struct 'P'"));
        assert!(err("struct P { x: int } fn main() { let p = P { x: 1 }; println(p.y); }")
            .contains("no field or method 'y'"));
        assert!(err("struct P { x: int } fn main() { let p = P { y: 1 }; }")
            .contains("no field 'y'"));
    }

    #[test]
    fn duplicate_errors() {
        assert!(err("struct P { x: int } struct P { y: int } fn main() { }")
            .contains("duplicate struct"));
        assert!(err("fn f() { } fn f() { } fn main() { }").contains("duplicate function"));
        assert!(err("struct P { x: int, x: int } fn main() { }").contains("duplicate field"));
        assert!(err("struct P { x: int } fn main() { let p = P { x: 1, x: 2 }; }")
            .contains("duplicate field"));
        assert!(err("struct int { x: int } fn main() { }").contains("shadows a primitive type"));
    }

    #[test]
    fn struct_literal_errors() {
        assert!(err("struct P { x: int, y: int } fn main() { let p = P { x: 1 }; }")
            .contains("missing field 'y'"));
    }

    #[test]
    fn call_errors() {
        let src = "fn f(a: int, b: int): int { return a; } fn main() { f(1); }";
        assert!(err(src).contains("expects 2 argument(s), got 1"));
        let src = "fn f(a: int): int { return a; } fn main() { f(true); }";
        assert!(err(src).contains("type mismatch"));
        assert!(err_in_main("let x = 1; x();").contains("cannot call a value of type int"));
    }

    #[test]
    fn type_mismatch_errors() {
        assert!(err_in_main("let x: int = 1; let y: bool = x;").contains("type mismatch"));
        assert!(err_in_main("if (1) { }").contains("condition must be 'bool'"));
        assert!(err_in_main("while (\"x\") { }").contains("condition must be 'bool'"));
        assert!(err_in_main("let x = 1 + \"s\";").contains("arithmetic on mixed types"));
        assert!(err_in_main("let x = 1 + 1.5;").contains("arithmetic on mixed types"));
        assert!(err_in_main("let x = true + false;").contains("invalid operand type"));
        assert!(err_in_main("let x = 1.5 % 2.0;").contains("invalid operand type"));
        assert!(err_in_main("let x = 1 == \"s\";").contains("cannot compare"));
        assert!(err_in_main("let x = \"nil\";  x = 1;").contains("type mismatch"));
        assert!(err_in_main("let x = \"a\" < \"b\";")
            .contains("comparison needs two ints or two floats"));
        assert!(err_in_main("let x = 1 && true;").contains("logical operator needs bool"));
        assert!(err_in_main("let x = -true;").contains("operator '-' needs a signed int or float"));
        assert!(err_in_main("let x = !1;").contains("operator '!' needs bool"));
    }

    #[test]
    fn assignment_errors() {
        assert!(err("fn f() { } fn main() { f = f; }").contains("cannot assign to function"));
        assert!(err_in_main("let n = 1; let f = fn() { n = 2; }; f();")
            .contains("capture by value"));
        assert!(err_in_main("let x = 1; x = \"s\";").contains("type mismatch"));
    }

    #[test]
    fn return_errors() {
        assert!(err("fn f(): int { let x = 1; } fn main() { }")
            .contains("must return a value on all paths"));
        assert!(err("fn f(): int { if (true) { return 1; } } fn main() { }")
            .contains("must return a value on all paths"));
        assert!(err("fn f() { return 1; } fn main() { }")
            .contains("does not return a value"));
        assert!(err("fn f(): int { return; } fn main() { }")
            .contains("must return a value of type int"));
        assert!(err("fn f(): int { return true; } fn main() { }").contains("type mismatch"));
        assert!(err_in_main("let f = fn(): int { let x = 1; };")
            .contains("lambda must return a value on all paths"));
        // Both branches returning satisfies the path check.
        check_src("fn f(): int { if (true) { return 1; } else { return 2; } } fn main() { }")
            .unwrap();
    }

    #[test]
    fn loop_errors() {
        assert!(err_in_main("break;").contains("outside of a loop"));
        assert!(err_in_main("continue;").contains("outside of a loop"));
        assert!(err_in_main("let f = fn() { break; }; f();").contains("outside of a loop"));
    }

    #[test]
    fn inference_errors() {
        assert!(err_in_main("let x = [];").contains("cannot infer the type of an empty array"));
        assert!(err("fn f() { } fn main() { let x = f(); }").contains("initializer has no value"));
    }

    #[test]
    fn builtin_errors() {
        assert!(err("struct P { x: int } fn main() { println(P { x: 1 }); }")
            .contains("cannot print a value of type P"));
        assert!(err_in_main("println(1, 2);").contains("expects 1 argument(s)"));
        assert!(err_in_main("let x = 1.len();").contains("type int has no method 'len'"));
        assert!(err_in_main("1.push(2);").contains("type int has no method 'push'"));
        assert!(err_in_main("let x = 1.pop();").contains("type int has no method 'pop'"));
        assert!(err_in_main("let xs = [1]; xs.push(\"s\");").contains("type mismatch"));
        assert!(err_in_main("let x = true.to_string();")
            .contains("type bool has no method 'to_string'"));
        assert!(err_in_main("let xs = [1]; xs.push(1, 2);")
            .contains("method 'push' expects 1 argument(s), got 2"));
        assert!(err_in_main("let xs = [1]; let n = xs.len(1);")
            .contains("method 'len' expects 0 argument(s), got 1"));
        assert!(err_in_main("assert(1);").contains("type mismatch"));
        assert!(err_in_main("let p = println;").contains("can only be called"));
        assert!(err_in_main("let xs = [1]; let s = xs.len;")
            .contains("builtin method 'len' can only be called"));
        // Retired free-function forms point at the method spelling.
        assert!(err_in_main("let xs = [1]; let n = len(xs);")
            .contains("'len' is now a method: write x.len()"));
        assert!(err_in_main("let s = itos(1);").contains("write i.to_string()"));
        assert!(err_in_main("let i = ftoi(1.5);").contains("write f as int"));
        assert!(err_in_main("let f = len;").contains("'len' is now a method"));
    }

    #[test]
    fn index_and_field_errors() {
        assert!(err_in_main("let x = 1; println(x[0]);").contains("cannot index a value of type"));
        assert!(err_in_main("let xs = [1]; println(xs[true]);")
            .contains("array index must be int"));
        assert!(err_in_main("let x = 1; println(x.y);").contains("type int has no method 'y'"));
        assert!(err_in_main("let s = \"x\"; println(s.len);")
            .contains("builtin method 'len' can only be called"));
    }

    #[test]
    fn main_signature_errors() {
        assert!(err("fn not_main() { }").contains("no 'main' function defined"));
        assert!(err("fn main(x: int) { }").contains("'main' must take no parameters"));
        assert!(err("fn main(): int { return 1; }").contains("'main' must take no parameters"));
    }

    #[test]
    fn generic_inference_and_instantiation() {
        let (program, _) = check_src(
            r#"struct Pair<T> { a: T, b: T }
               fn id<T>(x: T): T { return x; }
               fn firsts<T, U>(xs: [T], ys: [U]): T { return xs[0]; }
               fn main() {
                   let a = id(1);
                   let b = id("s");
                   let c = firsts([1.5], ["x"]);
                   let p = Pair { a: 1, b: 2 };
                   let q: Pair<string> = Pair { a: "x", b: "y" };
               }"#,
        )
        .unwrap();
        let get_inst = |stmt: &Stmt| -> Vec<Type> {
            let Stmt::Let { init, .. } = stmt else { panic!() };
            let ExprKind::Call { inst, .. } = &init.kind else { panic!() };
            inst.clone()
        };
        let stmts = &program.funcs[2].body.stmts;
        assert_eq!(get_inst(&stmts[0]), vec![INT]);
        assert_eq!(get_inst(&stmts[1]), vec![Type::Str]);
        assert_eq!(get_inst(&stmts[2]), vec![Type::Float, Type::Str]);
        let Stmt::Let { init, .. } = &stmts[3] else { panic!() };
        assert_eq!(init.ty, Type::Struct(0, vec![INT]));
        let Stmt::Let { init, .. } = &stmts[4] else { panic!() };
        assert_eq!(init.ty, Type::Struct(0, vec![Type::Str]));
    }

    #[test]
    fn generic_errors() {
        // Operations that are semantically type-directed are rejected on
        // opaque type parameters.
        assert!(err("fn f<T>(x: T): bool { return x == x; } fn main() { f(1); }")
            .contains("cannot compare values of generic type T"));
        assert!(err("fn f<T>(x: T): T { return x + x; } fn main() { f(1); }")
            .contains("invalid operand type T"));
        assert!(err("fn f<T>(x: T): bool { return x < x; } fn main() { f(1); }")
            .contains("comparison needs two ints or two floats"));
        assert!(err("fn f<T>(x: T) { println(x); } fn main() { f(1); }")
            .contains("cannot print a value of type T"));
        assert!(err("fn f<T>(x: T): int { return x as int; } fn main() { f(1); }")
            .contains("cannot cast T to int"));
        assert!(err("fn f<T>(x: T) { let y: T = 1; } fn main() { f(1); }")
            .contains("type mismatch: expected T, found int"));
        // Inference failures.
        assert!(err("fn f<T>(): [T] { return []; } fn main() { f(); }")
            .contains("cannot infer type parameter 'T' of 'f'"));
        assert!(err("fn f<T>(a: T, b: T) { } fn main() { f(1, \"s\"); }")
            .contains("type mismatch"));
        // Generic functions are not first-class values.
        assert!(err("fn f<T>(x: T): T { return x; } fn main() { let g = f; }")
            .contains("can only be called directly"));
        // Type argument arity and scoping.
        assert!(err("struct P<T> { v: T } fn main() { let p: P<int, int> = P { v: 1 }; }")
            .contains("expects 1 type argument(s), got 2"));
        assert!(err("struct P<T> { v: T } fn main() { let p: P = P { v: 1 }; }")
            .contains("expects 1 type argument(s), got 0"));
        assert!(err("struct P { v: int } fn main() { let p: P<int> = P { v: 1 }; }")
            .contains("expects 0 type argument(s), got 1"));
        assert!(err("fn main() { let x: int<bool> = 1; }")
            .contains("takes no type arguments"));
        assert!(err("fn f<T, T>(x: T) { } fn main() { }")
            .contains("duplicate type parameter"));
        assert!(err("fn f<u8>(x: u8) { } fn main() { }")
            .contains("shadows a primitive type"));
        assert!(err("fn f(x: T) { } fn main() { }").contains("unknown type 'T'"));
        assert!(err("fn main<T>() { }").contains("'main' cannot be generic"));
        // Instantiations are distinct types.
        assert!(err(
            "struct P<T> { v: T } \
             fn main() { let a = P { v: 1 }; let u: u8 = 2; let b = P { v: u }; a = b; }"
        )
        .contains("type mismatch: expected P<int>, found P<u8>"));
    }

    #[test]
    fn bitwise_and_shift_rules() {
        check_src(
            r#"fn main() {
                let a = 6 & 3 | 8 ^ 1;
                let b = 1 << 4 >> 2;
                let c = ~a;
                let d: u8 = 0;
                let e = d & 15;          // literal adopts u8
                let f = ~d;              // ~ keeps the operand type
                let g: u8 = ~1;          // context flows through ~
                d <<= 2;
            }"#,
        )
        .unwrap();
        assert!(err_in_main("let a: u8 = 1; let b: u16 = 2; let c = a & b;")
            .contains("arithmetic on mixed types u8 and u16"));
        assert!(err_in_main("let x = 1.5 & 2.0;").contains("operator '&' needs integer"));
        assert!(err_in_main("let x = true | false;").contains("operator '|' needs integer"));
        assert!(err_in_main("let x = 1.5 << 2.0;").contains("operator '<<' needs integer"));
        assert!(err_in_main("let x = \"a\" ^ \"b\";").contains("operator '^' needs integer"));
        assert!(err_in_main("let x = ~true;").contains("operator '~' needs an integer"));
        assert!(err_in_main("let x = ~1.5;").contains("operator '~' needs an integer"));
        assert!(err("fn f<T>(x: T): T { return x & x; } fn main() { f(1); }")
            .contains("operator '&' needs integer"));
    }

    #[test]
    fn byte_literals_are_u8() {
        let (_, checked) = check_src(
            r#"fn main() {
                let a = b'a';
                let s = "xy";
                let eq = s[0] == b'x';
                let d = s[1] - b'0';
            }"#,
        )
        .unwrap();
        assert_eq!(checked.func_locals[0][0], Type::Int(IntKind::U8));
        assert_eq!(checked.func_locals[0][3], Type::Int(IntKind::U8));
        // Exactly u8: no context adoption.
        assert!(err_in_main("let x: int = b'a';").contains("type mismatch"));
        assert!(err_in_main("let x = 1 + b'a';").contains("arithmetic on mixed types"));
    }

    #[test]
    fn string_indexing_rules() {
        let (_, checked) =
            check_src("fn main() { let s = \"abc\"; let b = s[1]; }").unwrap();
        assert_eq!(checked.func_locals[0][1], Type::Int(IntKind::U8));
        assert!(err_in_main("let s = \"abc\"; s[0] = b'x';").contains("strings are immutable"));
        assert!(err_in_main("let s = \"abc\"; s[0] += 1;").contains("strings are immutable"));
        assert!(err_in_main("let s = \"abc\"; let b = s[true];")
            .contains("array index must be int"));
    }

    #[test]
    fn compound_assignment_rules() {
        check_src(
            r#"struct P { n: int, s: string }
               fn main() {
                   let x = 1;
                   x += 2; x -= 1; x *= 3; x /= 2; x %= 2;
                   x &= 7; x |= 1; x ^= 2; x <<= 1; x >>= 1;
                   let f = 1.5; f += 0.5; f /= 2.0;
                   let s = "a"; s += "b";
                   let p = P { n: 0, s: "" };
                   p.n += 1; p.s += "!";
                   let xs = [1, 2]; xs[0] += 10;
               }"#,
        )
        .unwrap();
        assert!(err_in_main("let x = 1; x += 1.5;").contains("type mismatch"));
        assert!(err_in_main("let s = \"a\"; s -= \"b\";")
            .contains("invalid operand type string for arithmetic"));
        assert!(err_in_main("let f = 1.5; f %= 2.0;")
            .contains("invalid operand type float for arithmetic"));
        assert!(err_in_main("let f = 1.5; f &= 2.0;").contains("operator '&' needs integer"));
        assert!(err_in_main("let b = true; b += true;").contains("invalid operand type"));
        assert!(err_in_main("let n = 1; let f = fn() { n += 2; }; f();")
            .contains("capture by value"));
    }

    #[test]
    fn if_expression_rules() {
        let (_, checked) = check_src(
            r#"struct P { v: int }
               fn main() {
                   let a = if true { 1 } else { 2 };
                   let b = if false { "x" } else if true { "y" } else { "z" };
                   let c = if true { P { v: 1 } } else { P { v: 2 } };
                   let e: u8 = if true { 1 } else { 255 };  // literals adopt context
               }"#,
        )
        .unwrap();
        let locals = &checked.func_locals[0];
        assert_eq!(locals[0], INT);
        assert_eq!(locals[1], Type::Str);
        assert_eq!(locals[2], Type::Struct(0, vec![]));
        assert_eq!(locals[3], Type::Int(IntKind::U8));
        assert!(err_in_main("let x = if 1 { 2 } else { 3 };")
            .contains("condition must be 'bool'"));
        assert!(err_in_main("let x = if true { 1 } else { \"s\" };")
            .contains("branches have different types"));
        assert!(err("fn f() { } fn main() { let x = if true { f() } else { f() }; }")
            .contains("branches must produce a value"));
    }

    #[test]
    fn stob_btos_rules() {
        let (_, checked) = check_src(
            r#"fn main() {
                let bs = "abc".to_bytes();
                let s = bs.to_string();
            }"#,
        )
        .unwrap();
        assert_eq!(checked.func_locals[0][0], Type::Array(Box::new(Type::Int(IntKind::U8))));
        assert_eq!(checked.func_locals[0][1], Type::Str);
        assert!(err_in_main("let x = 1.to_bytes();")
            .contains("type int has no method 'to_bytes'"));
        assert!(err_in_main("let x = \"s\".to_string();")
            .contains("type string has no method 'to_string'"));
        assert!(err_in_main("let is = [1]; let x = is.to_string();")
            .contains("to_string() needs a [u8]"));
        // A receiver is checked without context, so a literal receiver needs
        // an annotation to become a [u8].
        assert!(err_in_main("let s = [104, 105].to_string();")
            .contains("to_string() needs a [u8]"));
        check_src("fn main() { let bs: [u8] = [104, 105]; let s = bs.to_string(); }").unwrap();
    }

    #[test]
    fn tail_expression_typing() {
        // Tail expressions are checked exactly like `return`.
        check_src("fn f(x: int): int { x + 1 }  fn main() { }").unwrap();
        assert!(err("fn f(): int { \"s\" }  fn main() { }").contains("type mismatch"));
        assert!(err("fn f(): int { 1; }  fn main() { }")
            .contains("must return a value on all paths"));
        assert!(err_in_main("let f = fn(): int { true };").contains("type mismatch"));
    }

    #[test]
    fn stoi_stof_rules() {
        check_src(
            r#"fn main() {
                let i = "42".to_int();
                let f = "1.5".to_float();
                let sum = i + 1;
                let prod = f * 2.0;
            }"#,
        )
        .unwrap();
        assert!(err_in_main("let x = 1.to_int();").contains("type int has no method 'to_int'"));
        assert!(err_in_main("let x = true.to_float();")
            .contains("type bool has no method 'to_float'"));
        assert!(err_in_main("let x = \"1\".to_int(\"2\");")
            .contains("method 'to_int' expects 0 argument(s), got 1"));
    }

    #[test]
    fn method_resolution_and_binding() {
        let (program, _) = check_src(
            r#"struct P { x: int }
               impl P {
                   fn get(self): int { self.x }
                   fn mk(v: int): P { P { x: v } }
               }
               struct Pair<T> { a: T, b: T }
               impl Pair<T> { fn first(self): T { self.a } }
               fn main() {
                   let p = P.mk(1);
                   let a = p.get();
                   let f = p.get;
                   let q = Pair { a: 1, b: 2 };
                   let g = q.first;
               }"#,
        )
        .unwrap();
        // funcs: P.get = 0, P.mk = 1, Pair.first = 2, main = 3.
        let stmts = &program.funcs[3].body.stmts;
        // An associated call is a plain direct call.
        let Stmt::Let { init, .. } = &stmts[0] else { panic!() };
        let ExprKind::Call { direct, args, .. } = &init.kind else { panic!() };
        assert_eq!((*direct, args.len()), (Some(1), 1));
        // A method call is a direct call with the receiver prepended.
        let Stmt::Let { init, .. } = &stmts[1] else { panic!() };
        let ExprKind::Call { direct, args, .. } = &init.kind else { panic!() };
        assert_eq!((*direct, args.len()), (Some(0), 1));
        assert_eq!(init.ty, INT);
        // `p.get` binds the receiver: a zero-argument fn value.
        let Stmt::Let { init, .. } = &stmts[2] else { panic!() };
        assert!(matches!(&init.kind, ExprKind::BoundMethod { fid: 0, .. }));
        assert_eq!(init.ty, Type::Fn(vec![], Box::new(INT)));
        // Binding a generic method records the receiver's type arguments.
        let Stmt::Let { init, .. } = &stmts[4] else { panic!() };
        let ExprKind::BoundMethod { inst, .. } = &init.kind else { panic!() };
        assert_eq!(inst, &vec![INT]);
        assert_eq!(init.ty, Type::Fn(vec![], Box::new(INT)));
    }

    #[test]
    fn user_names_shadow_builtins() {
        // A user *method* named like a builtin method wins on its own type
        // (here: a user `unwrap` on a struct is unrelated to Option's).
        check_src(
            "struct W { v: int } impl W { fn unwrap(self): int { self.v } } \
             fn main() { assert(W { v: 3 }.unwrap() == 3); }",
        )
        .unwrap();
        // A user function named like a *free* builtin wins.
        check_src(
            "fn assert(x: int): int { return x; } fn main() { println(assert(3)); }",
        )
        .unwrap();
        // A local shadows a top-level function inside its scope.
        check_src("fn f(): int { return 1; } fn main() { let f = 2; assert(f == 2); }").unwrap();
    }
}

#[cfg(test)]
mod limit_tests {
    use super::tests_support::*;

    #[test]
    fn struct_field_limit() {
        let fields: String = (0..65).map(|i| format!("f{i}: int, ")).collect();
        let src = format!("struct Big {{ {fields} }} fn main() {{ }}");
        assert!(err(&src).contains("more than 64 fields"));
        let fields: String = (0..64).map(|i| format!("f{i}: int, ")).collect();
        let src = format!("struct Big {{ {fields} }} fn main() {{ }}");
        check_src(&src).unwrap();
    }

    #[test]
    fn local_limit() {
        let make = |n: usize| {
            let decls: String = (0..n).map(|i| format!("let x{i} = 0; ")).collect();
            format!("fn main() {{ {decls} }}")
        };
        assert!(err(&make(4097)).contains("too many locals"));
        check_src(&make(4096)).unwrap();
    }

    #[test]
    fn lambda_capture_limit() {
        let make = |n: usize| {
            let decls: String = (0..n).map(|i| format!("let v{i} = {i}; ")).collect();
            let sum: Vec<String> = (0..n).map(|i| format!("v{i}")).collect();
            format!(
                "fn main() {{ {decls} let f = fn(): int {{ return {}; }}; assert(f() > 0); }}",
                sum.join(" + ")
            )
        };
        assert!(err(&make(64)).contains("captures too many variables"));
        check_src(&make(63)).unwrap();
    }
}
