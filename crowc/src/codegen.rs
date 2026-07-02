//! Cranelift code generation.
//!
//! # GC discipline (stack maps)
//!
//! The runtime GC is precise and moving (nursery evacuation), so compiled
//! code must guarantee that at every *safepoint* (any call that can
//! allocate) all live references are recoverable — and rewritable — by the
//! collector. Reference-typed variables and values are declared with
//! Cranelift's `declare_var_needs_stack_map` / `declare_value_needs_stack_map`;
//! the frontend then spills live references to stack slots around every call
//! and rewrites later uses into reloads, and each call site gets a stack map
//! of SP-relative root offsets. We serialize those maps into a
//! `crow_stackmaps` data section keyed by return address, which the runtime
//! walks via the frame-pointer chain at collection time.
//!
//! One invariant is maintained by construction: derived interior pointers
//! (field or element addresses) are never live across a safepoint — they are
//! always computed immediately before a plain store/load or a call to the
//! non-allocating `crow_write_ref`.

use crate::ast::*;
use crate::mono::{canonical, instantiate, shape_of, suffix, Shape};
use crate::types::{layout_fields, IntKind, Type as CrowType};
use crate::typeck::Checked;
use std::collections::HashMap;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, AbiParam, Block as IrBlock, InstBuilder, MemFlags, Signature, TrapCode, Value,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};

const HEADER: i32 = 16;
const FLAG_STATIC: i64 = 4;
const KIND_STRUCT: u64 = 0;
// Array object layout: { buf @16, len @24, cap @32 } (see runtime).
const ARR_BUF: i32 = HEADER;
const ARR_LEN: i32 = HEADER + 8;

pub fn compile(program: &Program, checked: &Checked) -> Result<Vec<u8>, String> {
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").unwrap();
    flags.set("is_pic", "true").unwrap();
    // The stack-map walker needs intact frame records in compiled code.
    flags.set("preserve_frame_pointers", "true").unwrap();
    // `Triple::host()` reports macOS as `Darwin`, which cranelift-object
    // turns into PLATFORM_UNKNOWN in the Mach-O build-version load command
    // and Apple's linker then rejects. Report `MacOSX` instead.
    let mut triple = target_lexicon::Triple::host();
    if let target_lexicon::OperatingSystem::Darwin(v) = triple.operating_system {
        triple.operating_system = target_lexicon::OperatingSystem::MacOSX(v);
    }
    let mut isa_builder = cranelift_codegen::isa::lookup(triple)
        .map_err(|e| format!("unsupported host machine: {e}"))?;
    cranelift_native::infer_native_flags(&mut isa_builder)
        .map_err(|e| format!("cannot detect host CPU features: {e}"))?;
    let isa = isa_builder
        .finish(settings::Flags::new(flags))
        .map_err(|e| format!("failed to configure codegen: {e}"))?;
    let builder = ObjectBuilder::new(isa, "crow", cranelift_module::default_libcall_names())
        .map_err(|e| e.to_string())?;
    let module = ObjectModule::new(builder);

    let mut cg = Codegen {
        module,
        program,
        checked,
        stackmaps: Vec::new(),
        rt: Vec::new(),
        func_ids: Vec::new(),
        thunk_ids: Vec::new(),
        fnval_ids: Vec::new(),
        inst_ids: HashMap::new(),
        struct_insts: HashMap::new(),
        desc_cache: HashMap::new(),
        worklist: Vec::new(),
        closure0_desc: None,
        desc_string: None,
        stack_limit: None,
        str_count: 0,
    };
    cg.declare_runtime()?;
    cg.closure0_desc = Some(cg.desc(KIND_STRUCT, 8, 0)?);
    cg.declare_functions(program)?;
    cg.define_thunks(program)?;
    cg.define_fnvals(program)?;
    // Every monomorphic function compiles unconditionally; generic functions
    // compile per instantiation shape, discovered from call sites while the
    // worklist drains.
    for (i, f) in program.funcs.iter().enumerate() {
        if f.type_params.is_empty() {
            let func_id = cg.func_ids[i].unwrap();
            cg.worklist.push(Work { func_id, fid: i as u32, shapes: Vec::new() });
        }
    }
    cg.run_worklist()?;
    cg.define_main_wrapper(program)?;
    let product = cg.module.finish();
    product.emit().map_err(|e| e.to_string())
}

/// Collect every lambda in a block, recursively (including lambdas nested
/// inside other lambdas). Exhaustive on statement kinds on purpose: a missed
/// kind here means lambdas inside it never get compiled.
fn collect_lambdas(block: &Block) -> Vec<&LambdaDef> {
    fn walk_block<'a>(b: &'a Block, out: &mut Vec<&'a LambdaDef>) {
        for s in &b.stmts {
            walk_stmt(s, out);
        }
    }
    fn walk_stmt<'a>(s: &'a Stmt, out: &mut Vec<&'a LambdaDef>) {
        match s {
            Stmt::Let { init, .. } => walk_expr(init, out),
            Stmt::Assign { target, value, .. } => {
                walk_expr(target, out);
                walk_expr(value, out);
            }
            Stmt::Expr(e) => walk_expr(e, out),
            Stmt::If { cond, then, els } => {
                walk_expr(cond, out);
                walk_block(then, out);
                if let Some(els) = els {
                    walk_block(els, out);
                }
            }
            Stmt::While { cond, body } => {
                walk_expr(cond, out);
                walk_block(body, out);
            }
            Stmt::For { init, cond, step, body } => {
                if let Some(s) = init {
                    walk_stmt(s, out);
                }
                if let Some(c) = cond {
                    walk_expr(c, out);
                }
                if let Some(s) = step {
                    walk_stmt(s, out);
                }
                walk_block(body, out);
            }
            Stmt::Return { value: Some(v), .. } => walk_expr(v, out),
            Stmt::Block(b) => walk_block(b, out),
            Stmt::Return { value: None, .. } | Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }
    fn walk_expr<'a>(e: &'a Expr, out: &mut Vec<&'a LambdaDef>) {
        match &e.kind {
            ExprKind::Unary(_, a) => walk_expr(a, out),
            ExprKind::Binary(_, a, b) => {
                walk_expr(a, out);
                walk_expr(b, out);
            }
            ExprKind::If { cond, then, els } => {
                walk_expr(cond, out);
                walk_expr(then, out);
                walk_expr(els, out);
            }
            ExprKind::Call { callee, args, .. } => {
                walk_expr(callee, out);
                for a in args {
                    walk_expr(a, out);
                }
            }
            ExprKind::Builtin(_, args) => {
                for a in args {
                    walk_expr(a, out);
                }
            }
            ExprKind::Field { obj, .. } => walk_expr(obj, out),
            ExprKind::Index(a, b) => {
                walk_expr(a, out);
                walk_expr(b, out);
            }
            ExprKind::Cast(a, _) => walk_expr(a, out),
            ExprKind::ArrayLit(els) => {
                for el in els {
                    walk_expr(el, out);
                }
            }
            ExprKind::StructLit { fields, .. } => {
                for (_, v, _) in fields {
                    walk_expr(v, out);
                }
            }
            ExprKind::Lambda(lam) => {
                out.push(lam);
                walk_block(&lam.body, out);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk_block(block, &mut out);
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Rt {
    RtInit,
    RtRegisterMaps,
    RtExit,
    Alloc,
    WriteRef,
    GcCollect,
    StrConcat,
    StrEq,
    Itos,
    Utos,
    Ftos,
    Stoi,
    Stof,
    Stob,
    Btos,
    PrintInt,
    PrintUint,
    PrintFloat,
    PrintBool,
    PrintStr,
    PrintNewline,
    ArrayNew,
    ArrayPush,
    ArrayPop,
    PanicBounds,
    PanicNull,
    PanicDiv,
    PanicOverflow,
    PanicShift,
    PanicCast,
    PanicStack,
    AssertFail,
}

const I: u8 = 0; // i64 param
const F: u8 = 1; // f64 param

/// (enum tag, symbol, param kinds, result kind)
const RT_FUNCS: &[(Rt, &str, &[u8], Option<u8>)] = &[
    (Rt::RtInit, "crow_rt_init", &[I], None),
    (Rt::RtRegisterMaps, "crow_rt_register_stackmaps", &[I], None),
    (Rt::RtExit, "crow_rt_exit", &[I], None),
    (Rt::Alloc, "crow_alloc", &[I, I], Some(I)),
    (Rt::WriteRef, "crow_write_ref", &[I, I, I], None),
    (Rt::GcCollect, "crow_gc_collect", &[I], None),
    (Rt::StrConcat, "crow_str_concat", &[I, I], Some(I)),
    (Rt::StrEq, "crow_str_eq", &[I, I], Some(I)),
    (Rt::Itos, "crow_itos", &[I], Some(I)),
    (Rt::Utos, "crow_utos", &[I], Some(I)),
    (Rt::Ftos, "crow_ftos", &[F], Some(I)),
    (Rt::Stoi, "crow_stoi", &[I, I], Some(I)),
    (Rt::Stof, "crow_stof", &[I, I], Some(F)),
    (Rt::Stob, "crow_stob", &[I], Some(I)),
    (Rt::Btos, "crow_btos", &[I, I], Some(I)),
    (Rt::PrintInt, "crow_print_int", &[I], None),
    (Rt::PrintUint, "crow_print_uint", &[I], None),
    (Rt::PrintFloat, "crow_print_float", &[F], None),
    (Rt::PrintBool, "crow_print_bool", &[I], None),
    (Rt::PrintStr, "crow_print_str", &[I], None),
    (Rt::PrintNewline, "crow_print_newline", &[], None),
    (Rt::ArrayNew, "crow_array_new", &[I, I, I], Some(I)),
    (Rt::ArrayPush, "crow_array_push", &[I, I, I, I], None),
    (Rt::ArrayPop, "crow_array_pop", &[I, I], Some(I)),
    (Rt::PanicBounds, "crow_panic_bounds", &[I, I, I], None),
    (Rt::PanicNull, "crow_panic_null", &[I], None),
    (Rt::PanicDiv, "crow_panic_div", &[I], None),
    (Rt::PanicOverflow, "crow_panic_overflow", &[I], None),
    (Rt::PanicShift, "crow_panic_shift", &[I], None),
    (Rt::PanicCast, "crow_panic_cast", &[I], None),
    (Rt::PanicStack, "crow_panic_stack", &[I], None),
    (Rt::AssertFail, "crow_assert_fail", &[I], None),
];

/// Layout and GC descriptor of one struct instantiation. Keyed by argument
/// *shapes*, so all reference instantiations of a generic struct share one
/// entry (and one descriptor).
#[derive(Clone)]
struct StructInst {
    offsets: Vec<u32>,
    desc: DataId,
}

/// A declared-but-not-yet-compiled function instantiation.
struct Work {
    func_id: FuncId,
    fid: u32,
    shapes: Vec<Shape>,
}

struct Codegen<'a> {
    module: ObjectModule,
    program: &'a Program,
    checked: &'a Checked,
    /// (function, return-address offset, frame size below the frame record,
    /// SP-relative root offsets) per safepoint with live references;
    /// collected after each function is compiled.
    stackmaps: Vec<(FuncId, u32, u32, Vec<u32>)>,
    rt: Vec<FuncId>,
    /// Declared symbols per *monomorphic* top-level function; None for
    /// generic definitions (those live in `inst_ids`).
    func_ids: Vec<Option<FuncId>>,
    thunk_ids: Vec<Option<FuncId>>,
    /// Static closure objects, one per monomorphic function used as a value.
    fnval_ids: Vec<Option<DataId>>,
    /// Generic function instantiations, keyed by (function id, arg shapes).
    inst_ids: HashMap<(u32, Vec<Shape>), FuncId>,
    /// Struct layouts + descriptors, keyed by (struct id, arg shapes).
    struct_insts: HashMap<(u32, Vec<Shape>), StructInst>,
    /// Descriptors are pure (kind, size, refmap) data, so every object shape
    /// shares one descriptor no matter which struct, closure, or generic
    /// instantiation produced it.
    desc_cache: HashMap<(u64, u64, u64), DataId>,
    worklist: Vec<Work>,
    closure0_desc: Option<DataId>,
    desc_string: Option<DataId>,
    /// Address of the runtime's `crow_stack_limit` global, loaded by every
    /// function prologue's stack-overflow check.
    stack_limit: Option<DataId>,
    str_count: u32,
}

fn abi_ty(t: &CrowType) -> types::Type {
    if *t == CrowType::Float {
        types::F64
    } else {
        types::I64
    }
}

impl<'a> Codegen<'a> {
    fn declare_runtime(&mut self) -> Result<(), String> {
        for (_, name, params, ret) in RT_FUNCS {
            let mut sig = self.module.make_signature();
            for &p in *params {
                sig.params.push(AbiParam::new(if p == F { types::F64 } else { types::I64 }));
            }
            if let Some(r) = ret {
                sig.returns.push(AbiParam::new(if *r == F { types::F64 } else { types::I64 }));
            }
            let id = self
                .module
                .declare_function(name, Linkage::Import, &sig)
                .map_err(|e| e.to_string())?;
            self.rt.push(id);
        }
        let id = self
            .module
            .declare_data("crow_desc_string", Linkage::Import, false, false)
            .map_err(|e| e.to_string())?;
        self.desc_string = Some(id);
        let id = self
            .module
            .declare_data("crow_stack_limit", Linkage::Import, false, false)
            .map_err(|e| e.to_string())?;
        self.stack_limit = Some(id);
        Ok(())
    }

    /// The descriptor for an object shape, defined once per distinct
    /// (kind, size, refmap) triple and shared by everything that matches.
    ///
    /// Sharing is sound only while descriptor identity carries no *type*
    /// information: the runtime reads descriptors purely as GC metadata, and
    /// object equality is pointer equality on objects. If Crow ever grows
    /// runtime type information (downcasting, reflection, dynamic dispatch),
    /// do NOT key type identity on descriptors — either un-share them here
    /// or add a separate type-id table alongside the descriptor pointer.
    fn desc(&mut self, kind: u64, size: u64, refmap: u64) -> Result<DataId, String> {
        if let Some(&id) = self.desc_cache.get(&(kind, size, refmap)) {
            return Ok(id);
        }
        let name = format!("crow_desc.k{kind}s{size}r{refmap:x}");
        let id = self.define_desc(&name, kind, size, refmap)?;
        self.desc_cache.insert((kind, size, refmap), id);
        Ok(id)
    }

    fn define_desc(&mut self, name: &str, kind: u64, size: u64, refmap: u64) -> Result<DataId, String> {
        let mut bytes = Vec::with_capacity(24);
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&refmap.to_le_bytes());
        let id = self
            .module
            .declare_data(name, Linkage::Local, false, false)
            .map_err(|e| e.to_string())?;
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        desc.set_align(16);
        self.module.define_data(id, &desc).map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// Layout + descriptor of a struct instantiation, computed on first use.
    /// The cache key is the argument *shape* vector: layout depends only on
    /// field sizes and refness, so type arguments with equal shapes share
    /// the entry.
    fn struct_inst(&mut self, sid: u32, targs: &[CrowType]) -> Result<StructInst, String> {
        let key = (sid, targs.iter().map(shape_of).collect::<Vec<_>>());
        if let Some(si) = self.struct_insts.get(&key) {
            return Ok(si.clone());
        }
        let canon: Vec<CrowType> = key.1.iter().map(|s| canonical(*s)).collect();
        let info = &self.checked.structs[sid as usize];
        let ftys: Vec<CrowType> = info.fields.iter().map(|(_, t)| t.subst(&canon)).collect();
        let (offsets, payload_size) = layout_fields(&ftys);
        // Fields are packed; the refmap is one bit per 8-byte payload word,
        // and reference fields are always 8-byte aligned.
        let mut refmap = 0u64;
        for (j, ty) in ftys.iter().enumerate() {
            if ty.is_ref() {
                debug_assert_eq!(offsets[j] % 8, 0);
                refmap |= 1 << (offsets[j] / 8);
            }
        }
        let desc = self.desc(KIND_STRUCT, payload_size as u64, refmap)?;
        let si = StructInst { offsets, desc };
        self.struct_insts.insert(key, si.clone());
        Ok(si)
    }

    /// The declared symbol for a generic function instantiation, declaring
    /// it and scheduling its body for compilation on first use.
    fn instance_func_id(&mut self, fid: u32, targs: &[CrowType]) -> Result<FuncId, String> {
        let key = (fid, targs.iter().map(shape_of).collect::<Vec<_>>());
        if let Some(&id) = self.inst_ids.get(&key) {
            return Ok(id);
        }
        let shapes = key.1.clone();
        let canon: Vec<CrowType> = shapes.iter().map(|s| canonical(*s)).collect();
        let sig_info = &self.checked.funcs[fid as usize];
        let params: Vec<CrowType> = sig_info.params.iter().map(|t| t.subst(&canon)).collect();
        let ret = sig_info.ret.subst(&canon);
        let sig = self.make_sig(&params, &ret, false);
        let name = format!("crow_fn.{}{}", self.program.funcs[fid as usize].name, suffix(&shapes));
        let id = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .map_err(|e| e.to_string())?;
        self.inst_ids.insert(key, id);
        self.worklist.push(Work { func_id: id, fid, shapes });
        Ok(id)
    }

    /// Compile until no instantiation remains. The map in `inst_ids` grows
    /// monotonically and instantiations are keyed by shape, so this
    /// terminates even for polymorphic recursion.
    fn run_worklist(&mut self) -> Result<(), String> {
        while let Some(w) = self.worklist.pop() {
            self.compile_work(w)?;
        }
        Ok(())
    }

    fn compile_work(&mut self, w: Work) -> Result<(), String> {
        let def = &self.program.funcs[w.fid as usize];
        let inst = instantiate(w.fid, def, self.checked, w.shapes);
        // Declare this instantiation's lambdas (function + descriptor) so
        // the body can reference them.
        let lams = collect_lambdas(&inst.def.body);
        let sfx = suffix(&inst.shapes);
        let mut lam_ids: HashMap<u32, (FuncId, DataId)> = HashMap::new();
        for lam in &lams {
            let info = &inst.lambdas[&lam.id];
            let sig = self.make_sig(&info.params, &info.ret, true);
            let func_id = self
                .module
                .declare_function(&format!("crow_lambda.{}{sfx}", lam.id), Linkage::Local, &sig)
                .map_err(|e| e.to_string())?;
            let mut refmap = 0u64;
            for (j, c) in lam.captures.iter().enumerate() {
                if c.ty.is_ref() {
                    refmap |= 1 << (j + 1); // field 0 is the code pointer
                }
            }
            let size = 8 * (1 + lam.captures.len() as u64);
            let desc = self.desc(KIND_STRUCT, size, refmap)?;
            lam_ids.insert(lam.id, (func_id, desc));
        }
        let sig = self.make_sig(&inst.params, &inst.ret, false);
        self.compile_body(
            w.func_id,
            sig,
            &inst.locals,
            inst.def.params.len(),
            false,
            &inst.def.body,
            &inst.ret,
            inst.def.line,
            &lam_ids,
        )
        .map_err(|e| format!("in function '{}': {e}", inst.name))?;
        for lam in &lams {
            let info = &inst.lambdas[&lam.id];
            let sig = self.make_sig(&info.params, &info.ret, true);
            let (func_id, _) = lam_ids[&lam.id];
            self.compile_body(
                func_id,
                sig,
                &info.locals,
                lam.params.len(),
                true,
                &lam.body,
                &info.ret,
                lam.line,
                &lam_ids,
            )
            .map_err(|e| format!("in lambda at line {}: {e}", lam.line))?;
        }
        Ok(())
    }

    fn make_sig(&self, params: &[CrowType], ret: &CrowType, with_env: bool) -> Signature {
        let mut sig = self.module.make_signature();
        if with_env {
            sig.params.push(AbiParam::new(types::I64));
        }
        for p in params {
            sig.params.push(AbiParam::new(abi_ty(p)));
        }
        if *ret != CrowType::Unit {
            sig.returns.push(AbiParam::new(abi_ty(ret)));
        }
        sig
    }

    /// Declare monomorphic functions and their thunks upfront so direct
    /// calls can reference them in any order. Generic definitions get no
    /// symbol here; their instantiations are declared on first use.
    fn declare_functions(&mut self, program: &Program) -> Result<(), String> {
        for (i, f) in program.funcs.iter().enumerate() {
            if !f.type_params.is_empty() {
                self.func_ids.push(None);
                self.thunk_ids.push(None);
                continue;
            }
            let s = &self.checked.funcs[i];
            let sig = self.make_sig(&s.params, &s.ret, false);
            let id = self
                .module
                .declare_function(&format!("crow_fn.{}", f.name), Linkage::Local, &sig)
                .map_err(|e| e.to_string())?;
            self.func_ids.push(Some(id));
            let tsig = self.make_sig(&s.params, &s.ret, true);
            let tid = self
                .module
                .declare_function(&format!("crow_thunk.{}", f.name), Linkage::Local, &tsig)
                .map_err(|e| e.to_string())?;
            self.thunk_ids.push(Some(tid));
        }
        Ok(())
    }

    /// Thunks adapt the closure calling convention (extra env argument) to a
    /// direct top-level function call, so functions can be used as values.
    /// Generic functions cannot be used as values (checker-enforced), so
    /// they get no thunk.
    fn define_thunks(&mut self, program: &Program) -> Result<(), String> {
        for i in 0..program.funcs.len() {
            let Some(thunk_id) = self.thunk_ids[i] else { continue };
            let s = &self.checked.funcs[i];
            let sig = self.make_sig(&s.params, &s.ret, true);
            let mut ctx = self.module.make_context();
            ctx.func.signature = sig;
            let mut fb_ctx = FunctionBuilderContext::new();
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let args: Vec<Value> = b.block_params(entry)[1..].to_vec();
            let callee = self.module.declare_func_in_func(self.func_ids[i].unwrap(), b.func);
            let call = b.ins().call(callee, &args);
            let results = b.inst_results(call).to_vec();
            b.ins().return_(&results);
            b.seal_all_blocks();
            b.finalize();
            self.module
                .define_function(thunk_id, &mut ctx)
                .map_err(|e| format!("thunk: {e}"))?;
        }
        Ok(())
    }

    /// One *static* closure object per top-level function, so using a
    /// function name as a value allocates nothing and the same function
    /// always yields the identical value (`f == f` holds). Layout matches a
    /// heap closure: [closure descriptor | STATIC, 0, thunk address].
    fn define_fnvals(&mut self, program: &Program) -> Result<(), String> {
        for i in 0..program.funcs.len() {
            let Some(thunk_id) = self.thunk_ids[i] else {
                self.fnval_ids.push(None);
                continue;
            };
            let id = self
                .module
                .declare_data(&format!("crow_fnval.{i}"), Linkage::Local, true, false)
                .map_err(|e| e.to_string())?;
            let mut desc = DataDescription::new();
            desc.define(vec![0u8; 24].into_boxed_slice());
            desc.set_align(16);
            let gv = self.module.declare_data_in_data(self.closure0_desc.unwrap(), &mut desc);
            desc.write_data_addr(0, gv, FLAG_STATIC);
            let fref = self.module.declare_func_in_data(thunk_id, &mut desc);
            desc.write_function_addr(16, fref);
            self.module.define_data(id, &desc).map_err(|e| e.to_string())?;
            self.fnval_ids.push(Some(id));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_body(
        &mut self,
        func_id: FuncId,
        sig: Signature,
        locals: &[CrowType],
        nparams: usize,
        is_lambda: bool,
        body: &Block,
        ret: &CrowType,
        line: u32,
        lam_ids: &HashMap<u32, (FuncId, DataId)>,
    ) -> Result<(), String> {
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);

        // Locals are Cranelift variables; reference-typed ones are declared
        // as needing stack maps, and Cranelift's safepoint pass spills them
        // around calls and rewrites uses into reloads.
        let mut local_repr = Vec::with_capacity(locals.len());
        for ty in locals {
            let var = b.declare_var(abi_ty(ty));
            if ty.is_ref() {
                b.declare_var_needs_stack_map(var);
            }
            local_repr.push(var);
        }
        let env_var = if is_lambda {
            let var = b.declare_var(types::I64);
            b.declare_var_needs_stack_map(var);
            Some(var)
        } else {
            None
        };

        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);

        let mut fc = FnCompiler {
            cg: self,
            b,
            local_repr,
            env_var,
            loop_stack: Vec::new(),
            terminated: false,
            ret: ret.clone(),
            lam_ids,
        };

        // Move parameters into their homes.
        let params: Vec<Value> = fc.b.block_params(entry).to_vec();
        let off = if is_lambda {
            // Root the environment pointer.
            fc.b.def_var(fc.env_var.unwrap(), params[0]);
            1
        } else {
            0
        };
        for i in 0..nparams {
            fc.b.def_var(fc.local_repr[i], params[i + off]);
        }

        // Stack-overflow guard: panic cleanly when SP dips below the limit
        // the runtime computed at startup (stack bottom + slack). The limit
        // is 0 until crow_rt_init runs, so the check passes trivially in
        // code that executes before initialization. Leaf functions (no calls
        // into compiled Crow code) cannot deepen the call chain; their
        // bounded frames are covered by the slack, so they skip the check.
        if makes_crow_calls(body) {
            let gv = fc.cg.module.declare_data_in_func(fc.cg.stack_limit.unwrap(), fc.b.func);
            let lim_addr = fc.b.ins().symbol_value(types::I64, gv);
            let lim = fc.b.ins().load(types::I64, MemFlags::trusted(), lim_addr, 0);
            let sp = fc.b.ins().get_stack_pointer(types::I64);
            let low = fc.b.ins().icmp(IntCC::UnsignedLessThan, sp, lim);
            fc.panic_if(low, Rt::PanicStack, line);
        }

        fc.gen_block(body)?;
        if !fc.terminated {
            if *ret == CrowType::Unit {
                fc.b.ins().return_(&[]);
            } else {
                // The checker guarantees this is unreachable.
                fc.b.ins().trap(TrapCode::user(1).unwrap());
            }
        }

        fc.b.seal_all_blocks();
        fc.b.finalize();
        if std::env::var("CROW_DUMP_CLIF").is_ok() {
            eprintln!("=== {func_id:?} ===\n{}", ctx.func.display());
        }
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| format!("codegen: {e}"))?;
        let compiled = ctx.compiled_code().expect("just compiled");
        for (ret_off, span, map) in compiled.buffer.user_stack_maps() {
            let offs: Vec<u32> = map.entries().map(|(_ty, off)| off).collect();
            if !offs.is_empty() {
                self.stackmaps.push((func_id, *ret_off, *span, offs));
            }
        }
        Ok(())
    }

    /// Serialize the collected stack maps into a data section. See
    /// `crow_rt_register_stackmaps` in the runtime for the layout.
    fn define_stackmap_table(&mut self) -> Result<DataId, String> {
        let n = self.stackmaps.len();
        let slots: Vec<u32> =
            self.stackmaps.iter().flat_map(|(_, _, _, s)| s.iter().copied()).collect();
        let mut bytes = vec![0u8; 8 + 40 * n + 8 * slots.len()];
        bytes[0..8].copy_from_slice(&(n as u64).to_le_bytes());
        let mut slot_start = 0u64;
        for (i, (_, ret_off, span, offs)) in self.stackmaps.iter().enumerate() {
            let base = 8 + 40 * i;
            // bytes[base..base+8] stay zero: the function address relocation
            // is added below.
            bytes[base + 8..base + 16].copy_from_slice(&(*ret_off as u64).to_le_bytes());
            bytes[base + 16..base + 24].copy_from_slice(&(*span as u64).to_le_bytes());
            bytes[base + 24..base + 32].copy_from_slice(&slot_start.to_le_bytes());
            bytes[base + 32..base + 40].copy_from_slice(&(offs.len() as u64).to_le_bytes());
            slot_start += offs.len() as u64;
        }
        for (j, off) in slots.iter().enumerate() {
            let base = 8 + 40 * n + 8 * j;
            bytes[base..base + 8].copy_from_slice(&(*off as u64).to_le_bytes());
        }
        let id = self
            .module
            .declare_data("crow_stackmaps", Linkage::Local, false, false)
            .map_err(|e| e.to_string())?;
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        desc.set_align(8);
        let mut frefs = std::collections::HashMap::new();
        for (i, (func_id, _, _, _)) in self.stackmaps.iter().enumerate() {
            let fref = *frefs
                .entry(*func_id)
                .or_insert_with(|| self.module.declare_func_in_data(*func_id, &mut desc));
            desc.write_function_addr((8 + 40 * i) as u32, fref);
        }
        self.module.define_data(id, &desc).map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// The real `main`: initialize the runtime (passing our frame pointer as
    /// the stack walk boundary), register stack maps, run the user main, exit.
    fn define_main_wrapper(&mut self, program: &Program) -> Result<(), String> {
        let stackmap_table = self.define_stackmap_table()?;
        let user_main = program
            .funcs
            .iter()
            .position(|f| f.name == "main")
            .expect("checker verified main exists");
        let mut sig = self.module.make_signature();
        sig.returns.push(AbiParam::new(types::I32));
        let main_id = self
            .module
            .declare_function("main", Linkage::Export, &sig)
            .map_err(|e| e.to_string())?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        let entry = b.create_block();
        b.switch_to_block(entry);
        let init = self.module.declare_func_in_func(self.rt[rt_index(Rt::RtInit)], b.func);
        let fp = b.ins().get_frame_pointer(types::I64);
        b.ins().call(init, &[fp]);
        let gv = self.module.declare_data_in_func(stackmap_table, b.func);
        let addr = b.ins().global_value(types::I64, gv);
        let reg = self
            .module
            .declare_func_in_func(self.rt[rt_index(Rt::RtRegisterMaps)], b.func);
        b.ins().call(reg, &[addr]);
        let um = self.module.declare_func_in_func(self.func_ids[user_main].unwrap(), b.func);
        b.ins().call(um, &[]);
        let exit = self.module.declare_func_in_func(self.rt[rt_index(Rt::RtExit)], b.func);
        let zero = b.ins().iconst(types::I64, 0);
        b.ins().call(exit, &[zero]);
        let r = b.ins().iconst(types::I32, 0);
        b.ins().return_(&[r]);
        b.seal_all_blocks();
        b.finalize();
        self.module.define_function(main_id, &mut ctx).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Define a string literal as a static, pre-built heap object.
    fn define_string_literal(&mut self, s: &str) -> Result<DataId, String> {
        let n = self.str_count;
        self.str_count += 1;
        let bytes_len = s.len();
        let padded = (bytes_len + 7) & !7;
        let mut data = Vec::with_capacity(16 + padded);
        data.extend_from_slice(&0u64.to_le_bytes()); // patched with desc | STATIC
        data.extend_from_slice(&(bytes_len as u64).to_le_bytes());
        data.extend_from_slice(s.as_bytes());
        data.resize(16 + padded, 0);
        let id = self
            .module
            .declare_data(&format!("crow_str.{n}"), Linkage::Local, true, false)
            .map_err(|e| e.to_string())?;
        let mut desc = DataDescription::new();
        desc.define(data.into_boxed_slice());
        desc.set_align(16);
        let gv = self.module.declare_data_in_data(self.desc_string.unwrap(), &mut desc);
        desc.write_data_addr(0, gv, FLAG_STATIC);
        self.module.define_data(id, &desc).map_err(|e| e.to_string())?;
        Ok(id)
    }
}

fn rt_index(rt: Rt) -> usize {
    RT_FUNCS.iter().position(|(r, ..)| *r == rt).unwrap()
}

/// Does this block call into compiled Crow code (directly or through a
/// function value)? Builtins don't count: the runtime never re-enters Crow
/// code, so its stack use is bounded. Lambda bodies are compiled as separate
/// functions with their own prologue check and are not descended into.
fn makes_crow_calls(block: &Block) -> bool {
    fn in_expr(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Call { .. } => true,
            ExprKind::Unary(_, a) | ExprKind::Cast(a, _) | ExprKind::Field { obj: a, .. } => {
                in_expr(a)
            }
            ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => in_expr(a) || in_expr(b),
            ExprKind::If { cond, then, els } => {
                in_expr(cond) || in_expr(then) || in_expr(els)
            }
            ExprKind::Builtin(_, args) | ExprKind::ArrayLit(args) => args.iter().any(in_expr),
            ExprKind::StructLit { fields, .. } => fields.iter().any(|(_, v, _)| in_expr(v)),
            ExprKind::Lambda(_)
            | ExprKind::Int(_)
            | ExprKind::Byte(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Nil
            | ExprKind::Var { .. } => false,
        }
    }
    fn in_stmt(s: &Stmt) -> bool {
        match s {
            Stmt::Let { init, .. } => in_expr(init),
            Stmt::Assign { target, value, .. } => in_expr(target) || in_expr(value),
            Stmt::Expr(e) => in_expr(e),
            Stmt::If { cond, then, els } => {
                in_expr(cond)
                    || makes_crow_calls(then)
                    || els.as_ref().is_some_and(makes_crow_calls)
            }
            Stmt::While { cond, body } => in_expr(cond) || makes_crow_calls(body),
            Stmt::For { init, cond, step, body } => {
                init.as_deref().is_some_and(in_stmt)
                    || cond.as_ref().is_some_and(in_expr)
                    || step.as_deref().is_some_and(in_stmt)
                    || makes_crow_calls(body)
            }
            Stmt::Return { value, .. } => value.as_ref().is_some_and(in_expr),
            Stmt::Break(_) | Stmt::Continue(_) => false,
            Stmt::Block(b) => makes_crow_calls(b),
        }
    }
    block.stmts.iter().any(in_stmt)
}

/// A compiled expression value. References are plain SSA values declared as
/// needing stack maps; Cranelift handles their spills and reloads.
#[derive(Clone, Copy)]
enum CV {
    Unit,
    Scalar(Value),
}

struct FnCompiler<'a, 'b> {
    cg: &'a mut Codegen<'b>,
    b: FunctionBuilder<'a>,
    local_repr: Vec<Variable>,
    /// The closure environment pointer (lambdas only).
    env_var: Option<Variable>,
    /// (break target, continue target)
    loop_stack: Vec<(IrBlock, IrBlock)>,
    terminated: bool,
    ret: CrowType,
    /// (function, descriptor) of every lambda in the current instantiation,
    /// keyed by the checker-assigned lambda id.
    lam_ids: &'a HashMap<u32, (FuncId, DataId)>,
}

impl FnCompiler<'_, '_> {
    // -- Value helpers ------------------------------------------------------

    fn value_of(&mut self, cv: CV) -> Value {
        match cv {
            CV::Unit => self.b.ins().iconst(types::I64, 0),
            CV::Scalar(v) => v,
        }
    }

    /// Root a freshly produced reference: mark the SSA value as a GC root
    /// and let Cranelift's safepoint pass spill/reload it around calls.
    fn root(&mut self, v: Value) -> CV {
        self.b.declare_value_needs_stack_map(v);
        CV::Scalar(v)
    }

    /// The closure environment pointer.
    fn load_env(&mut self) -> Value {
        self.b.use_var(self.env_var.expect("not inside a lambda"))
    }

    // -- Runtime calls ------------------------------------------------------

    fn call_rt(&mut self, rt: Rt, args: &[Value]) -> Option<Value> {
        let fid = self.cg.rt[rt_index(rt)];
        let fref = self.cg.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, args);
        self.b.inst_results(call).first().copied()
    }

    /// Call a noreturn runtime panic and terminate the block.
    fn emit_panic(&mut self, rt: Rt, args: &[Value]) {
        self.call_rt(rt, args);
        self.b.ins().trap(TrapCode::user(1).unwrap());
    }

    fn line_const(&mut self, line: u32) -> Value {
        self.b.ins().iconst(types::I64, line as i64)
    }

    fn null_check(&mut self, ptr: Value, line: u32) {
        let is_null = self.b.ins().icmp_imm(IntCC::Equal, ptr, 0);
        self.panic_if(is_null, Rt::PanicNull, line);
    }

    /// Branch to a panic call when `cond` is true, then continue.
    fn panic_if(&mut self, cond: Value, rt: Rt, line: u32) {
        let panic_blk = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, panic_blk, &[], cont, &[]);
        self.b.switch_to_block(panic_blk);
        let l = self.line_const(line);
        self.emit_panic(rt, &[l]);
        self.b.switch_to_block(cont);
    }

    // -- Packed memory access ------------------------------------------------

    /// Byte offset of a struct field from the object pointer, for the
    /// instantiation named by `targs`.
    fn field_off(&mut self, sid: u32, targs: &[CrowType], index: u32) -> Result<i32, String> {
        let si = self.cg.struct_inst(sid, targs)?;
        Ok(HEADER + si.offsets[index as usize] as i32)
    }

    /// A struct field's concrete type in the instantiation named by `targs`.
    fn field_ty(&self, sid: u32, targs: &[CrowType], index: u32) -> CrowType {
        self.cg.checked.structs[sid as usize].fields[index as usize].1.subst(targs)
    }

    fn small_ty(k: IntKind) -> types::Type {
        match k.size() {
            1 => types::I8,
            2 => types::I16,
            _ => types::I32,
        }
    }

    /// Load a value of Crow type `ty` from memory, extending sized integers
    /// and bools to the canonical 64-bit register form.
    fn load_typed(&mut self, ty: &CrowType, base: Value, off: i32) -> Value {
        match ty {
            CrowType::Int(k) if k.size() < 8 => {
                let v = self.b.ins().load(Self::small_ty(*k), MemFlags::trusted(), base, off);
                if k.signed() {
                    self.b.ins().sextend(types::I64, v)
                } else {
                    self.b.ins().uextend(types::I64, v)
                }
            }
            CrowType::Bool => {
                let v = self.b.ins().load(types::I8, MemFlags::trusted(), base, off);
                self.b.ins().uextend(types::I64, v)
            }
            _ => self.b.ins().load(abi_ty(ty), MemFlags::trusted(), base, off),
        }
    }

    /// Store a value of Crow type `ty`, truncating sized integers and bools
    /// to their storage width.
    fn store_typed(&mut self, ty: &CrowType, val: Value, base: Value, off: i32) {
        match ty {
            CrowType::Int(k) if k.size() < 8 => {
                match k.size() {
                    1 => self.b.ins().istore8(MemFlags::trusted(), val, base, off),
                    2 => self.b.ins().istore16(MemFlags::trusted(), val, base, off),
                    _ => self.b.ins().istore32(MemFlags::trusted(), val, base, off),
                };
            }
            CrowType::Bool => {
                self.b.ins().istore8(MemFlags::trusted(), val, base, off);
            }
            _ => {
                self.b.ins().store(MemFlags::trusted(), val, base, off);
            }
        }
    }

    // -- Statements ---------------------------------------------------------

    fn gen_block(&mut self, block: &Block) -> Result<(), String> {
        for stmt in &block.stmts {
            if self.terminated {
                break; // unreachable code after return/break/continue
            }
            self.gen_stmt(stmt)?;
        }
        Ok(())
    }

    fn gen_stmt(&mut self, stmt: &Stmt) -> Result<(), String> {
        match stmt {
            Stmt::Let { init, local, .. } => {
                let cv = self.gen_expr(init)?;
                self.assign_local(*local, cv);
            }
            Stmt::Assign { target, op, value, line } => match &target.kind {
                ExprKind::Var { res: Some(VarRes::Local(idx)), .. } => {
                    let v = match op {
                        None => {
                            let cv = self.gen_expr(value)?;
                            self.value_of(cv)
                        }
                        Some(op) => {
                            let cur = self.b.use_var(self.local_repr[*idx as usize]);
                            let cv = self.gen_expr(value)?;
                            let rv = self.value_of(cv);
                            self.gen_arith_values(*op, &target.ty, cur, rv, *line)
                        }
                    };
                    self.b.def_var(self.local_repr[*idx as usize], v);
                }
                ExprKind::Field { obj, index, .. } => {
                    let (sid, targs) = match &obj.ty {
                        CrowType::Struct(s, a) => (*s, a.clone()),
                        _ => unreachable!("checker validated field access"),
                    };
                    let fty = self.field_ty(sid, &targs, *index);
                    let off = self.field_off(sid, &targs, *index)?;
                    let (obj_cv, v) = match op {
                        None => {
                            let val = self.gen_expr(value)?;
                            let obj_cv = self.gen_expr(obj)?;
                            let obj_v = self.value_of(obj_cv);
                            self.null_check(obj_v, *line);
                            (obj_cv, self.value_of(val))
                        }
                        Some(op) => {
                            // Compound: the object is evaluated once, its
                            // field read before the right-hand side runs.
                            let obj_cv = self.gen_expr(obj)?;
                            let obj_v = self.value_of(obj_cv);
                            self.null_check(obj_v, *line);
                            let cur = self.load_typed(&fty, obj_v, off);
                            if fty.is_ref() {
                                self.root(cur);
                            }
                            let val = self.gen_expr(value)?;
                            let rv = self.value_of(val);
                            (obj_cv, self.gen_arith_values(*op, &fty, cur, rv, *line))
                        }
                    };
                    let obj_v = self.value_of(obj_cv);
                    if fty.is_ref() {
                        let addr = self.b.ins().iadd_imm(obj_v, off as i64);
                        self.call_rt(Rt::WriteRef, &[obj_v, addr, v]);
                    } else {
                        self.store_typed(&fty, v, obj_v, off);
                    }
                }
                ExprKind::Index(arr, idx) => {
                    let elem_ty = match &arr.ty {
                        CrowType::Array(t) => (**t).clone(),
                        _ => unreachable!("checker validated index targets"),
                    };
                    let (buf, elem_addr, v) = match op {
                        None => {
                            let val = self.gen_expr(value)?;
                            let (buf, elem_addr) = self.gen_index_addr(arr, idx, *line)?;
                            (buf, elem_addr, self.value_of(val))
                        }
                        Some(op) => {
                            // Compound: array and index evaluated once. The
                            // element address is recomputed (and re-checked)
                            // after the right-hand side runs — it may have
                            // allocated (moving the buffer) or popped the
                            // array (shrinking it).
                            let arr_cv = self.gen_expr(arr)?;
                            let idx_v = self.gen_scalar(idx)?;
                            let arr_v = self.value_of(arr_cv);
                            self.null_check(arr_v, *line);
                            let elem_size = elem_ty.size_bytes();
                            let (_, addr) =
                                self.index_addr_checked(arr_v, idx_v, elem_size, *line);
                            let cur = self.load_typed(&elem_ty, addr, 0);
                            if elem_ty.is_ref() {
                                self.root(cur);
                            }
                            let val = self.gen_expr(value)?;
                            let rv = self.value_of(val);
                            let v = self.gen_arith_values(*op, &elem_ty, cur, rv, *line);
                            let arr_v = self.value_of(arr_cv);
                            let (buf, addr) =
                                self.index_addr_checked(arr_v, idx_v, elem_size, *line);
                            (buf, addr, v)
                        }
                    };
                    if elem_ty.is_ref() {
                        self.call_rt(Rt::WriteRef, &[buf, elem_addr, v]);
                    } else {
                        self.store_typed(&elem_ty, v, elem_addr, 0);
                    }
                }
                _ => unreachable!("checker validated assignment targets"),
            },
            Stmt::Expr(e) => {
                self.gen_expr(e)?;
            }
            Stmt::If { cond, then, els } => {
                let c = self.gen_scalar(cond)?;
                let then_blk = self.b.create_block();
                let else_blk = self.b.create_block();
                let merge = self.b.create_block();
                self.b.ins().brif(c, then_blk, &[], else_blk, &[]);

                self.b.switch_to_block(then_blk);
                self.terminated = false;
                self.gen_block(then)?;
                if !self.terminated {
                    self.b.ins().jump(merge, &[]);
                }
                let then_terminated = self.terminated;

                self.b.switch_to_block(else_blk);
                self.terminated = false;
                if let Some(els) = els {
                    self.gen_block(els)?;
                }
                if !self.terminated {
                    self.b.ins().jump(merge, &[]);
                }
                let else_terminated = self.terminated;

                self.b.switch_to_block(merge);
                self.terminated = then_terminated && else_terminated;
                if self.terminated {
                    // Merge block is unreachable; keep IR valid.
                    self.b.ins().trap(TrapCode::user(1).unwrap());
                }
            }
            Stmt::While { cond, body } => {
                let header = self.b.create_block();
                let body_blk = self.b.create_block();
                let exit = self.b.create_block();
                self.b.ins().jump(header, &[]);
                self.b.switch_to_block(header);
                let c = self.gen_scalar(cond)?;
                self.b.ins().brif(c, body_blk, &[], exit, &[]);
                self.b.switch_to_block(body_blk);
                self.loop_stack.push((exit, header));
                self.terminated = false;
                self.gen_block(body)?;
                self.loop_stack.pop();
                if !self.terminated {
                    self.b.ins().jump(header, &[]);
                }
                self.b.switch_to_block(exit);
                self.terminated = false;
            }
            Stmt::For { init, cond, step, body } => {
                if let Some(init) = init {
                    self.gen_stmt(init)?;
                }
                let header = self.b.create_block();
                let body_blk = self.b.create_block();
                let step_blk = self.b.create_block();
                let exit = self.b.create_block();
                self.b.ins().jump(header, &[]);
                self.b.switch_to_block(header);
                match cond {
                    Some(cond) => {
                        let c = self.gen_scalar(cond)?;
                        self.b.ins().brif(c, body_blk, &[], exit, &[]);
                    }
                    None => {
                        self.b.ins().jump(body_blk, &[]);
                    }
                }
                self.b.switch_to_block(body_blk);
                self.loop_stack.push((exit, step_blk));
                self.terminated = false;
                self.gen_block(body)?;
                self.loop_stack.pop();
                if !self.terminated {
                    self.b.ins().jump(step_blk, &[]);
                }
                self.b.switch_to_block(step_blk);
                self.terminated = false;
                if let Some(step) = step {
                    self.gen_stmt(step)?;
                }
                self.b.ins().jump(header, &[]);
                self.b.switch_to_block(exit);
                self.terminated = false;
            }
            Stmt::Return { value, .. } => {
                match value {
                    Some(v) => {
                        let cv = self.gen_expr(v)?;
                        let mut rv = self.value_of(cv);
                        if v.ty == CrowType::Nil && self.ret == CrowType::Float {
                            rv = self.b.ins().f64const(0.0); // unreachable in practice
                        }
                        self.b.ins().return_(&[rv]);
                    }
                    None => {
                        self.b.ins().return_(&[]);
                    }
                }
                self.terminated = true;
            }
            Stmt::Break(_) => {
                let (exit, _) = *self.loop_stack.last().unwrap();
                self.b.ins().jump(exit, &[]);
                self.terminated = true;
            }
            Stmt::Continue(_) => {
                let (_, cont) = *self.loop_stack.last().unwrap();
                self.b.ins().jump(cont, &[]);
                self.terminated = true;
            }
            Stmt::Block(b) => self.gen_block(b)?,
        }
        Ok(())
    }

    fn assign_local(&mut self, local: u32, cv: CV) {
        let v = self.value_of(cv);
        self.b.def_var(self.local_repr[local as usize], v);
    }

    // -- Expressions --------------------------------------------------------

    /// Generate an expression that is known to be scalar-typed.
    fn gen_scalar(&mut self, e: &Expr) -> Result<Value, String> {
        let cv = self.gen_expr(e)?;
        let v = self.value_of(cv);
        Ok(v)
    }

    fn gen_expr(&mut self, e: &Expr) -> Result<CV, String> {
        let line = e.line;
        Ok(match &e.kind {
            // The checker stored the canonical two's-complement pattern.
            ExprKind::Int(v) => CV::Scalar(self.b.ins().iconst(types::I64, *v as i64)),
            ExprKind::Byte(v) => CV::Scalar(self.b.ins().iconst(types::I64, *v as i64)),
            ExprKind::Float(v) => CV::Scalar(self.b.ins().f64const(*v)),
            ExprKind::Bool(v) => CV::Scalar(self.b.ins().iconst(types::I64, *v as i64)),
            ExprKind::Str(s) => {
                // String literals are static objects: the GC neither moves
                // nor frees them, so the address needs no root slot.
                let data_id = self.cg.define_string_literal(s)?;
                let gv = self.cg.module.declare_data_in_func(data_id, self.b.func);
                CV::Scalar(self.b.ins().global_value(types::I64, gv))
            }
            ExprKind::Nil => CV::Scalar(self.b.ins().iconst(types::I64, 0)),
            ExprKind::Var { res, .. } => match res.unwrap() {
                VarRes::Local(idx) => {
                    CV::Scalar(self.b.use_var(self.local_repr[idx as usize]))
                }
                VarRes::Captured(i) => {
                    let env = self.load_env();
                    let off = HEADER + 8 * (i as i32 + 1);
                    let ty = abi_ty(&e.ty);
                    let v = self.b.ins().load(ty, MemFlags::trusted(), env, off);
                    if e.ty.is_ref() {
                        self.root(v)
                    } else {
                        CV::Scalar(v)
                    }
                }
                VarRes::Func(fid) => self.make_closure_for_func(fid),
            },
            ExprKind::Unary(op, sub) => {
                let v = self.gen_scalar(sub)?;
                let r = match (op, &sub.ty) {
                    (UnOp::Neg, CrowType::Float) => self.b.ins().fneg(v),
                    (UnOp::Neg, CrowType::Int(k)) => {
                        // Negating the most negative value overflows.
                        let is_min = self.b.ins().icmp_imm(IntCC::Equal, v, k.min() as i64);
                        self.panic_if(is_min, Rt::PanicOverflow, line);
                        self.b.ins().ineg(v)
                    }
                    (UnOp::Neg, _) => self.b.ins().ineg(v),
                    (UnOp::Not, _) => self.b.ins().bxor_imm(v, 1),
                    (UnOp::BitNot, CrowType::Int(k)) => {
                        let r = self.b.ins().bnot(v);
                        // Re-canonicalize narrow unsigned results: bnot sets
                        // the (zero) extension bits. Sign-extended values are
                        // closed under complement.
                        if k.signed() || k.bits() == 64 {
                            r
                        } else {
                            self.b.ins().band_imm(r, k.max() as i64)
                        }
                    }
                    (UnOp::BitNot, _) => unreachable!("checker restricted '~' to integers"),
                };
                CV::Scalar(r)
            }
            ExprKind::Binary(op, lhs, rhs) => self.gen_binary(*op, lhs, rhs, line)?,
            ExprKind::If { cond, then, els } => {
                let c = self.gen_scalar(cond)?;
                let then_blk = self.b.create_block();
                let else_blk = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, abi_ty(&e.ty));
                self.b.ins().brif(c, then_blk, &[], else_blk, &[]);
                self.b.switch_to_block(then_blk);
                let tv = self.gen_scalar(then)?;
                self.b.ins().jump(merge, &[tv.into()]);
                self.b.switch_to_block(else_blk);
                let ev = self.gen_scalar(els)?;
                self.b.ins().jump(merge, &[ev.into()]);
                self.b.switch_to_block(merge);
                let r = self.b.block_params(merge)[0];
                if e.ty.is_ref() {
                    self.root(r)
                } else {
                    CV::Scalar(r)
                }
            }
            ExprKind::Call { callee, args, direct, inst } => {
                if let Some(fid) = direct {
                    let mut vals = Vec::with_capacity(args.len());
                    let cvs: Vec<CV> =
                        args.iter().map(|a| self.gen_expr(a)).collect::<Result<_, _>>()?;
                    for cv in &cvs {
                        vals.push(self.value_of(*cv));
                    }
                    let callee_id = if inst.is_empty() {
                        self.cg.func_ids[*fid as usize].expect("monomorphic function declared")
                    } else {
                        self.cg.instance_func_id(*fid, inst)?
                    };
                    let fref = self.cg.module.declare_func_in_func(callee_id, self.b.func);
                    let call = self.b.ins().call(fref, &vals);
                    let results = self.b.inst_results(call).to_vec();
                    self.finish_call(&e.ty, results)
                } else {
                    let f_cv = self.gen_expr(callee)?;
                    let cvs: Vec<CV> =
                        args.iter().map(|a| self.gen_expr(a)).collect::<Result<_, _>>()?;
                    let fval = self.value_of(f_cv);
                    self.null_check(fval, line);
                    let fnptr = self.b.ins().load(types::I64, MemFlags::trusted(), fval, HEADER);
                    let mut vals = vec![fval];
                    for cv in &cvs {
                        vals.push(self.value_of(*cv));
                    }
                    let (params, ret) = match &callee.ty {
                        CrowType::Fn(p, r) => (p.clone(), (**r).clone()),
                        _ => unreachable!(),
                    };
                    let sig = self.cg.make_sig(&params, &ret, true);
                    let sigref = self.b.import_signature(sig);
                    let call = self.b.ins().call_indirect(sigref, fnptr, &vals);
                    let results = self.b.inst_results(call).to_vec();
                    self.finish_call(&e.ty, results)
                }
            }
            ExprKind::Builtin(b, args) => self.gen_builtin(*b, args, line)?,
            ExprKind::Field { obj, index, .. } => {
                let obj_cv = self.gen_expr(obj)?;
                let obj_v = self.value_of(obj_cv);
                self.null_check(obj_v, line);
                let (sid, targs) = match &obj.ty {
                    CrowType::Struct(s, a) => (*s, a.clone()),
                    _ => unreachable!("checker validated field access"),
                };
                let off = self.field_off(sid, &targs, *index)?;
                let v = self.load_typed(&e.ty.clone(), obj_v, off);
                if e.ty.is_ref() {
                    self.root(v)
                } else {
                    CV::Scalar(v)
                }
            }
            ExprKind::Index(arr, idx) => {
                // `s[i]`: bounds-checked byte access into a string.
                if arr.ty == CrowType::Str {
                    let s_cv = self.gen_expr(arr)?;
                    let idx_v = self.gen_scalar(idx)?;
                    let s_v = self.value_of(s_cv);
                    self.null_check(s_v, line);
                    let len = self.b.ins().load(types::I64, MemFlags::trusted(), s_v, 8);
                    self.bounds_check(idx_v, len, line);
                    let base = self.b.ins().iadd(s_v, idx_v);
                    let byte = self.b.ins().load(types::I8, MemFlags::trusted(), base, HEADER);
                    return Ok(CV::Scalar(self.b.ins().uextend(types::I64, byte)));
                }
                let (_, elem_addr) = self.gen_index_addr(arr, idx, line)?;
                let v = self.load_typed(&e.ty.clone(), elem_addr, 0);
                if e.ty.is_ref() {
                    self.root(v)
                } else {
                    CV::Scalar(v)
                }
            }
            ExprKind::Cast(sub, _) => {
                let v = self.gen_scalar(sub)?;
                let src = sub.ty.clone();
                let dst = e.ty.clone();
                CV::Scalar(self.gen_cast(&src, &dst, v, line))
            }
            ExprKind::ArrayLit(elems) => {
                let elem_ty = match &e.ty {
                    CrowType::Array(t) => (**t).clone(),
                    _ => unreachable!(),
                };
                let is_ref = self.b.ins().iconst(types::I64, elem_ty.is_ref() as i64);
                let elem_size = self.b.ins().iconst(types::I64, elem_ty.size_bytes() as i64);
                let cap = self.b.ins().iconst(types::I64, elems.len().max(4) as i64);
                let arr = self.call_rt(Rt::ArrayNew, &[elem_size, is_ref, cap]).unwrap();
                let arr_cv = self.root(arr);
                for el in elems {
                    let cv = self.gen_expr(el)?;
                    let mut v = self.value_of(cv);
                    if elem_ty == CrowType::Float {
                        v = self.b.ins().bitcast(types::I64, MemFlags::new(), v);
                    }
                    let arr_v = self.value_of(arr_cv);
                    let elem_size = self.b.ins().iconst(types::I64, elem_ty.size_bytes() as i64);
                    let is_ref = self.b.ins().iconst(types::I64, elem_ty.is_ref() as i64);
                    self.call_rt(Rt::ArrayPush, &[arr_v, v, elem_size, is_ref]);
                }
                arr_cv
            }
            ExprKind::StructLit { fields, struct_id, .. } => {
                let targs = match &e.ty {
                    CrowType::Struct(_, a) => a.clone(),
                    _ => unreachable!("checker typed struct literals"),
                };
                let desc_id = self.cg.struct_inst(*struct_id, &targs)?.desc;
                let gv = self.cg.module.declare_data_in_func(desc_id, self.b.func);
                let desc = self.b.ins().global_value(types::I64, gv);
                let zero = self.b.ins().iconst(types::I64, 0);
                let obj = self.call_rt(Rt::Alloc, &[desc, zero]).unwrap();
                let obj_cv = self.root(obj);
                for (_, value, index) in fields {
                    let cv = self.gen_expr(value)?;
                    let obj_v = self.value_of(obj_cv);
                    let fty = self.field_ty(*struct_id, &targs, *index);
                    let off = self.field_off(*struct_id, &targs, *index)?;
                    let v = self.value_of(cv);
                    if fty.is_ref() {
                        let addr = self.b.ins().iadd_imm(obj_v, off as i64);
                        self.call_rt(Rt::WriteRef, &[obj_v, addr, v]);
                    } else {
                        self.store_typed(&fty, v, obj_v, off);
                    }
                }
                obj_cv
            }
            ExprKind::Lambda(lam) => {
                let (lam_func, lam_desc) = self.lam_ids[&lam.id];
                let gv = self.cg.module.declare_data_in_func(lam_desc, self.b.func);
                let desc = self.b.ins().global_value(types::I64, gv);
                let zero = self.b.ins().iconst(types::I64, 0);
                let obj = self.call_rt(Rt::Alloc, &[desc, zero]).unwrap();
                let obj_cv = self.root(obj);
                // Code pointer (scalar field 0).
                let fref = self.cg.module.declare_func_in_func(lam_func, self.b.func);
                let fnptr = self.b.ins().func_addr(types::I64, fref);
                let obj_v = self.value_of(obj_cv);
                self.b.ins().store(MemFlags::trusted(), fnptr, obj_v, HEADER);
                // Captures, by value, from the enclosing frame.
                for (ci, cap) in lam.captures.iter().enumerate() {
                    let val = match cap.src {
                        VarRes::Local(idx) => self.b.use_var(self.local_repr[idx as usize]),
                        VarRes::Captured(i) => {
                            let env = self.load_env();
                            let off = HEADER + 8 * (i as i32 + 1);
                            self.b.ins().load(abi_ty(&cap.ty), MemFlags::trusted(), env, off)
                        }
                        VarRes::Func(_) => unreachable!(),
                    };
                    let obj_v = self.value_of(obj_cv);
                    let off = HEADER + 8 * (ci as i32 + 1);
                    if cap.ty.is_ref() {
                        let addr = self.b.ins().iadd_imm(obj_v, off as i64);
                        self.call_rt(Rt::WriteRef, &[obj_v, addr, val]);
                    } else {
                        self.b.ins().store(MemFlags::trusted(), val, obj_v, off);
                    }
                }
                obj_cv
            }
        })
    }

    fn finish_call(&mut self, ret_ty: &CrowType, results: Vec<Value>) -> CV {
        if *ret_ty == CrowType::Unit {
            CV::Unit
        } else if ret_ty.is_ref() {
            self.root(results[0])
        } else {
            CV::Scalar(results[0])
        }
    }

    /// A top-level function as a value: the address of its static closure.
    /// Static objects never move or die, so no GC root is needed (same as
    /// string literals).
    fn make_closure_for_func(&mut self, fid: u32) -> CV {
        let data_id = self.cg.fnval_ids[fid as usize].expect("generic functions are not values");
        let gv = self.cg.module.declare_data_in_func(data_id, self.b.func);
        CV::Scalar(self.b.ins().global_value(types::I64, gv))
    }

    /// Null check + bounds check + element address. Returns (buffer, addr).
    fn gen_index_addr(&mut self, arr: &Expr, idx: &Expr, line: u32) -> Result<(Value, Value), String> {
        let arr_cv = self.gen_expr(arr)?;
        let idx_v = self.gen_scalar(idx)?;
        let arr_v = self.value_of(arr_cv);
        self.null_check(arr_v, line);
        let elem_size = match &arr.ty {
            CrowType::Array(t) => t.size_bytes(),
            _ => unreachable!("checker validated index targets"),
        };
        Ok(self.index_addr_checked(arr_v, idx_v, elem_size, line))
    }

    /// Bounds check + element address for an already-evaluated array value
    /// and index. Returns (buffer, addr); both are derived pointers that
    /// must not live across a safepoint.
    fn index_addr_checked(
        &mut self,
        arr_v: Value,
        idx_v: Value,
        elem_size: u32,
        line: u32,
    ) -> (Value, Value) {
        let len = self.b.ins().load(types::I64, MemFlags::trusted(), arr_v, ARR_LEN);
        self.bounds_check(idx_v, len, line);
        let buf = self.b.ins().load(types::I64, MemFlags::trusted(), arr_v, ARR_BUF);
        let scaled = self.b.ins().imul_imm(idx_v, elem_size as i64);
        let base = self.b.ins().iadd_imm(scaled, HEADER as i64);
        let addr = self.b.ins().iadd(buf, base);
        (buf, addr)
    }

    /// Panic when `idx` is not in `[0, len)`.
    fn bounds_check(&mut self, idx_v: Value, len: Value, line: u32) {
        // Unsigned compare also catches negative indices.
        let oob = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, idx_v, len);
        let panic_blk = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(oob, panic_blk, &[], cont, &[]);
        self.b.switch_to_block(panic_blk);
        let l = self.line_const(line);
        self.emit_panic(Rt::PanicBounds, &[idx_v, len, l]);
        self.b.switch_to_block(cont);
    }

    fn gen_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, line: u32) -> Result<CV, String> {
        // Short-circuit logical operators.
        if op == BinOp::And || op == BinOp::Or {
            let l = self.gen_scalar(lhs)?;
            let rhs_blk = self.b.create_block();
            let merge = self.b.create_block();
            self.b.append_block_param(merge, types::I64);
            match op {
                BinOp::And => self.b.ins().brif(l, rhs_blk, &[], merge, &[l.into()]),
                _ => self.b.ins().brif(l, merge, &[l.into()], rhs_blk, &[]),
            };
            self.b.switch_to_block(rhs_blk);
            let r = self.gen_scalar(rhs)?;
            self.b.ins().jump(merge, &[r.into()]);
            self.b.switch_to_block(merge);
            return Ok(CV::Scalar(self.b.block_params(merge)[0]));
        }

        let operand_ty = lhs.ty.clone();

        // String equality goes through the runtime; concatenation shares
        // `gen_arith_values` with compound assignment.
        if operand_ty == CrowType::Str || rhs.ty == CrowType::Str {
            let l_cv = self.gen_expr(lhs)?;
            let r_cv = self.gen_expr(rhs)?;
            let lv = self.value_of(l_cv);
            let rv = self.value_of(r_cv);
            let result = match op {
                BinOp::Add => {
                    let v = self.gen_arith_values(op, &CrowType::Str, lv, rv, line);
                    return Ok(CV::Scalar(v));
                }
                BinOp::Eq => self.call_rt(Rt::StrEq, &[lv, rv]).unwrap(),
                BinOp::Ne => {
                    let eq = self.call_rt(Rt::StrEq, &[lv, rv]).unwrap();
                    self.b.ins().bxor_imm(eq, 1)
                }
                _ => unreachable!(),
            };
            return Ok(CV::Scalar(result));
        }

        // Reference equality (struct/array/fn identity, nil comparisons).
        if matches!(op, BinOp::Eq | BinOp::Ne)
            && (operand_ty.is_ref() || rhs.ty.is_ref())
        {
            let l_cv = self.gen_expr(lhs)?;
            let r_cv = self.gen_expr(rhs)?;
            let lv = self.value_of(l_cv);
            let rv = self.value_of(r_cv);
            let cc = if op == BinOp::Eq { IntCC::Equal } else { IntCC::NotEqual };
            let c = self.b.ins().icmp(cc, lv, rv);
            let r = self.b.ins().uextend(types::I64, c);
            return Ok(CV::Scalar(r));
        }

        let lv = self.gen_scalar(lhs)?;
        let rv = self.gen_scalar(rhs)?;

        // Comparisons. Bools only reach here for Eq/Ne.
        if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
            if operand_ty == CrowType::Float {
                let cc = match op {
                    BinOp::Eq => FloatCC::Equal,
                    BinOp::Ne => FloatCC::NotEqual,
                    BinOp::Lt => FloatCC::LessThan,
                    BinOp::Le => FloatCC::LessThanOrEqual,
                    BinOp::Gt => FloatCC::GreaterThan,
                    BinOp::Ge => FloatCC::GreaterThanOrEqual,
                    _ => unreachable!(),
                };
                let c = self.b.ins().fcmp(cc, lv, rv);
                return Ok(CV::Scalar(self.b.ins().uextend(types::I64, c)));
            }
            let signed = operand_ty.int_kind().map_or(true, |k| k.signed());
            let cc = match op {
                BinOp::Eq => IntCC::Equal,
                BinOp::Ne => IntCC::NotEqual,
                BinOp::Lt if signed => IntCC::SignedLessThan,
                BinOp::Le if signed => IntCC::SignedLessThanOrEqual,
                BinOp::Gt if signed => IntCC::SignedGreaterThan,
                BinOp::Ge if signed => IntCC::SignedGreaterThanOrEqual,
                BinOp::Lt => IntCC::UnsignedLessThan,
                BinOp::Le => IntCC::UnsignedLessThanOrEqual,
                BinOp::Gt => IntCC::UnsignedGreaterThan,
                BinOp::Ge => IntCC::UnsignedGreaterThanOrEqual,
                _ => unreachable!(),
            };
            let c = self.b.ins().icmp(cc, lv, rv);
            return Ok(CV::Scalar(self.b.ins().uextend(types::I64, c)));
        }

        Ok(CV::Scalar(self.gen_arith_values(op, &operand_ty, lv, rv, line)))
    }

    /// Arithmetic, bitwise, and string-concat operators on already-evaluated
    /// operands of type `ty` (both sides). Shared by binary expressions and
    /// compound assignment. A returned reference (concat result) is already
    /// rooted.
    fn gen_arith_values(
        &mut self,
        op: BinOp,
        ty: &CrowType,
        lv: Value,
        rv: Value,
        line: u32,
    ) -> Value {
        if *ty == CrowType::Str {
            debug_assert_eq!(op, BinOp::Add, "checker restricted string arithmetic to '+'");
            self.null_check(lv, line);
            self.null_check(rv, line);
            let v = self.call_rt(Rt::StrConcat, &[lv, rv]).unwrap();
            self.root(v);
            return v;
        }
        if *ty == CrowType::Float {
            return match op {
                BinOp::Add => self.b.ins().fadd(lv, rv),
                BinOp::Sub => self.b.ins().fsub(lv, rv),
                BinOp::Mul => self.b.ins().fmul(lv, rv),
                BinOp::Div => self.b.ins().fdiv(lv, rv),
                _ => unreachable!("checker restricted float arithmetic"),
            };
        }
        let k = ty.int_kind().expect("checker restricted arithmetic to numeric types");
        match op {
            // Full-width checked arithmetic reads the hardware overflow flag
            // (adds + b.vs on arm64, add + jo on x64) instead of recomputing
            // it with sign tricks — ~6 fewer instructions per operation.
            BinOp::Add | BinOp::Sub | BinOp::Mul if k.bits() == 64 => {
                let (r, of) = match (op, k.signed()) {
                    (BinOp::Add, true) => self.b.ins().sadd_overflow(lv, rv),
                    (BinOp::Add, false) => self.b.ins().uadd_overflow(lv, rv),
                    (BinOp::Sub, true) => self.b.ins().ssub_overflow(lv, rv),
                    (BinOp::Sub, false) => self.b.ins().usub_overflow(lv, rv),
                    (BinOp::Mul, true) => self.b.ins().smul_overflow(lv, rv),
                    (BinOp::Mul, false) => self.b.ins().umul_overflow(lv, rv),
                    _ => unreachable!(),
                };
                self.panic_if(of, Rt::PanicOverflow, line);
                r
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let r = match op {
                    BinOp::Add => self.b.ins().iadd(lv, rv),
                    BinOp::Sub => self.b.ins().isub(lv, rv),
                    _ => self.b.ins().imul(lv, rv),
                };
                self.check_overflow(k, r, line);
                r
            }
            BinOp::Div | BinOp::Rem => self.gen_div(op, k, lv, rv, line),
            BinOp::BitAnd => self.b.ins().band(lv, rv),
            BinOp::BitOr => self.b.ins().bor(lv, rv),
            BinOp::BitXor => self.b.ins().bxor(lv, rv),
            BinOp::Shl | BinOp::Shr => self.gen_shift(op, k, lv, rv, line),
            _ => unreachable!("not an arithmetic operator"),
        }
    }

    /// Shifts: the amount must be in `[0, bits)` (panics otherwise; the
    /// unsigned compare also catches negative amounts in canonical form).
    /// `>>` is arithmetic on signed types and logical on unsigned ones;
    /// `<<` discards bits shifted out of the width.
    fn gen_shift(&mut self, op: BinOp, k: IntKind, lv: Value, rv: Value, line: u32) -> Value {
        let bad = self.b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, rv, k.bits() as i64);
        self.panic_if(bad, Rt::PanicShift, line);
        if op == BinOp::Shr {
            // Canonical (extended) 64-bit operands make the 64-bit shift
            // exact for every width.
            return if k.signed() {
                self.b.ins().sshr(lv, rv)
            } else {
                self.b.ins().ushr(lv, rv)
            };
        }
        let r = self.b.ins().ishl(lv, rv);
        // Re-canonicalize narrow results to their storage width.
        if k.bits() == 64 {
            r
        } else if k.signed() {
            let small = self.b.ins().ireduce(Self::small_ty(k), r);
            self.b.ins().sextend(types::I64, small)
        } else {
            self.b.ins().band_imm(r, k.max() as i64)
        }
    }

    /// Panic if a narrow (< 64-bit) add/sub/mul result overflowed the
    /// integer kind. Canonical operands are small enough that the 64-bit
    /// result is exact, so overflow is just a range check.
    fn check_overflow(&mut self, k: IntKind, r: Value, line: u32) {
        debug_assert!(k.bits() < 64, "full-width ops use the hardware overflow flag");
        let lo = self.b.ins().icmp_imm(IntCC::SignedLessThan, r, k.min() as i64);
        let hi = self.b.ins().icmp_imm(IntCC::SignedGreaterThan, r, k.max() as i64);
        let overflow = self.b.ins().bor(lo, hi);
        self.panic_if(overflow, Rt::PanicOverflow, line);
    }

    /// Integer division with explicit zero and overflow handling (C-style UB
    /// is replaced by runtime panics).
    fn gen_div(&mut self, op: BinOp, k: IntKind, lv: Value, rv: Value, line: u32) -> Value {
        let is_zero = self.b.ins().icmp_imm(IntCC::Equal, rv, 0);
        self.panic_if(is_zero, Rt::PanicDiv, line);

        if !k.signed() {
            return if op == BinOp::Div {
                self.b.ins().udiv(lv, rv)
            } else {
                self.b.ins().urem(lv, rv)
            };
        }

        // Signed: special-case a divisor of -1, which both overflows for
        // MIN / -1 and traps in hardware for 64-bit MIN / -1.
        let neg1_blk = self.b.create_block();
        let normal = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);

        let is_neg1 = self.b.ins().icmp_imm(IntCC::Equal, rv, -1);
        self.b.ins().brif(is_neg1, neg1_blk, &[], normal, &[]);

        self.b.switch_to_block(neg1_blk);
        let special = if op == BinOp::Div {
            let is_min = self.b.ins().icmp_imm(IntCC::Equal, lv, k.min() as i64);
            self.panic_if(is_min, Rt::PanicOverflow, line);
            self.b.ins().ineg(lv)
        } else {
            self.b.ins().iconst(types::I64, 0) // x % -1 == 0
        };
        self.b.ins().jump(merge, &[special.into()]);

        self.b.switch_to_block(normal);
        let res = if op == BinOp::Div {
            self.b.ins().sdiv(lv, rv)
        } else {
            self.b.ins().srem(lv, rv)
        };
        self.b.ins().jump(merge, &[res.into()]);

        self.b.switch_to_block(merge);
        self.b.block_params(merge)[0]
    }

    /// `expr as Type`: numeric conversion, panicking when the value does not
    /// fit the target. A passing check leaves integer bits unchanged (values
    /// in range are identical in canonical form), so no re-extension needed.
    fn gen_cast(&mut self, src: &CrowType, dst: &CrowType, v: Value, line: u32) -> Value {
        match (src, dst) {
            (CrowType::Float, CrowType::Float) => v,
            (CrowType::Int(s), CrowType::Float) => {
                if s.signed() {
                    self.b.ins().fcvt_from_sint(types::F64, v)
                } else {
                    self.b.ins().fcvt_from_uint(types::F64, v)
                }
            }
            (CrowType::Int(s), CrowType::Int(d)) => {
                let covered = match (s.signed(), d.signed()) {
                    (true, true) | (false, false) => d.bits() >= s.bits(),
                    (false, true) => d.bits() > s.bits(),
                    (true, false) => false,
                };
                if covered {
                    return v;
                }
                let bad = if !s.signed() {
                    // Raw unsigned value; the target is not u64 (that would
                    // be covered), so its max fits in an i64 immediate.
                    self.b.ins().icmp_imm(IntCC::UnsignedGreaterThan, v, d.max() as i64)
                } else if !d.signed() {
                    let neg = self.b.ins().icmp_imm(IntCC::SignedLessThan, v, 0);
                    if d.bits() < 64 {
                        let hi =
                            self.b.ins().icmp_imm(IntCC::SignedGreaterThan, v, d.max() as i64);
                        self.b.ins().bor(neg, hi)
                    } else {
                        neg
                    }
                } else {
                    let lo = self.b.ins().icmp_imm(IntCC::SignedLessThan, v, d.min() as i64);
                    let hi = self.b.ins().icmp_imm(IntCC::SignedGreaterThan, v, d.max() as i64);
                    self.b.ins().bor(lo, hi)
                };
                self.panic_if(bad, Rt::PanicCast, line);
                v
            }
            (CrowType::Float, CrowType::Int(d)) => {
                // Truncation toward zero; valid iff trunc(v) is in range.
                // NaN fails both compares and lands in the panic path.
                let lo_ok = if !d.signed() {
                    let bound = self.b.ins().f64const(-1.0);
                    self.b.ins().fcmp(FloatCC::GreaterThan, v, bound)
                } else if d.bits() == 64 {
                    // min - 1 is not representable; min itself is exact and
                    // no f64 lies strictly between min - 1 and min.
                    let bound = self.b.ins().f64const(d.min() as f64);
                    self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, v, bound)
                } else {
                    let bound = self.b.ins().f64const((d.min() - 1) as f64);
                    self.b.ins().fcmp(FloatCC::GreaterThan, v, bound)
                };
                let hi_bound = self.b.ins().f64const((d.max() + 1) as f64);
                let hi_ok = self.b.ins().fcmp(FloatCC::LessThan, v, hi_bound);
                let ok = self.b.ins().band(lo_ok, hi_ok);
                let bad = self.b.ins().bxor_imm(ok, 1);
                self.panic_if(bad, Rt::PanicCast, line);
                if d.signed() {
                    self.b.ins().fcvt_to_sint_sat(types::I64, v)
                } else {
                    self.b.ins().fcvt_to_uint_sat(types::I64, v)
                }
            }
            _ => unreachable!("checker restricted casts to numeric types"),
        }
    }

    fn gen_builtin(&mut self, b: Builtin, args: &[Expr], line: u32) -> Result<CV, String> {
        Ok(match b {
            Builtin::Println | Builtin::Print => {
                let arg = &args[0];
                let cv = self.gen_expr(arg)?;
                let v = self.value_of(cv);
                if arg.ty == CrowType::Str {
                    self.null_check(v, line);
                }
                match arg.ty {
                    CrowType::Int(k) if k.signed() => self.call_rt(Rt::PrintInt, &[v]),
                    CrowType::Int(_) => self.call_rt(Rt::PrintUint, &[v]),
                    CrowType::Float => self.call_rt(Rt::PrintFloat, &[v]),
                    CrowType::Bool => self.call_rt(Rt::PrintBool, &[v]),
                    _ => self.call_rt(Rt::PrintStr, &[v]),
                };
                if b == Builtin::Println {
                    self.call_rt(Rt::PrintNewline, &[]);
                }
                CV::Unit
            }
            Builtin::Len => {
                let arg = &args[0];
                let cv = self.gen_expr(arg)?;
                let v = self.value_of(cv);
                self.null_check(v, line);
                let off = if arg.ty == CrowType::Str { 8 } else { ARR_LEN };
                let len = self.b.ins().load(types::I64, MemFlags::trusted(), v, off);
                CV::Scalar(len)
            }
            Builtin::Push => {
                let elem_ty = match &args[0].ty {
                    CrowType::Array(t) => (**t).clone(),
                    _ => unreachable!(),
                };
                let arr_cv = self.gen_expr(&args[0])?;
                let val_cv = self.gen_expr(&args[1])?;
                let arr_v = self.value_of(arr_cv);
                self.null_check(arr_v, line);
                let mut v = self.value_of(val_cv);
                if elem_ty == CrowType::Float {
                    v = self.b.ins().bitcast(types::I64, MemFlags::new(), v);
                }
                let elem_size = self.b.ins().iconst(types::I64, elem_ty.size_bytes() as i64);
                let is_ref = self.b.ins().iconst(types::I64, elem_ty.is_ref() as i64);
                self.call_rt(Rt::ArrayPush, &[arr_v, v, elem_size, is_ref]);
                CV::Unit
            }
            Builtin::Pop => {
                let elem_ty = match &args[0].ty {
                    CrowType::Array(t) => (**t).clone(),
                    _ => unreachable!(),
                };
                let arr_cv = self.gen_expr(&args[0])?;
                let arr_v = self.value_of(arr_cv);
                self.null_check(arr_v, line);
                let elem_size = self.b.ins().iconst(types::I64, elem_ty.size_bytes() as i64);
                let raw = self.call_rt(Rt::ArrayPop, &[arr_v, elem_size]).unwrap();
                if elem_ty.is_ref() {
                    self.root(raw)
                } else if elem_ty == CrowType::Float {
                    CV::Scalar(self.b.ins().bitcast(types::F64, MemFlags::new(), raw))
                } else if let CrowType::Int(k) = elem_ty {
                    // The runtime zero-extends narrow elements; re-extend
                    // signed kinds into canonical form.
                    if k.size() < 8 && k.signed() {
                        let small = self.b.ins().ireduce(Self::small_ty(k), raw);
                        CV::Scalar(self.b.ins().sextend(types::I64, small))
                    } else {
                        CV::Scalar(raw)
                    }
                } else {
                    CV::Scalar(raw)
                }
            }
            Builtin::Itos => {
                let arg = &args[0];
                let signed = arg.ty.int_kind().map_or(true, |k| k.signed());
                let v = self.gen_scalar(arg)?;
                let rt = if signed { Rt::Itos } else { Rt::Utos };
                let s = self.call_rt(rt, &[v]).unwrap();
                self.root(s)
            }
            Builtin::Ftos => {
                let v = self.gen_scalar(&args[0])?;
                let s = self.call_rt(Rt::Ftos, &[v]).unwrap();
                self.root(s)
            }
            Builtin::Itof => {
                let signed = args[0].ty.int_kind().map_or(true, |k| k.signed());
                let v = self.gen_scalar(&args[0])?;
                if signed {
                    CV::Scalar(self.b.ins().fcvt_from_sint(types::F64, v))
                } else {
                    CV::Scalar(self.b.ins().fcvt_from_uint(types::F64, v))
                }
            }
            Builtin::Ftoi => {
                // Same semantics as `expr as int`: panic when out of range.
                let v = self.gen_scalar(&args[0])?;
                CV::Scalar(self.gen_cast(&CrowType::Float, &CrowType::Int(IntKind::I64), v, line))
            }
            Builtin::Stoi | Builtin::Stof => {
                let cv = self.gen_expr(&args[0])?;
                let v = self.value_of(cv);
                self.null_check(v, line);
                let l = self.line_const(line);
                let rt = if b == Builtin::Stoi { Rt::Stoi } else { Rt::Stof };
                CV::Scalar(self.call_rt(rt, &[v, l]).unwrap())
            }
            Builtin::Stob => {
                let cv = self.gen_expr(&args[0])?;
                let v = self.value_of(cv);
                self.null_check(v, line);
                let arr = self.call_rt(Rt::Stob, &[v]).unwrap();
                self.root(arr)
            }
            Builtin::Btos => {
                let cv = self.gen_expr(&args[0])?;
                let v = self.value_of(cv);
                self.null_check(v, line);
                let l = self.line_const(line);
                let s = self.call_rt(Rt::Btos, &[v, l]).unwrap();
                self.root(s)
            }
            Builtin::Assert => {
                let v = self.gen_scalar(&args[0])?;
                let fail = self.b.create_block();
                let cont = self.b.create_block();
                self.b.ins().brif(v, cont, &[], fail, &[]);
                self.b.switch_to_block(fail);
                let l = self.line_const(line);
                self.emit_panic(Rt::AssertFail, &[l]);
                self.b.switch_to_block(cont);
                CV::Unit
            }
            Builtin::GcCollect => {
                let full = self.b.ins().iconst(types::I64, 1);
                self.call_rt(Rt::GcCollect, &[full]);
                CV::Unit
            }
        })
    }
}

#[cfg(test)]
mod tests {
    /// Compile source and return the distinct `crow_desc.*` symbol names
    /// embedded in the object file's string table.
    fn desc_symbols(src: &str) -> Vec<String> {
        let toks = crate::lexer::lex(src).unwrap();
        let mut program = crate::parser::parse(toks).unwrap();
        let checked = crate::typeck::check(&mut program).unwrap();
        let bytes = super::compile(&program, &checked).unwrap();
        let needle = b"crow_desc.";
        let mut out = std::collections::BTreeSet::new();
        for i in 0..bytes.len().saturating_sub(needle.len()) {
            if &bytes[i..i + needle.len()] == needle {
                let end = bytes[i..]
                    .iter()
                    .position(|&b| !(b.is_ascii_alphanumeric() || b == b'.' || b == b'_'))
                    .map_or(bytes.len(), |p| i + p);
                out.insert(String::from_utf8_lossy(&bytes[i..end]).into_owned());
            }
        }
        out.into_iter().collect()
    }

    /// Descriptors are keyed by object shape, not by type: unrelated structs
    /// with the same layout, and all reference instantiations of a generic
    /// struct, share one descriptor.
    #[test]
    fn descriptors_dedupe_by_shape() {
        let syms = desc_symbols(
            r#"
struct A { s: string, n: int }
struct B { t: string, m: int }
struct Pair<T> { a: T, b: T }
fn main() {
    let a = A { s: "x", n: 1 };
    let b = B { t: "y", m: 2 };
    let p = Pair { a: "u", b: "v" };
    let q = Pair { a: a, b: a };
    let r = Pair { a: 1, b: 2 };
    assert(a != nil && b != nil && p != nil && q != nil && r != nil);
}
"#,
        );
        assert_eq!(
            syms,
            vec![
                "crow_desc.k0s16r0", // Pair<int>: two scalar words
                "crow_desc.k0s16r1", // A and B: ref word + scalar word
                "crow_desc.k0s16r3", // Pair<string> and Pair<A>: two ref words
                "crow_desc.k0s8r0",  // static closure objects (crow_fnval)
            ]
        );
    }
}
