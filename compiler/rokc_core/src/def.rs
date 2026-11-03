//! Definition ids, assigned by name resolution and carried to codegen.

use crate::HashMap;

/// 32-bit index into `DefTable`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DefId(pub u32);

impl DefId {
    pub const DUMMY: Self = Self(u32::MAX);

    #[must_use]
    pub const fn is_dummy(self) -> bool {
        self.0 == u32::MAX
    }
}

/// How far an item is visible.
///
/// - `Pub`     external packages
/// - `Pkg`     same package only
/// - `Sub`     self + descendant modules
/// - `Private` same module only (default)
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Visibility {
    Pub,
    Pkg,
    Sub,
    #[default]
    Private,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefKind {
    Module,
    Function,
    Struct,
    Enum,
    EnumVariant,
    Union,
    Trait,
    Impl,
    TypeAlias,
    Const,
    Static,
    Extern,
    /// User-defined derive / attribute / `derive_attribute` block.
    MetaImpl,
}

#[derive(Clone, Debug)]
pub struct DefInfo {
    pub kind: DefKind,
    pub name: String,
    /// Module path, e.g. `["std", "vec"]`.
    pub module_path: Vec<String>,
    pub visibility: Visibility,
    /// Enclosing definition (module / impl block).
    pub parent: Option<DefId>,
}

#[derive(Clone, Debug, Default)]
pub struct DefTable {
    defs: Vec<DefInfo>,
    by_qpath: HashMap<String, DefId>,
}

impl DefTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, info: DefInfo) -> DefId {
        let qpath = qualified_path(&info.module_path, &info.name);
        if let Some(id) = self.by_qpath.get(&qpath) {
            return *id;
        }
        let id = DefId(u32::try_from(self.defs.len()).expect("DefId overflow"));
        self.defs.push(info);
        self.by_qpath.insert(qpath, id);
        id
    }

    #[must_use]
    pub fn get(&self, id: DefId) -> Option<&DefInfo> {
        self.defs.get(id.0 as usize)
    }

    #[must_use]
    pub fn lookup_qpath(&self, qpath: &str) -> Option<DefId> {
        self.by_qpath.get(qpath).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (DefId, &DefInfo)> {
        self.defs
            .iter()
            .enumerate()
            .map(|(i, info)| (DefId(u32::try_from(i).expect("DefId overflow")), info))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

fn qualified_path(module: &[String], name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{}::{}", module.join("::"), name)
    }
}

crate::codec_enum!(Visibility {
    Pub,
    Pkg,
    Sub,
    Private
});

impl crate::codec::Codec for DefId {
    fn enc(&self, e: &mut crate::codec::Enc) {
        e.u(u64::from(self.0));
    }
    fn dec(d: &mut crate::codec::Dec<'_>) -> Option<Self> {
        Some(Self(u32::try_from(d.u()?).ok()?))
    }
}
