//! Semantic types and the struct table.

use std::fmt;

/// A sized integer type. `int` is an alias for `I64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntKind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
}

impl IntKind {
    pub fn from_name(name: &str) -> Option<IntKind> {
        Some(match name {
            "i8" => IntKind::I8,
            "u8" => IntKind::U8,
            "i16" => IntKind::I16,
            "u16" => IntKind::U16,
            "i32" => IntKind::I32,
            "u32" => IntKind::U32,
            "int" | "i64" => IntKind::I64,
            "u64" => IntKind::U64,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            IntKind::I8 => "i8",
            IntKind::U8 => "u8",
            IntKind::I16 => "i16",
            IntKind::U16 => "u16",
            IntKind::I32 => "i32",
            IntKind::U32 => "u32",
            IntKind::I64 => "int",
            IntKind::U64 => "u64",
        }
    }

    pub fn signed(self) -> bool {
        matches!(self, IntKind::I8 | IntKind::I16 | IntKind::I32 | IntKind::I64)
    }

    pub fn bits(self) -> u32 {
        match self {
            IntKind::I8 | IntKind::U8 => 8,
            IntKind::I16 | IntKind::U16 => 16,
            IntKind::I32 | IntKind::U32 => 32,
            IntKind::I64 | IntKind::U64 => 64,
        }
    }

    /// Storage size in bytes (also the alignment).
    pub fn size(self) -> u32 {
        self.bits() / 8
    }

    pub fn min(self) -> i128 {
        if self.signed() {
            -(1i128 << (self.bits() - 1))
        } else {
            0
        }
    }

    pub fn max(self) -> i128 {
        if self.signed() {
            (1i128 << (self.bits() - 1)) - 1
        } else {
            (1i128 << self.bits()) - 1
        }
    }

    pub fn contains(self, v: i128) -> bool {
        v >= self.min() && v <= self.max()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    /// Placeholder before checking.
    Unknown,
    Unit,
    Int(IntKind),
    Float,
    Bool,
    Str,
    /// The type of the `nil` literal; unifies with any reference type.
    Nil,
    /// Struct definition id plus type arguments (empty for a non-generic
    /// struct).
    Struct(u32, Vec<Type>),
    Array(Box<Type>),
    Fn(Vec<Type>, Box<Type>),
    /// A type parameter of the enclosing generic function or struct, by
    /// index. Opaque during checking; codegen substitutes it away before
    /// any layout or ABI decision.
    Param(u32),
}

/// `int`, the default integer type.
pub const INT: Type = Type::Int(IntKind::I64);

impl Type {
    /// Reference types are heap pointers the GC must track. A bare type
    /// parameter is *not* a reference: it is opaque during checking (so
    /// `nil` is not assignable to it) and never survives to codegen.
    pub fn is_ref(&self) -> bool {
        matches!(
            self,
            Type::Str | Type::Struct(..) | Type::Array(_) | Type::Fn(..) | Type::Nil
        )
    }

    pub fn accepts_nil(&self) -> bool {
        self.is_ref()
    }

    pub fn int_kind(&self) -> Option<IntKind> {
        match self {
            Type::Int(k) => Some(*k),
            _ => None,
        }
    }

    /// Storage size in bytes for a struct field or array element (also its
    /// alignment). Sized integers use their natural size, `bool` is one byte,
    /// and everything else occupies 8 bytes.
    pub fn size_bytes(&self) -> u32 {
        match self {
            Type::Int(k) => k.size(),
            Type::Bool => 1,
            Type::Param(_) => unreachable!("type parameter not substituted before layout"),
            _ => 8,
        }
    }

    /// Substitute type parameters with an instantiation's type arguments.
    /// `args` must cover every parameter index that occurs in `self`.
    pub fn subst(&self, args: &[Type]) -> Type {
        match self {
            Type::Param(i) => args[*i as usize].clone(),
            Type::Array(elem) => Type::Array(Box::new(elem.subst(args))),
            Type::Fn(params, ret) => Type::Fn(
                params.iter().map(|p| p.subst(args)).collect(),
                Box::new(ret.subst(args)),
            ),
            Type::Struct(id, targs) => {
                Type::Struct(*id, targs.iter().map(|t| t.subst(args)).collect())
            }
            t => t.clone(),
        }
    }

    /// Does this type mention any type parameter?
    pub fn has_param(&self) -> bool {
        match self {
            Type::Param(_) => true,
            Type::Array(elem) => elem.has_param(),
            Type::Fn(params, ret) => params.iter().any(Type::has_param) || ret.has_param(),
            Type::Struct(_, targs) => targs.iter().any(Type::has_param),
            _ => false,
        }
    }

    pub fn display<'a>(
        &'a self,
        structs: &'a [StructInfo],
        params: &'a [String],
    ) -> TypeDisplay<'a> {
        TypeDisplay { ty: self, structs, params }
    }
}

/// Packed field layout: each field is aligned to its own size, in declaration
/// order. Returns the byte offset of each field (relative to the payload
/// start) and the total payload size rounded up to 8 bytes. Because every
/// alignment equals the field size and references are 8 bytes, reference
/// fields always land on 8-byte boundaries, which the GC refmap relies on.
pub fn layout_fields(types: &[Type]) -> (Vec<u32>, u32) {
    let mut off = 0u32;
    let mut offsets = Vec::with_capacity(types.len());
    for ty in types {
        let size = ty.size_bytes();
        off = (off + size - 1) & !(size - 1);
        offsets.push(off);
        off += size;
    }
    (offsets, (off + 7) & !7)
}

/// A struct *definition*. Field types may contain `Type::Param` indices
/// into `type_params`; per-instantiation layout (offsets, payload size,
/// GC descriptor) is computed in codegen.
#[derive(Debug)]
pub struct StructInfo {
    pub name: String,
    pub type_params: Vec<String>,
    pub fields: Vec<(String, Type)>,
    #[allow(dead_code)]
    pub line: u32,
}

pub struct TypeDisplay<'a> {
    ty: &'a Type,
    structs: &'a [StructInfo],
    params: &'a [String],
}

impl fmt::Display for TypeDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            Type::Unknown => write!(f, "<unknown>"),
            Type::Unit => write!(f, "unit"),
            Type::Int(k) => write!(f, "{}", k.name()),
            Type::Float => write!(f, "float"),
            Type::Bool => write!(f, "bool"),
            Type::Str => write!(f, "string"),
            Type::Nil => write!(f, "nil"),
            Type::Param(i) => match self.params.get(*i as usize) {
                Some(name) => write!(f, "{name}"),
                None => write!(f, "<param {i}>"),
            },
            Type::Struct(id, targs) => {
                write!(f, "{}", self.structs[*id as usize].name)?;
                if !targs.is_empty() {
                    write!(f, "<")?;
                    for (i, t) in targs.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", t.display(self.structs, self.params))?;
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
            Type::Array(elem) => write!(f, "[{}]", elem.display(self.structs, self.params)),
            Type::Fn(params, ret) => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p.display(self.structs, self.params))?;
                }
                write!(f, ")")?;
                if **ret != Type::Unit {
                    write!(f, ": {}", ret.display(self.structs, self.params))?;
                }
                Ok(())
            }
        }
    }
}
