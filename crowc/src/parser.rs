//! Recursive-descent parser for Crow.

use crate::ast::*;
use crate::lexer::{Tok, Token};

pub fn parse(tokens: Vec<Token>) -> Result<Program, String> {
    let mut p = Parser { toks: tokens, pos: 0, depth: 0, no_struct: false };
    p.program()
}

/// Deepest allowed nesting of expressions, statements, and types. Counts
/// genuine nesting only (left-associative operator chains parse iteratively),
/// so real programs never come close; the limit exists to turn pathological
/// input into a clean error instead of a parser stack overflow. One paren
/// level costs ~11 stack frames across the precedence chain, so the limit
/// must stay small enough for a 2 MiB thread stack in debug builds.
const MAX_DEPTH: u32 = 200;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    depth: u32,
    /// True while parsing an if/while condition: `Ident {` is then never a
    /// struct literal, so the `{` starts the body. Cleared inside any
    /// bracketing construct (parens, brackets, argument lists, blocks).
    no_struct: bool,
}

type PResult<T> = Result<T, String>;

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek2(&self) -> &Tok {
        &self.toks[(self.pos + 1).min(self.toks.len() - 1)].tok
    }

    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn err<T>(&self, msg: &str) -> PResult<T> {
        let t = &self.toks[self.pos];
        Err(format!("{}:{}: {}, found {}", t.line, t.col, msg, t.tok))
    }

    /// Guard a recursive descent against pathological nesting. Wraps every
    /// self-recursive entry point (expressions, statements, types).
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!("{}: program is nested too deeply", self.line()));
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// Parse an if/while condition. Parentheses are ordinary grouping, not
    /// required. Unparenthesized struct literals are disallowed so that
    /// `if x {` reads the `{` as the body, not as `x { ... }`.
    fn cond(&mut self) -> PResult<Expr> {
        let saved = std::mem::replace(&mut self.no_struct, true);
        let r = self.expr();
        self.no_struct = saved;
        r
    }

    /// Run `f` with the struct-literal restriction lifted (inside brackets
    /// the body `{` can no longer be confused with a struct literal).
    fn unrestricted<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        let saved = std::mem::replace(&mut self.no_struct, false);
        let r = f(self);
        self.no_struct = saved;
        r
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == tok {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: Tok) -> PResult<()> {
        if self.peek() == &tok {
            self.next();
            Ok(())
        } else {
            self.err(&format!("expected {tok}"))
        }
    }

    /// Consume the `>` that closes a type-argument list. A `>>` token (lexed
    /// as a shift) is split in place: its first `>` is consumed here and a
    /// plain `>` remains for the enclosing list (`Pair<Pair<int>>`).
    fn expect_gt(&mut self) -> PResult<()> {
        match self.peek() {
            Tok::Gt => {
                self.next();
                Ok(())
            }
            Tok::Shr => {
                self.toks[self.pos].tok = Tok::Gt;
                self.toks[self.pos].col += 1;
                Ok(())
            }
            _ => self.err("expected '>'"),
        }
    }

    fn ident(&mut self) -> PResult<String> {
        match self.peek() {
            Tok::Ident(name) => {
                let name = name.clone();
                self.next();
                Ok(name)
            }
            _ => self.err("expected identifier"),
        }
    }

    // -- Top level ----------------------------------------------------------

    fn program(&mut self) -> PResult<Program> {
        let mut structs = Vec::new();
        let mut enums = Vec::new();
        let mut funcs = Vec::new();
        let mut methods = Vec::new();
        loop {
            match self.peek() {
                Tok::Eof => break,
                Tok::Struct => structs.push(self.struct_def()?),
                Tok::Enum => enums.push(self.enum_def()?),
                Tok::Fn => funcs.push(self.func_def()?),
                Tok::Impl => self.impl_block(&mut funcs, &mut methods)?,
                _ => {
                    return self.err("expected 'fn', 'struct', 'enum', or 'impl' at top level")
                }
            }
        }
        Ok(Program { structs, enums, funcs, methods })
    }

    /// Optional `<T, U, ...>` type parameter list on a declaration.
    fn type_params(&mut self) -> PResult<Vec<String>> {
        let mut params = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                params.push(self.ident()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect_gt()?;
        }
        Ok(params)
    }

    fn struct_def(&mut self) -> PResult<StructDef> {
        let line = self.line();
        self.expect(Tok::Struct)?;
        let name = self.ident()?;
        let type_params = self.type_params()?;
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        while self.peek() != &Tok::RBrace {
            let fname = self.ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.type_expr()?;
            fields.push((fname, ty));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(StructDef { name, type_params, fields, line })
    }

    /// `enum Name<T,...> { Bare, Wrapping(type), Record { f: type, ... } }`
    /// — a variant is bare, wraps exactly one value, or carries named
    /// fields stored inline in the enum object.
    fn enum_def(&mut self) -> PResult<EnumDef> {
        let line = self.line();
        self.expect(Tok::Enum)?;
        let name = self.ident()?;
        let type_params = self.type_params()?;
        self.expect(Tok::LBrace)?;
        let mut variants = Vec::new();
        while self.peek() != &Tok::RBrace {
            let vname = self.ident()?;
            let payload = if self.eat(&Tok::LParen) {
                let ty = self.type_expr()?;
                self.expect(Tok::RParen)?;
                VariantPayloadExpr::Single(ty)
            } else if self.eat(&Tok::LBrace) {
                let mut fields = Vec::new();
                while self.peek() != &Tok::RBrace {
                    let fname = self.ident()?;
                    self.expect(Tok::Colon)?;
                    let ty = self.type_expr()?;
                    fields.push((fname, ty));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RBrace)?;
                VariantPayloadExpr::Fields(fields)
            } else {
                VariantPayloadExpr::Bare
            };
            variants.push((vname, payload));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(EnumDef { name, type_params, variants, line })
    }

    fn func_def(&mut self) -> PResult<FuncDef> {
        let line = self.line();
        self.expect(Tok::Fn)?;
        let name = self.ident()?;
        let type_params = self.type_params()?;
        let params = self.params()?;
        let ret = if self.eat(&Tok::Colon) {
            Some(self.type_expr()?)
        } else {
            None
        };
        let body = self.fn_body(ret.is_some())?;
        Ok(FuncDef { name, type_params, params, ret, body, line, num_locals: 0, is_method: false })
    }

    /// `impl Type<T, ...> { fn ... }`. Each function flattens into the
    /// top-level function list as `Type.name`, with the impl's type
    /// parameters prepended to the method's own and a `self` receiver
    /// desugared to a first parameter of the impl type.
    fn impl_block(
        &mut self,
        funcs: &mut Vec<FuncDef>,
        methods: &mut Vec<MethodDef>,
    ) -> PResult<()> {
        self.expect(Tok::Impl)?;
        let impl_line = self.line();
        let type_name = self.ident()?;
        let impl_params = self.type_params()?;
        self.expect(Tok::LBrace)?;
        while !self.eat(&Tok::RBrace) {
            if self.peek() == &Tok::Eof {
                return self.err("expected '}'");
            }
            if self.peek() != &Tok::Fn {
                return self.err("expected 'fn' inside an impl block");
            }
            let line = self.line();
            self.next();
            let name = self.ident()?;
            let own_params = self.type_params()?;
            let mut type_params = impl_params.clone();
            type_params.extend(own_params);
            let (has_self, mut params) = self.method_params()?;
            if has_self {
                // The receiver is an ordinary first parameter of the impl
                // type; its annotation is synthesized here so the checker
                // resolves it like any other.
                let targs: Vec<TypeExpr> = impl_params
                    .iter()
                    .map(|p| TypeExpr::Named(p.clone(), Vec::new(), impl_line))
                    .collect();
                params.insert(
                    0,
                    ("self".to_string(), TypeExpr::Named(type_name.clone(), targs, impl_line)),
                );
            }
            let ret = if self.eat(&Tok::Colon) {
                Some(self.type_expr()?)
            } else {
                None
            };
            let body = self.fn_body(ret.is_some())?;
            methods.push(MethodDef {
                type_name: type_name.clone(),
                name: name.clone(),
                func: funcs.len() as u32,
                has_self,
                impl_type_params: impl_params.len() as u32,
                line,
            });
            funcs.push(FuncDef {
                name: format!("{type_name}.{name}"),
                type_params,
                params,
                ret,
                body,
                line,
                num_locals: 0,
                is_method: true,
            });
        }
        Ok(())
    }

    /// Parameter list of an impl-block function: an optional leading bare
    /// `self`, then ordinary `name: type` parameters. Returns whether the
    /// receiver was present.
    fn method_params(&mut self) -> PResult<(bool, Vec<(String, TypeExpr)>)> {
        self.expect(Tok::LParen)?;
        let mut has_self = false;
        if self.peek() == &Tok::SelfKw {
            self.next();
            has_self = true;
            if self.peek() == &Tok::Colon {
                return self.err("'self' takes no type annotation");
            }
            if self.peek() != &Tok::RParen {
                self.expect(Tok::Comma)?;
            }
        }
        let mut params = Vec::new();
        while self.peek() != &Tok::RParen {
            if self.peek() == &Tok::SelfKw {
                return self.err("'self' must be the first parameter");
            }
            let name = self.ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.type_expr()?;
            params.push((name, ty));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok((has_self, params))
    }

    fn params(&mut self) -> PResult<Vec<(String, TypeExpr)>> {
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        while self.peek() != &Tok::RParen {
            let name = self.ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.type_expr()?;
            params.push((name, ty));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(params)
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        self.nested(Self::type_expr_inner)
    }

    fn type_expr_inner(&mut self) -> PResult<TypeExpr> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.next();
                let mut args = Vec::new();
                if self.eat(&Tok::Lt) {
                    loop {
                        args.push(self.type_expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect_gt()?;
                }
                Ok(TypeExpr::Named(name, args, line))
            }
            Tok::LBracket => {
                self.next();
                let elem = self.type_expr()?;
                self.expect(Tok::RBracket)?;
                Ok(TypeExpr::Array(Box::new(elem)))
            }
            Tok::Fn => {
                self.next();
                self.expect(Tok::LParen)?;
                let mut params = Vec::new();
                while self.peek() != &Tok::RParen {
                    params.push(self.type_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen)?;
                let ret = if self.eat(&Tok::Colon) {
                    Some(Box::new(self.type_expr()?))
                } else {
                    None
                };
                Ok(TypeExpr::Fn(params, ret, line))
            }
            _ => self.err("expected a type"),
        }
    }

    // -- Statements ---------------------------------------------------------

    fn block(&mut self) -> PResult<Block> {
        self.unrestricted(|p| {
            p.expect(Tok::LBrace)?;
            let mut stmts = Vec::new();
            while p.peek() != &Tok::RBrace {
                if p.peek() == &Tok::Eof {
                    return p.err("expected '}'");
                }
                stmts.push(p.stmt()?);
            }
            p.expect(Tok::RBrace)?;
            Ok(Block { stmts })
        })
    }

    /// A function or lambda body: a block whose *final* statement may be a
    /// bare expression with no `;`. When the function returns a value it
    /// desugars to `return expr;`; for a unit function it is an ordinary
    /// expression statement. A statement starting with `if` is always the
    /// if statement (§7); parenthesize a tail if-expression.
    fn fn_body(&mut self, has_ret: bool) -> PResult<Block> {
        self.unrestricted(|p| {
            p.expect(Tok::LBrace)?;
            let mut stmts = Vec::new();
            loop {
                match p.peek() {
                    Tok::RBrace => {
                        p.next();
                        break;
                    }
                    Tok::Eof => return p.err("expected '}'"),
                    Tok::Let
                    | Tok::If
                    | Tok::Match
                    | Tok::While
                    | Tok::For
                    | Tok::Return
                    | Tok::Break
                    | Tok::Continue
                    | Tok::LBrace => stmts.push(p.stmt()?),
                    _ => {
                        let line = p.line();
                        let s = p.nested(Self::simple_stmt)?;
                        match s {
                            Stmt::Expr(e) if p.peek() == &Tok::RBrace => {
                                p.next();
                                stmts.push(if has_ret {
                                    Stmt::Return { value: Some(e), line }
                                } else {
                                    Stmt::Expr(e)
                                });
                                break;
                            }
                            s => {
                                p.expect(Tok::Semi)?;
                                stmts.push(s);
                            }
                        }
                    }
                }
            }
            Ok(Block { stmts })
        })
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        self.nested(Self::stmt_inner)
    }

    fn stmt_inner(&mut self) -> PResult<Stmt> {
        match self.peek() {
            Tok::Let => {
                let s = self.let_stmt()?;
                self.expect(Tok::Semi)?;
                Ok(s)
            }
            Tok::If => self.if_stmt(),
            Tok::Match => self.match_stmt(),
            Tok::While => {
                self.next();
                let cond = self.cond()?;
                let body = self.block()?;
                Ok(Stmt::While { cond, body })
            }
            Tok::For => self.for_stmt(),
            Tok::Return => {
                let line = self.line();
                self.next();
                let value = if self.peek() == &Tok::Semi {
                    None
                } else {
                    Some(self.expr()?)
                };
                self.expect(Tok::Semi)?;
                Ok(Stmt::Return { value, line })
            }
            Tok::Break => {
                let line = self.line();
                self.next();
                self.expect(Tok::Semi)?;
                Ok(Stmt::Break(line))
            }
            Tok::Continue => {
                let line = self.line();
                self.next();
                self.expect(Tok::Semi)?;
                Ok(Stmt::Continue(line))
            }
            Tok::LBrace => Ok(Stmt::Block(self.block()?)),
            _ => {
                let s = self.simple_stmt()?;
                self.expect(Tok::Semi)?;
                Ok(s)
            }
        }
    }

    fn let_stmt(&mut self) -> PResult<Stmt> {
        let line = self.line();
        self.expect(Tok::Let)?;
        let name = self.ident()?;
        let ann = if self.eat(&Tok::Colon) {
            Some(self.type_expr()?)
        } else {
            None
        };
        self.expect(Tok::Assign)?;
        let init = self.expr()?;
        Ok(Stmt::Let { name, ann, init, line, local: 0, ty: crate::types::Type::Unknown })
    }

    /// Expression statement or (compound) assignment (no trailing ';').
    fn simple_stmt(&mut self) -> PResult<Stmt> {
        let line = self.line();
        let e = self.expr()?;
        let op = match self.peek() {
            Tok::Assign => None,
            Tok::OpAssign(op) => Some(*op),
            _ => return Ok(Stmt::Expr(e)),
        };
        self.next();
        match e.kind {
            ExprKind::Var { .. } | ExprKind::Field { .. } | ExprKind::Index(..) => {}
            _ => return Err(format!("{line}: invalid assignment target")),
        }
        let value = self.expr()?;
        Ok(Stmt::Assign { target: e, op, value, line })
    }

    fn if_stmt(&mut self) -> PResult<Stmt> {
        self.expect(Tok::If)?;
        let cond = self.cond()?;
        let then = self.block()?;
        let els = if self.eat(&Tok::Else) {
            if self.peek() == &Tok::If {
                Some(Block { stmts: vec![self.if_stmt()?] })
            } else {
                Some(self.block()?)
            }
        } else {
            None
        };
        Ok(Stmt::If { cond, then, els })
    }

    /// `match expr { pat => { ... }, ... }` in statement position. Arms are
    /// blocks (like all statement bodies); the comma after a block is
    /// optional. The scrutinee gets the same struct-literal restriction as
    /// if/while conditions, so `match x {` reads the `{` as the match body.
    fn match_stmt(&mut self) -> PResult<Stmt> {
        let line = self.line();
        self.expect(Tok::Match)?;
        let scrutinee = self.cond()?;
        self.expect(Tok::LBrace)?;
        let mut arms = Vec::new();
        while self.peek() != &Tok::RBrace {
            let pat = self.pattern()?;
            self.expect(Tok::FatArrow)?;
            let body = self.block()?;
            arms.push((pat, body));
            self.eat(&Tok::Comma);
        }
        self.expect(Tok::RBrace)?;
        Ok(Stmt::Match { scrutinee, arms, line })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        self.expect(Tok::For)?;
        self.expect(Tok::LParen)?;
        let init = if self.peek() == &Tok::Semi {
            None
        } else if self.peek() == &Tok::Let {
            Some(Box::new(self.let_stmt()?))
        } else {
            Some(Box::new(self.simple_stmt()?))
        };
        self.expect(Tok::Semi)?;
        let cond = if self.peek() == &Tok::Semi {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect(Tok::Semi)?;
        let step = if self.peek() == &Tok::RParen {
            None
        } else {
            Some(Box::new(self.simple_stmt()?))
        };
        self.expect(Tok::RParen)?;
        let body = self.block()?;
        Ok(Stmt::For { init, cond, step, body })
    }

    // -- Expressions --------------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.nested(Self::or_expr)
    }

    fn or_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.and_expr()?;
        while self.peek() == &Tok::OrOr {
            let line = self.line();
            self.next();
            let rhs = self.and_expr()?;
            lhs = Expr::new(ExprKind::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.eq_expr()?;
        while self.peek() == &Tok::AndAnd {
            let line = self.line();
            self.next();
            let rhs = self.eq_expr()?;
            lhs = Expr::new(ExprKind::Binary(BinOp::And, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn eq_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.rel_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Eq => BinOp::Eq,
                Tok::Ne => BinOp::Ne,
                _ => break,
            };
            let line = self.line();
            self.next();
            let rhs = self.rel_expr()?;
            lhs = Expr::new(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn rel_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.bitor_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                _ => break,
            };
            let line = self.line();
            self.next();
            let rhs = self.bitor_expr()?;
            lhs = Expr::new(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    // Bitwise operators bind tighter than comparisons (unlike C), so
    // `x & 1 == 0` means `(x & 1) == 0`.
    fn bitor_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.bitxor_expr()?;
        while self.peek() == &Tok::Pipe {
            let line = self.line();
            self.next();
            let rhs = self.bitxor_expr()?;
            lhs = Expr::new(ExprKind::Binary(BinOp::BitOr, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn bitxor_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.bitand_expr()?;
        while self.peek() == &Tok::Caret {
            let line = self.line();
            self.next();
            let rhs = self.bitand_expr()?;
            lhs = Expr::new(ExprKind::Binary(BinOp::BitXor, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn bitand_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.shift_expr()?;
        while self.peek() == &Tok::Amp {
            let line = self.line();
            self.next();
            let rhs = self.shift_expr()?;
            lhs = Expr::new(ExprKind::Binary(BinOp::BitAnd, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn shift_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.add_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Shl => BinOp::Shl,
                Tok::Shr => BinOp::Shr,
                _ => break,
            };
            let line = self.line();
            self.next();
            let rhs = self.add_expr()?;
            lhs = Expr::new(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn add_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            let line = self.line();
            self.next();
            let rhs = self.mul_expr()?;
            lhs = Expr::new(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    fn mul_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.cast_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                _ => break,
            };
            let line = self.line();
            self.next();
            let rhs = self.cast_expr()?;
            lhs = Expr::new(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), line);
        }
        Ok(lhs)
    }

    /// `expr as Type`, binding looser than unary so `-x as u8` casts `-x`.
    fn cast_expr(&mut self) -> PResult<Expr> {
        let mut e = self.unary_expr()?;
        while self.peek() == &Tok::As {
            let line = self.line();
            self.next();
            let ty = self.type_expr()?;
            e = Expr::new(ExprKind::Cast(Box::new(e), ty), line);
        }
        Ok(e)
    }

    fn unary_expr(&mut self) -> PResult<Expr> {
        self.nested(Self::unary_expr_inner)
    }

    fn unary_expr_inner(&mut self) -> PResult<Expr> {
        let line = self.line();
        match self.peek() {
            Tok::Minus => {
                self.next();
                let e = self.unary_expr()?;
                Ok(Expr::new(ExprKind::Unary(UnOp::Neg, Box::new(e)), line))
            }
            Tok::Not => {
                self.next();
                let e = self.unary_expr()?;
                Ok(Expr::new(ExprKind::Unary(UnOp::Not, Box::new(e)), line))
            }
            Tok::Tilde => {
                self.next();
                let e = self.unary_expr()?;
                Ok(Expr::new(ExprKind::Unary(UnOp::BitNot, Box::new(e)), line))
            }
            _ => self.postfix_expr(),
        }
    }

    fn postfix_expr(&mut self) -> PResult<Expr> {
        let mut e = self.primary_expr()?;
        loop {
            let line = self.line();
            match self.peek() {
                Tok::LParen => {
                    self.next();
                    let mut args = Vec::new();
                    while self.peek() != &Tok::RParen {
                        args.push(self.unrestricted(|p| p.expr())?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                    e = Expr::new(
                        ExprKind::Call { callee: Box::new(e), args, direct: None, inst: Vec::new() },
                        line,
                    );
                }
                Tok::Dot => {
                    self.next();
                    let name = self.ident()?;
                    // `EnumName.Variant { field: value, ... }`: like a struct
                    // literal, the name resolves in the type namespace, and
                    // the same `{` heuristic and condition restriction apply.
                    if let ExprKind::Var { name: base, .. } = &e.kind {
                        if self.peek() == &Tok::LBrace
                            && !self.no_struct
                            && self.struct_lit_ahead()
                        {
                            let (base, variant) = (base.clone(), name);
                            self.expect(Tok::LBrace)?;
                            let mut fields = Vec::new();
                            while self.peek() != &Tok::RBrace {
                                let fname = self.ident()?;
                                self.expect(Tok::Colon)?;
                                let value = self.expr()?;
                                fields.push((fname, value, 0));
                                if !self.eat(&Tok::Comma) {
                                    break;
                                }
                            }
                            self.expect(Tok::RBrace)?;
                            e = Expr::new(
                                ExprKind::VariantStructLit {
                                    enum_name: base,
                                    variant,
                                    fields,
                                },
                                line,
                            );
                            continue;
                        }
                    }
                    e = Expr::new(ExprKind::Field { obj: Box::new(e), name, index: 0 }, line);
                }
                Tok::LBracket => {
                    self.next();
                    let idx = self.unrestricted(|p| p.expr())?;
                    self.expect(Tok::RBracket)?;
                    e = Expr::new(ExprKind::Index(Box::new(e), Box::new(idx)), line);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn primary_expr(&mut self) -> PResult<Expr> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.next();
                Ok(Expr::new(ExprKind::Int(v), line))
            }
            Tok::Byte(v) => {
                self.next();
                Ok(Expr::new(ExprKind::Byte(v), line))
            }
            Tok::If => self.if_expr(),
            Tok::Match => self.match_expr(),
            Tok::Float(v) => {
                self.next();
                Ok(Expr::new(ExprKind::Float(v), line))
            }
            Tok::Str(s) => {
                self.next();
                Ok(Expr::new(ExprKind::Str(s), line))
            }
            Tok::True => {
                self.next();
                Ok(Expr::new(ExprKind::Bool(true), line))
            }
            Tok::False => {
                self.next();
                Ok(Expr::new(ExprKind::Bool(false), line))
            }
            Tok::Ident(name) => {
                self.next();
                if self.peek() == &Tok::LBrace && !self.no_struct && self.struct_lit_ahead() {
                    self.struct_lit(name, line)
                } else {
                    Ok(Expr::new(ExprKind::Var { name, res: None }, line))
                }
            }
            // `self` is an ordinary local inside a method; the checker
            // rejects it anywhere else (no such variable is in scope).
            Tok::SelfKw => {
                self.next();
                Ok(Expr::new(ExprKind::Var { name: "self".to_string(), res: None }, line))
            }
            Tok::LParen => {
                self.next();
                let e = self.unrestricted(|p| p.expr())?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::LBracket => {
                self.next();
                let mut elems = Vec::new();
                while self.peek() != &Tok::RBracket {
                    elems.push(self.unrestricted(|p| p.expr())?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RBracket)?;
                Ok(Expr::new(ExprKind::ArrayLit(elems), line))
            }
            Tok::Fn => {
                self.next();
                let params = self.params()?;
                let ret = if self.eat(&Tok::Colon) {
                    Some(self.type_expr()?)
                } else {
                    None
                };
                let body = self.fn_body(ret.is_some())?;
                Ok(Expr::new(
                    ExprKind::Lambda(Box::new(LambdaDef {
                        params,
                        ret,
                        body,
                        line,
                        id: 0,
                        num_locals: 0,
                        captures: Vec::new(),
                    })),
                    line,
                ))
            }
            _ => self.err("expected an expression"),
        }
    }

    /// `if cond { expr } else { expr }` in expression position. Each branch
    /// is a single expression (no statements, no trailing `;`) and `else` is
    /// mandatory; `else if` chains recurse.
    fn if_expr(&mut self) -> PResult<Expr> {
        self.nested(Self::if_expr_inner)
    }

    fn if_expr_inner(&mut self) -> PResult<Expr> {
        let line = self.line();
        self.expect(Tok::If)?;
        let cond = self.cond()?;
        self.expect(Tok::LBrace)?;
        let then = self.unrestricted(|p| p.expr())?;
        self.expect(Tok::RBrace)?;
        self.expect(Tok::Else)?;
        let els = if self.peek() == &Tok::If {
            self.if_expr()?
        } else {
            self.expect(Tok::LBrace)?;
            let e = self.unrestricted(|p| p.expr())?;
            self.expect(Tok::RBrace)?;
            e
        };
        Ok(Expr::new(
            ExprKind::If { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) },
            line,
        ))
    }

    /// `match expr { pat => expr, ... }` in expression position. Each arm is
    /// a single expression; arms are comma-separated (trailing comma
    /// allowed). A statement *starting* with `match` is always the match
    /// statement, so a tail match-expression needs parens, like `if`.
    fn match_expr(&mut self) -> PResult<Expr> {
        self.nested(Self::match_expr_inner)
    }

    fn match_expr_inner(&mut self) -> PResult<Expr> {
        let line = self.line();
        self.expect(Tok::Match)?;
        let scrutinee = self.cond()?;
        self.expect(Tok::LBrace)?;
        let mut arms = Vec::new();
        while self.peek() != &Tok::RBrace {
            let pat = self.pattern()?;
            self.expect(Tok::FatArrow)?;
            let body = self.unrestricted(|p| p.expr())?;
            arms.push((pat, body));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(Expr::new(
            ExprKind::Match { scrutinee: Box::new(scrutinee), arms },
            line,
        ))
    }

    /// One-constructor-deep match pattern: `Enum.Variant`,
    /// `Enum.Variant(binder)`, a literal, or `_`.
    fn pattern(&mut self) -> PResult<Pattern> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Ident(name) if name == "_" => {
                self.next();
                Ok(Pattern::Wildcard { line })
            }
            Tok::Ident(enum_name) => {
                self.next();
                self.expect(Tok::Dot)?;
                let variant = self.ident()?;
                let binder = |name: String| PatBinder {
                    name,
                    local: 0,
                    ty: crate::types::Type::Unknown,
                };
                let args = if self.eat(&Tok::LParen) {
                    let bname = self.ident()?;
                    self.expect(Tok::RParen)?;
                    PatArgs::Single(binder(bname))
                } else if self.eat(&Tok::LBrace) {
                    // `{ field: name, ... }`; a lone `field` binds a local
                    // of the same name. Every declared field must appear.
                    let mut fields = Vec::new();
                    while self.peek() != &Tok::RBrace {
                        let fname = self.ident()?;
                        let bname = if self.eat(&Tok::Colon) {
                            self.ident()?
                        } else {
                            fname.clone()
                        };
                        fields.push((fname, binder(bname), 0));
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RBrace)?;
                    PatArgs::Fields(fields)
                } else {
                    PatArgs::Bare
                };
                Ok(Pattern::Variant { enum_name, variant, args, tag: 0, line })
            }
            Tok::Int(digits) => {
                self.next();
                Ok(Pattern::IntLit { neg: false, digits, value: 0, line })
            }
            Tok::Minus => {
                self.next();
                match self.peek().clone() {
                    Tok::Int(digits) => {
                        self.next();
                        Ok(Pattern::IntLit { neg: true, digits, value: 0, line })
                    }
                    _ => self.err("expected an integer literal after '-' in a pattern"),
                }
            }
            Tok::Byte(value) => {
                self.next();
                Ok(Pattern::ByteLit { value, line })
            }
            Tok::True => {
                self.next();
                Ok(Pattern::BoolLit { value: true, line })
            }
            Tok::False => {
                self.next();
                Ok(Pattern::BoolLit { value: false, line })
            }
            _ => self.err("expected a pattern"),
        }
    }

    /// After `Ident`, decide whether a `{` begins a struct literal. It does
    /// when it looks like `{}` or `{ ident :`. This keeps statements like
    /// `x; { ... }` unambiguous enough for a minimal language.
    fn struct_lit_ahead(&self) -> bool {
        debug_assert_eq!(self.peek(), &Tok::LBrace);
        match self.peek2() {
            Tok::RBrace => true,
            Tok::Ident(_) => {
                matches!(self.toks.get(self.pos + 2).map(|t| &t.tok), Some(Tok::Colon))
            }
            _ => false,
        }
    }

    fn struct_lit(&mut self, name: String, line: u32) -> PResult<Expr> {
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        while self.peek() != &Tok::RBrace {
            let fname = self.ident()?;
            self.expect(Tok::Colon)?;
            let value = self.expr()?;
            fields.push((fname, value, 0));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(Expr::new(ExprKind::StructLit { name, fields, struct_id: 0 }, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{ExprKind, Stmt};

    fn parse_src(src: &str) -> Result<Program, String> {
        parse(crate::lexer::lex(src).expect("lex error in test"))
    }

    fn parse_err(src: &str) -> String {
        parse_src(src).expect_err("expected parse error")
    }

    #[test]
    fn full_program_parses() {
        let p = parse_src(
            r#"
struct Point { x: int, y: int, }
struct Empty { }
fn dist(a: Point, b: [float], f: fn(int): bool): int { return 0; }
fn main() {
    let p = Point { x: 1, y: 2 };
    let e = Empty { };
    let xs = [1, 2, 3,];
    let f = fn(x: int): int { return x; };
    for (let i = 0; i < 3; i = i + 1) { continue; }
    for (;;) { break; }
    while (p.x < 3) { p.x = p.x + 1; }
    if (true) { } else if (false) { } else { }
    xs[0] = f(xs[1]) + -p.y * 2;
    return;
}
"#,
        )
        .unwrap();
        assert_eq!(p.structs.len(), 2);
        assert_eq!(p.funcs.len(), 2);
        assert_eq!(p.structs[0].fields.len(), 2);
        assert_eq!(p.funcs[0].params.len(), 3);
    }

    #[test]
    fn precedence_shapes() {
        let p = parse_src("fn main() { let a = 1 + 2 * 3; let b = (1 + 2) * 3; }").unwrap();
        // a = Add(1, Mul(2, 3))
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        let ExprKind::Binary(crate::ast::BinOp::Add, _, rhs) = &init.kind else { panic!() };
        assert!(matches!(rhs.kind, ExprKind::Binary(crate::ast::BinOp::Mul, _, _)));
        // b = Mul(Add(1, 2), 3)
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[1] else { panic!() };
        assert!(matches!(init.kind, ExprKind::Binary(crate::ast::BinOp::Mul, _, _)));
    }

    #[test]
    fn bitwise_precedence_shapes() {
        use crate::ast::BinOp;
        let first_init = |src: &str| {
            let mut p = parse_src(&format!("fn main() {{ let a = {src}; }}")).unwrap();
            let Stmt::Let { init, .. } = p.funcs.remove(0).body.stmts.remove(0) else {
                panic!()
            };
            init
        };
        // Bitwise binds tighter than comparison: (x & 1) == 0.
        let e = first_init("x & 1 == 0");
        let ExprKind::Binary(BinOp::Eq, lhs, _) = &e.kind else { panic!() };
        assert!(matches!(lhs.kind, ExprKind::Binary(BinOp::BitAnd, _, _)));
        // | < ^ < & : a | (b ^ (c & d)).
        let e = first_init("a | b ^ c & d");
        let ExprKind::Binary(BinOp::BitOr, _, rhs) = &e.kind else { panic!() };
        let ExprKind::Binary(BinOp::BitXor, _, rhs) = &rhs.kind else { panic!() };
        assert!(matches!(rhs.kind, ExprKind::Binary(BinOp::BitAnd, _, _)));
        // Shifts bind tighter than & and looser than +: a & (b << (c + 1)).
        let e = first_init("a & b << c + 1");
        let ExprKind::Binary(BinOp::BitAnd, _, rhs) = &e.kind else { panic!() };
        let ExprKind::Binary(BinOp::Shl, _, rhs) = &rhs.kind else { panic!() };
        assert!(matches!(rhs.kind, ExprKind::Binary(BinOp::Add, _, _)));
        // `~` is unary and nests: ~(~x) parses.
        let e = first_init("~~x");
        let ExprKind::Unary(crate::ast::UnOp::BitNot, inner) = &e.kind else { panic!() };
        assert!(matches!(inner.kind, ExprKind::Unary(crate::ast::UnOp::BitNot, _)));
    }

    #[test]
    fn compound_assignment() {
        use crate::ast::BinOp;
        let p = parse_src("fn main() { x += 1; a[i] <<= 2; p.f &= 3; }").unwrap();
        let ops: Vec<Option<BinOp>> = p.funcs[0]
            .body
            .stmts
            .iter()
            .map(|s| match s {
                Stmt::Assign { op, .. } => *op,
                _ => panic!("expected assignment"),
            })
            .collect();
        assert_eq!(ops, vec![Some(BinOp::Add), Some(BinOp::Shl), Some(BinOp::BitAnd)]);
        assert!(parse_err("fn main() { 1 += 2; }").contains("invalid assignment target"));
        assert!(parse_err("fn main() { f() += 2; }").contains("invalid assignment target"));
    }

    #[test]
    fn if_expressions() {
        let p = parse_src("fn main() { let x = if c { 1 } else if d { 2 } else { 3 }; }")
            .unwrap();
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        let ExprKind::If { els, .. } = &init.kind else { panic!() };
        assert!(matches!(els.kind, ExprKind::If { .. }));
        // Branches are single expressions; else is mandatory.
        assert!(parse_err("fn main() { let x = if c { 1 }; }").contains("expected 'else'"));
        assert!(parse_err("fn main() { let x = if c { let y = 1; } else { 2 }; }")
            .contains("expected an expression"));
        // Works nested in argument position and in conditions.
        parse_src("fn main() { f(if c { 1 } else { 2 }); }").unwrap();
        parse_src("fn main() { if (if c { true } else { false }) { } }").unwrap();
    }

    #[test]
    fn tail_expressions() {
        // A trailing bare expression desugars to `return` when the function
        // returns a value...
        let p = parse_src("fn f(x: int): int { x + 1 }  fn main() { }").unwrap();
        assert!(matches!(p.funcs[0].body.stmts[0], Stmt::Return { value: Some(_), .. }));
        // ...after any number of ordinary statements...
        let p = parse_src("fn f(x: int): int { let y = x; y }  fn main() { }").unwrap();
        assert_eq!(p.funcs[0].body.stmts.len(), 2);
        assert!(matches!(p.funcs[0].body.stmts[1], Stmt::Return { .. }));
        // ...and stays a plain expression statement in a unit function.
        let p = parse_src("fn main() { println(1) }").unwrap();
        assert!(matches!(p.funcs[0].body.stmts[0], Stmt::Expr(_)));
        // Lambdas get the same rule.
        let p = parse_src("fn main() { let f = fn(x: int): int { x * 2 }; }").unwrap();
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        let ExprKind::Lambda(lam) = &init.kind else { panic!() };
        assert!(matches!(lam.body.stmts[0], Stmt::Return { .. }));
        // A parenthesized if-expression can be the tail.
        parse_src("fn f(x: int): int { (if x > 0 { 1 } else { 0 }) }  fn main() { }").unwrap();
        // Only the final statement may omit ';'; assignments always need it.
        assert!(parse_err("fn f(): int { 1 2 }").contains("expected ';'"));
        assert!(parse_err("fn main() { x = 1 }").contains("expected ';'"));
    }

    #[test]
    fn generic_declarations() {
        let p = parse_src(
            r#"
struct Pair<T> { a: T, b: T }
struct Two<K, V> { k: K, v: V }
fn id<T>(x: T): T { return x; }
fn main() {
    let a: Pair<int> = x;
    let b: Pair<Pair<int>> = x;
    let c: Two<string, [Pair<int>]> = x;
    let d: fn(Pair<int>): int = x;
}
"#,
        )
        .unwrap();
        assert_eq!(p.structs[0].type_params, vec!["T"]);
        assert_eq!(p.structs[1].type_params, vec!["K", "V"]);
        assert_eq!(p.funcs[0].type_params, vec!["T"]);
        assert!(p.funcs[1].type_params.is_empty());
        // Nested closing angle brackets lex as two '>' tokens.
        let Stmt::Let { ann: Some(TypeExpr::Named(name, args, _)), .. } =
            &p.funcs[1].body.stmts[1]
        else {
            panic!()
        };
        assert_eq!(name, "Pair");
        assert!(matches!(&args[0], TypeExpr::Named(n, a, _) if n == "Pair" && a.len() == 1));
        assert!(parse_err("fn f<>() { }").contains("expected identifier"));
        assert!(parse_err("fn f<T() { }").contains("expected '>'"));
        assert!(parse_err("fn main() { let x: Pair<int = y; }").contains("expected '>'"));
    }

    #[test]
    fn enum_declarations() {
        let p = parse_src(
            r#"
enum Color { Red, Green, Blue }
enum Shape<T> { Circle(float), Box(T), Rect { w: float, h: float }, Empty, }
fn main() { }
"#,
        )
        .unwrap();
        assert_eq!(p.enums.len(), 2);
        assert_eq!(p.enums[0].variants.len(), 3);
        assert!(p.enums[0]
            .variants
            .iter()
            .all(|(_, t)| matches!(t, VariantPayloadExpr::Bare)));
        assert_eq!(p.enums[1].type_params, vec!["T"]);
        assert!(matches!(p.enums[1].variants[0].1, VariantPayloadExpr::Single(_)));
        assert!(matches!(&p.enums[1].variants[2].1,
            VariantPayloadExpr::Fields(fs) if fs.len() == 2));
        assert!(matches!(p.enums[1].variants[3].1, VariantPayloadExpr::Bare));
        assert!(parse_err("enum E { A(int, int) } fn main() { }").contains("expected ')'"));
        assert!(parse_err("enum E { A: int } fn main() { }").contains("expected '}'"));
    }

    #[test]
    fn match_forms() {
        // Statement position: arms are blocks, comma after a block optional.
        let p = parse_src(
            "fn main() { match x { E.A(v) => { f(v); } E.B => { }, _ => { } } }",
        )
        .unwrap();
        let Stmt::Match { arms, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        assert_eq!(arms.len(), 3);
        assert!(matches!(&arms[0].0,
            Pattern::Variant { args: PatArgs::Single(b), .. } if b.name == "v"));
        assert!(matches!(&arms[1].0, Pattern::Variant { args: PatArgs::Bare, .. }));
        assert!(matches!(&arms[2].0, Pattern::Wildcard { .. }));
        // Field patterns: `field: name` binds, a lone `field` is shorthand.
        let p = parse_src(
            "fn main() { match x { E.R { w: a, h } => { } _ => { } } }",
        )
        .unwrap();
        let Stmt::Match { arms, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        let Pattern::Variant { args: PatArgs::Fields(fs), .. } = &arms[0].0 else { panic!() };
        assert_eq!((fs[0].0.as_str(), fs[0].1.name.as_str()), ("w", "a"));
        assert_eq!((fs[1].0.as_str(), fs[1].1.name.as_str()), ("h", "h"));
        // Qualified variant struct literals parse in expression position...
        let p = parse_src("fn main() { let e = E.R { w: 1.0, h: 2.0 }; }").unwrap();
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        assert!(matches!(&init.kind, ExprKind::VariantStructLit { fields, .. }
            if fields.len() == 2));
        // ...but not unparenthesized in a condition (same rule as struct
        // literals): `E.R {` there reads the `{` as the body.
        assert!(parse_src("fn main() { if x == E.R { w: 1.0 } { } }").is_err());
        parse_src("fn main() { if (x == E.R { w: 1.0 }) { } }").unwrap();
        // Expression position: arms are comma-separated expressions.
        let p = parse_src(
            "fn main() { let x = match n { 0 => a, -1 => b, b'c' => c, true => d, _ => e, }; }",
        )
        .unwrap();
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        let ExprKind::Match { arms, .. } = &init.kind else { panic!() };
        assert_eq!(arms.len(), 5);
        assert!(matches!(&arms[1].0, Pattern::IntLit { neg: true, digits: 1, .. }));
        assert!(matches!(&arms[2].0, Pattern::ByteLit { value: b'c', .. }));
        assert!(matches!(&arms[3].0, Pattern::BoolLit { value: true, .. }));
        // Works nested in calls and conditions; scrutinee takes no parens.
        parse_src("fn main() { f(match x { _ => 1 }); }").unwrap();
        parse_src("fn main() { if (match x { _ => true }) { } }").unwrap();
        // A statement starting with `match` is the match statement, so a
        // tail match-expression needs parens (same rule as `if`).
        let p = parse_src("fn f(): int { (match x { _ => 1 }) }  fn main() { }").unwrap();
        assert!(matches!(p.funcs[0].body.stmts[0], Stmt::Return { .. }));
        assert!(parse_err("fn f(): int { match x { _ => 1 } }  fn main() { }")
            .contains("expected '{'"));
        // Pattern errors.
        assert!(parse_err("fn main() { match x { A => { } } }").contains("expected '.'"));
        assert!(parse_err("fn main() { match x { 1.5 => { } } }").contains("expected a pattern"));
        assert!(parse_err("fn main() { match x { -x => { } } }")
            .contains("expected an integer literal after '-'"));
        assert!(parse_err("fn main() { match x { E.A(1) => { } } }")
            .contains("expected identifier"));
        assert!(parse_err("fn main() { let y = match x { 1 => 2 3 => 4 }; }")
            .contains("expected '}'"));
    }

    #[test]
    fn impl_blocks() {
        let p = parse_src(
            r#"
struct P { x: int }
impl P {
    fn mk(v: int): P { P { x: v } }
    fn get(self): int { self.x }
}
impl Pair<T> { fn first(self): T { self.a } }
fn main() { }
"#,
        )
        .unwrap();
        // Methods flatten into the function list as `Type.name`; a `self`
        // receiver becomes a synthesized first parameter of the impl type.
        assert_eq!(p.funcs[0].name, "P.mk");
        assert!(p.funcs[0].is_method && !p.methods[0].has_self);
        assert_eq!(p.funcs[1].name, "P.get");
        assert_eq!(p.funcs[1].params[0].0, "self");
        assert!(matches!(&p.funcs[1].params[0].1, TypeExpr::Named(n, a, _)
            if n == "P" && a.is_empty()));
        assert!(p.methods[1].has_self);
        // The impl's type parameters are prepended to the method's own.
        assert_eq!(p.funcs[2].type_params, vec!["T"]);
        assert_eq!(p.methods[2].impl_type_params, 1);
        assert!(matches!(&p.funcs[2].params[0].1, TypeExpr::Named(n, a, _)
            if n == "Pair" && a.len() == 1));
        // `self` is a keyword: usable in expressions, not as a binder.
        assert!(parse_err("impl P { let x = 1; } fn main() { }")
            .contains("expected 'fn' inside an impl block"));
        assert!(parse_err("impl P { fn f(self: P) { } } fn main() { }")
            .contains("'self' takes no type annotation"));
        assert!(parse_err("impl P { fn f(a: int, self) { } } fn main() { }")
            .contains("'self' must be the first parameter"));
        assert!(parse_err("fn f(self) { } fn main() { }").contains("expected identifier"));
        assert!(parse_err("fn main() { let self = 1; }").contains("expected identifier"));
        assert!(parse_err("impl P { fn f(self) }").contains("expected"));
    }

    #[test]
    fn struct_literal_heuristic() {
        // `Ident {` is a struct literal when followed by `}` or `ident:`.
        let p = parse_src("fn main() { let p = P { x: 1 }; }").unwrap();
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        assert!(matches!(init.kind, ExprKind::StructLit { .. }));
        // ...but an expression statement followed by a block is not.
        let p = parse_src("fn main() { x; { let y = 1; } }").unwrap();
        assert!(matches!(p.funcs[0].body.stmts[1], Stmt::Block(_)));
    }

    #[test]
    fn parenless_conditions() {
        // if/while conditions need no parens; `ident {` starts the body.
        let p = parse_src("fn main() { if x {} while x { y = 1; } }").unwrap();
        assert!(matches!(p.funcs[0].body.stmts[0], Stmt::If { .. }));
        assert!(matches!(p.funcs[0].body.stmts[1], Stmt::While { .. }));
        parse_src("fn main() { if a == 1 {} else if b {} else {} }").unwrap();
        // Parens still work as plain grouping.
        parse_src("fn main() { if (a && b) {} while (x) {} }").unwrap();
        // A struct literal in a condition needs parens...
        parse_src("fn main() { if (p == Point { x: 1 }) {} }").unwrap();
        // ...and the restriction lifts inside brackets and argument lists.
        parse_src("fn main() { if f(Point { x: 1 }) {} }").unwrap();
        parse_src("fn main() { while xs[Point { x: 1 }.x] {} }").unwrap();
        // Unparenthesized, `Point {` is read as condition + body.
        assert!(parse_src("fn main() { if p == Point { x: 1 } {} }").is_err());
    }

    #[test]
    fn cast_precedence() {
        // `as` binds tighter than `*` and looser than unary `-`.
        let p = parse_src("fn main() { let a = x as u8 * 2; let b = -x as i8; }").unwrap();
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        let ExprKind::Binary(crate::ast::BinOp::Mul, lhs, _) = &init.kind else { panic!() };
        assert!(matches!(lhs.kind, ExprKind::Cast(..)));
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[1] else { panic!() };
        let ExprKind::Cast(inner, _) = &init.kind else { panic!() };
        assert!(matches!(inner.kind, ExprKind::Unary(..)));
    }

    #[test]
    fn postfix_chains() {
        let p = parse_src("fn main() { let v = a.b[0].c(1)(2); }").unwrap();
        let Stmt::Let { init, .. } = &p.funcs[0].body.stmts[0] else { panic!() };
        // Outermost is the trailing call.
        assert!(matches!(init.kind, ExprKind::Call { .. }));
    }

    #[test]
    fn depth_limit() {
        // Pathological nesting reports a clean error instead of overflowing
        // the parser's stack. Run on a thread with the same 8 MiB the real
        // binary's main thread gets (test threads default to 2 MiB, which
        // MAX_DEPTH deliberately does not target).
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(depth_limit_cases)
            .unwrap()
            .join()
            .unwrap();
    }

    fn depth_limit_cases() {
        let deep = |n: usize| format!("fn main() {{ let x = {}1{}; }}", "(".repeat(n), ")".repeat(n));
        assert!(parse_err(&deep(100_000)).contains("nested too deeply"));
        assert!(parse_src(&deep(80)).is_ok());
        let unary = format!("fn main() {{ let b = {}true; }}", "!".repeat(100_000));
        assert!(parse_err(&unary).contains("nested too deeply"));
        let blocks = format!("fn main() {{ {} {} }}", "{".repeat(100_000), "}".repeat(100_000));
        assert!(parse_err(&blocks).contains("nested too deeply"));
        let ty = format!("fn main() {{ let x: {}int{} = y; }}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse_err(&ty).contains("nested too deeply"));
        // The depth counter unwinds correctly: deep-but-legal nesting in one
        // statement doesn't eat budget from the next.
        let two = format!(
            "fn main() {{ let x = {p}1{q}; let y = {p}2{q}; }}",
            p = "(".repeat(80),
            q = ")".repeat(80)
        );
        assert!(parse_src(&two).is_ok());
    }

    #[test]
    fn errors() {
        assert!(parse_err("fn main() { let x = 1 }").contains("expected ';'"));
        assert!(parse_err("fn main() { 1 = 2; }").contains("invalid assignment target"));
        assert!(parse_err("fn main() { let x = ; }").contains("expected an expression"));
        assert!(parse_err("fn main() { let x: = 1; }").contains("expected a type"));
        assert!(parse_err("fn main() {").contains("expected '}'"));
        assert!(parse_err("let x = 1;")
            .contains("expected 'fn', 'struct', 'enum', or 'impl' at top level"));
        assert!(parse_err("fn () {}").contains("expected identifier"));
        assert!(parse_err("fn main( {}").contains("expected identifier"));
        assert!(parse_err("struct P { x int }").contains("expected ':'"));
        assert!(parse_err("fn main() { let x = (1; }").contains("expected ')'"));
    }
}
