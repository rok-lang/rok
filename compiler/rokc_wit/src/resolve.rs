//! Parsed WIT files to one resolved set of packages: every name bound to
//! its definition, `use` turned into aliases, world `include`s merged.

use rokc_core::HashMap;
use std::path::PathBuf;

use crate::parse::{
    self, Error, Extern, File, FuncKind, IfaceItem, PkgName, Pos, Prim, TopItem, Ty, TypeDecl,
    TypeDeclKind, UsePath, WorldItem,
};

pub type TypeId = usize;
pub type IfaceId = usize;
pub type WorldId = usize;
pub type PkgId = usize;

#[derive(Clone, Debug, Default)]
pub struct Resolve {
    pub packages: Vec<Package>,
    pub interfaces: Vec<Iface>,
    pub types: Vec<TypeDef>,
    pub worlds: Vec<WorldDef>,
    /// The files read, with their text (for messages).
    pub files: Vec<(PathBuf, String)>,
}

#[derive(Clone, Debug)]
pub struct Package {
    pub name: PkgName,
    pub interfaces: Vec<(String, IfaceId)>,
    pub worlds: Vec<(String, WorldId)>,
}

#[derive(Clone, Debug)]
pub struct Iface {
    pub name: String,
    pub pkg: PkgId,
    /// Declared inline in a world (`import name: interface { .. }`).
    pub inline: Option<WorldId>,
    pub types: Vec<(String, TypeId)>,
    pub funcs: Vec<Function>,
    /// The file it is in (index into `Resolve::files`).
    pub file: usize,
}

#[derive(Clone, Debug)]
pub struct TypeDef {
    pub name: String,
    pub owner: Owner,
    pub kind: TypeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Iface(IfaceId),
    World(WorldId),
}

#[derive(Clone, Debug)]
pub enum TypeKind {
    Alias(Type),
    Record(Vec<(String, Type)>),
    Variant(Vec<(String, Option<Type>)>),
    Enum(Vec<String>),
    Flags(Vec<String>),
    Resource,
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Prim(Prim),
    List(Box<Type>),
    FixedList(Box<Type>, u32),
    Option(Box<Type>),
    Result(Option<Box<Type>>, Option<Box<Type>>),
    Tuple(Vec<Type>),
    Own(TypeId),
    Borrow(TypeId),
    Id(TypeId),
    Future(Option<Box<Type>>),
    Stream(Option<Box<Type>>),
    ErrorContext,
}

impl Type {
    /// `self` with every type id `map` names replaced.
    #[must_use]
    pub fn remap(&self, map: &HashMap<TypeId, TypeId>) -> Self {
        let id = |t: &TypeId| *map.get(t).unwrap_or(t);
        let bx = |t: &Self| Box::new(t.remap(map));
        let opt = |t: &Option<Box<Self>>| t.as_deref().map(bx);
        match self {
            Self::Prim(_) | Self::ErrorContext => self.clone(),
            Self::List(t) => Self::List(bx(t)),
            Self::FixedList(t, n) => Self::FixedList(bx(t), *n),
            Self::Option(t) => Self::Option(bx(t)),
            Self::Result(a, b) => Self::Result(opt(a), opt(b)),
            Self::Tuple(ts) => Self::Tuple(ts.iter().map(|t| t.remap(map)).collect()),
            Self::Own(t) => Self::Own(id(t)),
            Self::Borrow(t) => Self::Borrow(id(t)),
            Self::Id(t) => Self::Id(id(t)),
            Self::Future(t) => Self::Future(opt(t)),
            Self::Stream(t) => Self::Stream(opt(t)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    Free,
    Method(TypeId),
    Static(TypeId),
    Constructor(TypeId),
}

#[derive(Clone, Debug)]
pub struct Function {
    /// The WIT name (`read`; empty for a constructor).
    pub name: String,
    pub kind: FnKind,
    /// `async func`.
    pub is_async: bool,
    pub params: Vec<(String, Type)>,
    pub result: Option<Type>,
}

#[derive(Clone, Debug)]
pub struct WorldDef {
    pub name: String,
    pub pkg: PkgId,
    pub types: Vec<(String, TypeId)>,
    pub imports: Vec<WorldEntry>,
    pub exports: Vec<WorldEntry>,
    pub file: usize,
}

#[derive(Clone, Debug)]
pub enum WorldEntry {
    /// A named interface (`import wasi:cli/env;`) or an inline one.
    Iface(IfaceId),
    Func(Function),
}

impl Function {
    /// The name the canonical ABI gives it (`[method]file.read`).
    pub fn wit_name(&self, r: &Resolve) -> String {
        match self.kind {
            FnKind::Free => self.name.clone(),
            FnKind::Method(t) => format!("[method]{}.{}", r.types[t].name, self.name),
            FnKind::Static(t) => format!("[static]{}.{}", r.types[t].name, self.name),
            FnKind::Constructor(t) => format!("[constructor]{}", r.types[t].name),
        }
    }
}

impl Resolve {
    /// `id` through its aliases.
    pub fn dealias(&self, mut id: TypeId) -> TypeId {
        while let TypeKind::Alias(Type::Id(t)) = &self.types[id].kind {
            id = *t;
        }
        id
    }

    /// The name an interface is imported / exported by: `ns:pkg/name@v`,
    /// or its plain name when inline in a world.
    pub fn iface_key(&self, i: IfaceId) -> String {
        let f = &self.interfaces[i];
        if f.inline.is_some() {
            return f.name.clone();
        }
        let p = &self.packages[f.pkg].name;
        let mut s = format!("{}:{}/{}", p.ns, p.name, f.name);
        if let Some(v) = &p.version {
            s.push('@');
            s.push_str(v);
        }
        s
    }

    fn find_pkg(&self, want: &PkgName) -> Option<PkgId> {
        let same = |p: &PkgName| p.ns == want.ns && p.name == want.name;
        if want.version.is_some() {
            return self.packages.iter().position(|p| p.name == *want);
        }
        let mut hits = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, p)| same(&p.name));
        let first = hits.next()?.0;
        hits.next().is_none().then_some(first)
    }

    /// Resolve `files`: each a parsed file and the package it belongs to
    /// when it declares none (the other files of its directory).
    pub fn new(files: Vec<(File, String)>) -> Result<Self, Error> {
        let mut r = Self::default();
        let mut cx = Cx::default();
        for (file, src) in files {
            let fi = r.files.len();
            r.files.push((file.path.clone(), src));
            if let Some(pkg) = &file.package {
                cx.declare(&mut r, fi, pkg.clone(), file.items)?;
            } else if !file.items.is_empty() {
                return Err(r.err(
                    fi,
                    Pos(0),
                    "a WIT file needs `package ns:name;` first".into(),
                ));
            }
            for (pkg, items) in file.nested {
                cx.declare(&mut r, fi, pkg, items)?;
            }
        }
        cx.resolve(&mut r)?;
        Ok(r)
    }

    pub(crate) fn err(&self, file: usize, pos: Pos, msg: String) -> Error {
        let (path, src) = &self.files[file];
        parse::error_pos(path, src, pos, msg)
    }
}

/// Unresolved parts kept for the later passes.
#[derive(Default)]
struct Cx {
    /// Interface bodies, by interface.
    iface_items: Vec<Vec<IfaceItem>>,
    /// World bodies, by world.
    world_items: Vec<Vec<WorldItem>>,
    /// `use ns:pkg/iface as name;` of each file.
    file_uses: HashMap<(usize, String), (UsePath, Pos)>,
    /// Names in scope in each interface / world.
    iface_scope: Vec<HashMap<String, TypeId>>,
    world_scope: Vec<HashMap<String, TypeId>>,
    /// The type declarations still to resolve: (type, decl, scope owner, file).
    pending: Vec<(TypeId, TypeDecl, Owner, usize)>,
    /// `use` aliases to resolve: (type, path, name, owner, file, pos).
    pending_uses: Vec<(TypeId, UsePath, String, Owner, usize, Pos)>,
    worlds_done: Vec<bool>,
}

impl Cx {
    fn declare(
        &mut self,
        r: &mut Resolve,
        file: usize,
        name: PkgName,
        items: Vec<TopItem>,
    ) -> Result<(), Error> {
        let pkg = if let Some(p) = r.packages.iter().position(|p| p.name == name) {
            p
        } else {
            r.packages.push(Package {
                name,
                interfaces: Vec::new(),
                worlds: Vec::new(),
            });
            r.packages.len() - 1
        };
        for item in items {
            match item {
                TopItem::Interface(i) => {
                    if r.packages[pkg].interfaces.iter().any(|(n, _)| *n == i.name) {
                        return Err(r.err(
                            file,
                            i.pos,
                            format!("interface `{}` is defined twice", i.name),
                        ));
                    }
                    let id = r.interfaces.len();
                    r.interfaces.push(Iface {
                        name: i.name.clone(),
                        pkg,
                        inline: None,
                        types: Vec::new(),
                        funcs: Vec::new(),
                        file,
                    });
                    self.iface_items.push(i.items);
                    r.packages[pkg].interfaces.push((i.name, id));
                }
                TopItem::World(w) => {
                    if r.packages[pkg].worlds.iter().any(|(n, _)| *n == w.name) {
                        return Err(r.err(
                            file,
                            w.pos,
                            format!("world `{}` is defined twice", w.name),
                        ));
                    }
                    let id = r.worlds.len();
                    r.worlds.push(WorldDef {
                        name: w.name.clone(),
                        pkg,
                        types: Vec::new(),
                        imports: Vec::new(),
                        exports: Vec::new(),
                        file,
                    });
                    self.world_items.push(w.items);
                    r.packages[pkg].worlds.push((w.name, id));
                }
                TopItem::Use(path, alias, pos) => {
                    self.file_uses.insert((file, alias), (path, pos));
                }
            }
        }
        Ok(())
    }

    /// The interface `path` names from `file` of package `pkg`.
    fn lookup_iface(
        &self,
        r: &Resolve,
        pkg: PkgId,
        file: usize,
        path: &UsePath,
        pos: Pos,
    ) -> Result<IfaceId, Error> {
        match path {
            UsePath::Local(n) => {
                if let Some((_, id)) = r.packages[pkg].interfaces.iter().find(|(m, _)| m == n) {
                    return Ok(*id);
                }
                if let Some((p, at)) = self.file_uses.get(&(file, n.clone())) {
                    return self.lookup_iface(r, pkg, file, p, *at);
                }
                Err(r.err(file, pos, format!("no interface `{n}`")))
            }
            UsePath::Foreign { pkg: want, iface } => {
                let Some(p) = r.find_pkg(want) else {
                    return Err(r.err(
                        file,
                        pos,
                        format!("package `{want}` not found (WIT dependencies go in a `deps/` directory beside the package's files)"),
                    ));
                };
                r.packages[p]
                    .interfaces
                    .iter()
                    .find(|(m, _)| m == iface)
                    .map(|(_, id)| *id)
                    .ok_or_else(|| {
                        r.err(
                            file,
                            pos,
                            format!("package `{want}` has no interface `{iface}`"),
                        )
                    })
            }
        }
    }

    fn resolve(&mut self, r: &mut Resolve) -> Result<(), Error> {
        self.iface_scope = vec![HashMap::default(); r.interfaces.len()];
        self.world_scope = vec![HashMap::default(); r.worlds.len()];
        self.worlds_done = vec![false; r.worlds.len()];
        for i in 0..r.interfaces.len() {
            let items = std::mem::take(&mut self.iface_items[i]);
            self.scan_iface(r, i, items)?;
        }
        for w in 0..r.worlds.len() {
            self.scan_world(r, w)?;
        }
        self.finish_types(r)?;
        for w in 0..r.worlds.len() {
            self.resolve_world(r, w)?;
        }
        Ok(())
    }

    /// Give the types (and `use`d names) of interface `i` their ids;
    /// functions resolve later, once every name has one.
    fn scan_iface(
        &mut self,
        r: &mut Resolve,
        i: IfaceId,
        items: Vec<IfaceItem>,
    ) -> Result<(), Error> {
        let file = r.interfaces[i].file;
        let owner = Owner::Iface(i);
        let mut funcs = Vec::new();
        for item in items {
            match item {
                IfaceItem::Type(t) => self.add_type(r, owner, file, t)?,
                IfaceItem::Use(u) => self.add_use(r, owner, file, u)?,
                IfaceItem::Func(f) => funcs.push(f),
            }
        }
        self.iface_items[i] = funcs.into_iter().map(IfaceItem::Func).collect();
        Ok(())
    }

    fn scope_mut(&mut self, owner: Owner) -> &mut HashMap<String, TypeId> {
        match owner {
            Owner::Iface(i) => &mut self.iface_scope[i],
            Owner::World(w) => &mut self.world_scope[w],
        }
    }

    fn new_type(
        &mut self,
        r: &mut Resolve,
        owner: Owner,
        file: usize,
        name: &str,
        pos: Pos,
    ) -> Result<TypeId, Error> {
        let id = r.types.len();
        if self.scope_mut(owner).insert(name.to_owned(), id).is_some() {
            return Err(r.err(file, pos, format!("type `{name}` is defined twice")));
        }
        r.types.push(TypeDef {
            name: name.to_owned(),
            owner,
            kind: TypeKind::Unresolved,
        });
        match owner {
            Owner::Iface(i) => r.interfaces[i].types.push((name.to_owned(), id)),
            Owner::World(w) => r.worlds[w].types.push((name.to_owned(), id)),
        }
        Ok(id)
    }

    fn add_type(
        &mut self,
        r: &mut Resolve,
        owner: Owner,
        file: usize,
        t: TypeDecl,
    ) -> Result<(), Error> {
        let id = self.new_type(r, owner, file, &t.name, t.pos)?;
        self.pending.push((id, t, owner, file));
        Ok(())
    }

    fn add_use(
        &mut self,
        r: &mut Resolve,
        owner: Owner,
        file: usize,
        u: parse::Use,
    ) -> Result<(), Error> {
        for (name, alias) in u.names {
            let local = alias.unwrap_or_else(|| name.clone());
            let id = self.new_type(r, owner, file, &local, u.pos)?;
            self.pending_uses
                .push((id, u.path.clone(), name, owner, file, u.pos));
        }
        Ok(())
    }

    fn owner_pkg(r: &Resolve, owner: Owner) -> PkgId {
        match owner {
            Owner::Iface(i) => r.interfaces[i].pkg,
            Owner::World(w) => r.worlds[w].pkg,
        }
    }

    /// Types of worlds and their inline interfaces get ids.
    fn scan_world(&mut self, r: &mut Resolve, w: WorldId) -> Result<(), Error> {
        let file = r.worlds[w].file;
        let items = std::mem::take(&mut self.world_items[w]);
        let mut rest = Vec::new();
        for item in items {
            match item {
                WorldItem::Type(t) => self.add_type(r, Owner::World(w), file, t)?,
                WorldItem::Use(u) => self.add_use(r, Owner::World(w), file, u)?,
                WorldItem::Import(Extern::Interface(name, items, pos)) => {
                    let path = self.inline_iface(r, w, file, name, items)?;
                    rest.push(WorldItem::Import(Extern::Path(path, pos)));
                }
                WorldItem::Export(Extern::Interface(name, items, pos)) => {
                    let path = self.inline_iface(r, w, file, name, items)?;
                    rest.push(WorldItem::Export(Extern::Path(path, pos)));
                }
                other => rest.push(other),
            }
        }
        self.world_items[w] = rest;
        Ok(())
    }

    /// An interface declared inside world `w`: a path naming its id.
    fn inline_iface(
        &mut self,
        r: &mut Resolve,
        w: WorldId,
        file: usize,
        name: String,
        items: Vec<IfaceItem>,
    ) -> Result<UsePath, Error> {
        let id = r.interfaces.len();
        r.interfaces.push(Iface {
            name,
            pkg: r.worlds[w].pkg,
            inline: Some(w),
            types: Vec::new(),
            funcs: Vec::new(),
            file,
        });
        self.iface_scope.push(HashMap::default());
        self.iface_items.push(Vec::new());
        self.scan_iface(r, id, items)?;
        Ok(UsePath::Local(format!("\0{id}")))
    }

    fn finish_types(&mut self, r: &mut Resolve) -> Result<(), Error> {
        // A `use`d name is an alias of the name it imports (which may be
        // an alias itself).
        for (id, path, name, owner, file, pos) in std::mem::take(&mut self.pending_uses) {
            let pkg = Self::owner_pkg(r, owner);
            let target = self.lookup_iface(r, pkg, file, &path, pos)?;
            let Some(&t) = self.iface_scope[target].get(&name) else {
                return Err(r.err(
                    file,
                    pos,
                    format!("interface `{}` has no type `{name}`", r.iface_key(target)),
                ));
            };
            r.types[id].kind = TypeKind::Alias(Type::Id(t));
        }
        for (id, decl, owner, file) in std::mem::take(&mut self.pending) {
            let kind = match decl.kind {
                TypeDeclKind::Alias(t) => TypeKind::Alias(self.ty(r, owner, file, &t)?),
                TypeDeclKind::Record(fields) => {
                    let mut out = Vec::new();
                    for (n, t) in fields {
                        out.push((n, self.ty(r, owner, file, &t)?));
                    }
                    if out.is_empty() {
                        return Err(r.err(
                            file,
                            decl.pos,
                            format!("record `{}` has no fields", decl.name),
                        ));
                    }
                    TypeKind::Record(out)
                }
                TypeDeclKind::Variant(cases) => {
                    let mut out = Vec::new();
                    for (n, t) in cases {
                        let t = t.map(|t| self.ty(r, owner, file, &t)).transpose()?;
                        out.push((n, t));
                    }
                    if out.is_empty() {
                        return Err(r.err(
                            file,
                            decl.pos,
                            format!("variant `{}` has no cases", decl.name),
                        ));
                    }
                    TypeKind::Variant(out)
                }
                TypeDeclKind::Enum(cases) => {
                    if cases.is_empty() {
                        return Err(r.err(
                            file,
                            decl.pos,
                            format!("enum `{}` has no cases", decl.name),
                        ));
                    }
                    TypeKind::Enum(cases)
                }
                TypeDeclKind::Flags(names) => {
                    if names.len() > 32 {
                        return Err(r.err(
                            file,
                            decl.pos,
                            format!(
                                "flags `{}`: more than 32 flags are not supported",
                                decl.name
                            ),
                        ));
                    }
                    TypeKind::Flags(names)
                }
                TypeDeclKind::Resource(funcs) => {
                    let Owner::Iface(i) = owner else {
                        return Err(r.err(
                            file,
                            decl.pos,
                            "resources in worlds are not supported; define them in an interface"
                                .into(),
                        ));
                    };
                    for f in funcs {
                        let kind = match f.kind {
                            FuncKind::Constructor => FnKind::Constructor(id),
                            FuncKind::Static => FnKind::Static(id),
                            FuncKind::Method | FuncKind::Free => FnKind::Method(id),
                        };
                        let mut func = self.func(r, owner, file, &f, kind)?;
                        if let FnKind::Method(_) = kind {
                            func.params.insert(0, ("self".into(), Type::Borrow(id)));
                        }
                        if let FnKind::Constructor(_) = kind {
                            func.result = Some(constructor_result(r, file, &f, id, func.result)?);
                        }
                        r.interfaces[i].funcs.push(func);
                    }
                    TypeKind::Resource
                }
            };
            r.types[id].kind = kind;
        }
        for i in 0..r.interfaces.len() {
            let file = r.interfaces[i].file;
            for item in std::mem::take(&mut self.iface_items[i]) {
                if let IfaceItem::Func(f) = item {
                    let func = self.func(r, Owner::Iface(i), file, &f, FnKind::Free)?;
                    r.interfaces[i].funcs.push(func);
                }
            }
        }
        // Alias cycles.
        for id in 0..r.types.len() {
            let mut seen = 0;
            let mut t = id;
            while let TypeKind::Alias(Type::Id(n)) = &r.types[t].kind {
                t = *n;
                seen += 1;
                if seen > r.types.len() {
                    let file = match r.types[id].owner {
                        Owner::Iface(i) => r.interfaces[i].file,
                        Owner::World(w) => r.worlds[w].file,
                    };
                    return Err(r.err(
                        file,
                        Pos(0),
                        format!("type `{}` is defined by itself", r.types[id].name),
                    ));
                }
            }
        }
        Ok(())
    }

    fn func(
        &self,
        r: &Resolve,
        owner: Owner,
        file: usize,
        f: &parse::Func,
        kind: FnKind,
    ) -> Result<Function, Error> {
        let mut params = Vec::new();
        for (n, t) in &f.params {
            if params.iter().any(|(m, _): &(String, Type)| m == n) {
                return Err(r.err(file, f.pos, format!("parameter `{n}` is given twice")));
            }
            params.push((n.clone(), self.ty(r, owner, file, t)?));
        }
        let result = f
            .result
            .as_ref()
            .map(|t| self.ty(r, owner, file, t))
            .transpose()?;
        Ok(Function {
            name: f.name.clone(),
            kind,
            is_async: f.is_async,
            params,
            result,
        })
    }

    fn named(
        &self,
        r: &Resolve,
        owner: Owner,
        file: usize,
        n: &str,
        pos: Pos,
    ) -> Result<TypeId, Error> {
        let scope = match owner {
            Owner::Iface(i) => &self.iface_scope[i],
            Owner::World(w) => &self.world_scope[w],
        };
        if let Some(&t) = scope.get(n) {
            return Ok(t);
        }
        // An inline interface sees its world's types.
        if let Owner::Iface(i) = owner {
            if let Some(w) = r.interfaces[i].inline {
                if let Some(&t) = self.world_scope[w].get(n) {
                    return Ok(t);
                }
            }
        }
        Err(r.err(file, pos, format!("no type `{n}`")))
    }

    fn ty(&self, r: &Resolve, owner: Owner, file: usize, t: &Ty) -> Result<Type, Error> {
        let go = |t: &Ty| self.ty(r, owner, file, t).map(Box::new);
        let opt = |t: Option<&Ty>| t.map(go).transpose();
        Ok(match t {
            Ty::Prim(p) => Type::Prim(*p),
            Ty::List(t) => Type::List(go(t)?),
            Ty::FixedList(t, n) => Type::FixedList(go(t)?, *n),
            Ty::Future(t) => Type::Future(opt(t.as_deref())?),
            Ty::Stream(t) => Type::Stream(opt(t.as_deref())?),
            Ty::ErrorContext => Type::ErrorContext,
            Ty::Option(t) => Type::Option(go(t)?),
            Ty::Result(a, b) => Type::Result(opt(a.as_deref())?, opt(b.as_deref())?),
            Ty::Tuple(ts) => Type::Tuple(
                ts.iter()
                    .map(|t| self.ty(r, owner, file, t))
                    .collect::<Result<_, _>>()?,
            ),
            Ty::Named(n, pos) => Type::Id(self.named(r, owner, file, n, *pos)?),
            Ty::Own(n, pos) | Ty::Borrow(n, pos) => {
                let id = self.named(r, owner, file, n, *pos)?;
                if !matches!(
                    r.types[r.dealias(id)].kind,
                    TypeKind::Resource | TypeKind::Unresolved
                ) {
                    return Err(r.err(file, *pos, format!("`{n}` is not a resource")));
                }
                if matches!(t, Ty::Own(..)) {
                    Type::Own(id)
                } else {
                    Type::Borrow(id)
                }
            }
        })
    }

    fn resolve_world(&mut self, r: &mut Resolve, w: WorldId) -> Result<(), Error> {
        if self.worlds_done[w] {
            return Ok(());
        }
        self.worlds_done[w] = true;
        let file = r.worlds[w].file;
        let pkg = r.worlds[w].pkg;
        for item in std::mem::take(&mut self.world_items[w]) {
            let (import, e) = match item {
                WorldItem::Import(e) => (true, e),
                WorldItem::Export(e) => (false, e),
                WorldItem::Include(path, renames, pos) => {
                    let inc = Self::lookup_world(r, pkg, file, &path, pos)?;
                    self.resolve_world(r, inc)?;
                    let (imports, exports) =
                        (r.worlds[inc].imports.clone(), r.worlds[inc].exports.clone());
                    let mut used = vec![false; renames.len()];
                    for (import, list) in [(true, imports), (false, exports)] {
                        for e in list {
                            let e = rename_entry(r, w, e, &renames, &mut used);
                            add_entry(r, w, import, e);
                        }
                    }
                    if let Some(i) = used.iter().position(|u| !u) {
                        return Err(r.err(
                            file,
                            pos,
                            format!(
                                "`include .. with`: the world has no import or export named `{}` to rename",
                                renames[i].0
                            ),
                        ));
                    }
                    continue;
                }
                WorldItem::Use(_) | WorldItem::Type(_) => continue,
            };
            let entry = match e {
                Extern::Path(UsePath::Local(n), _) if n.starts_with('\0') => {
                    WorldEntry::Iface(n[1..].parse().unwrap_or_default())
                }
                Extern::Path(path, pos) => {
                    WorldEntry::Iface(self.lookup_iface(r, pkg, file, &path, pos)?)
                }
                Extern::Func(f) => {
                    WorldEntry::Func(self.func(r, Owner::World(w), file, &f, FnKind::Free)?)
                }
                Extern::Interface(..) => continue,
            };
            add_entry(r, w, import, entry);
        }
        Ok(())
    }

    fn lookup_world(
        r: &Resolve,
        pkg: PkgId,
        file: usize,
        path: &UsePath,
        pos: Pos,
    ) -> Result<WorldId, Error> {
        let (p, name) = match path {
            UsePath::Local(n) => (pkg, n),
            UsePath::Foreign { pkg: want, iface } => {
                let p = r
                    .find_pkg(want)
                    .ok_or_else(|| r.err(file, pos, format!("package `{want}` not found")))?;
                (p, iface)
            }
        };
        r.packages[p]
            .worlds
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id)
            .ok_or_else(|| r.err(file, pos, format!("no world `{name}`")))
    }
}

/// The declared result of a constructor: its resource, or `result<r,
/// E>` (a fallible one).
fn constructor_result(
    r: &Resolve,
    file: usize,
    f: &parse::Func,
    res: TypeId,
    written: Option<Type>,
) -> Result<Type, Error> {
    let Some(t) = written else {
        return Ok(Type::Own(res));
    };
    let is_res = |t: &Type| matches!(t, Type::Id(i) | Type::Own(i) if r.dealias(*i) == res);
    match t {
        Type::Result(Some(ok), err) if is_res(&ok) => {
            Ok(Type::Result(Some(Box::new(Type::Own(res))), err))
        }
        _ => Err(r.err(
            file,
            f.pos,
            format!(
                "a fallible constructor returns `result<{}, ..>`",
                r.types[res].name
            ),
        )),
    }
}

/// `e` of an included world under `include .. with { from as to }`: a
/// function or inline interface by a plain name takes its new name (an
/// interface as a copy, so the including world's may differ).
fn rename_entry(
    r: &mut Resolve,
    w: WorldId,
    e: WorldEntry,
    renames: &[(String, String)],
    used: &mut [bool],
) -> WorldEntry {
    let name = match &e {
        WorldEntry::Func(f) => f.name.clone(),
        WorldEntry::Iface(i) if r.interfaces[*i].inline.is_some() => r.interfaces[*i].name.clone(),
        WorldEntry::Iface(_) => return e,
    };
    let Some(k) = renames.iter().position(|(from, _)| *from == name) else {
        return e;
    };
    used[k] = true;
    let to = renames[k].1.clone();
    match e {
        WorldEntry::Func(mut f) => {
            f.name = to;
            WorldEntry::Func(f)
        }
        WorldEntry::Iface(i) => WorldEntry::Iface(copy_iface(r, i, to, w)),
    }
}

/// A copy of inline interface `i` named `name` in world `w`, with its
/// own types.
fn copy_iface(r: &mut Resolve, i: IfaceId, name: String, w: WorldId) -> IfaceId {
    let id = r.interfaces.len();
    let src = r.interfaces[i].clone();
    let mut map = HashMap::default();
    for (_, t) in &src.types {
        map.insert(*t, r.types.len() + map.len());
    }
    let mut types = Vec::new();
    for (n, t) in &src.types {
        let old = &r.types[*t];
        let kind = match &old.kind {
            TypeKind::Alias(a) => TypeKind::Alias(a.remap(&map)),
            TypeKind::Record(fs) => {
                TypeKind::Record(fs.iter().map(|(n, t)| (n.clone(), t.remap(&map))).collect())
            }
            TypeKind::Variant(cs) => TypeKind::Variant(
                cs.iter()
                    .map(|(n, t)| (n.clone(), t.as_ref().map(|t| t.remap(&map))))
                    .collect(),
            ),
            k => k.clone(),
        };
        types.push(TypeDef {
            name: old.name.clone(),
            owner: Owner::Iface(id),
            kind,
        });
        let _ = n;
    }
    r.types.extend(types);
    let remap_kind = |k: FnKind| match k {
        FnKind::Free => FnKind::Free,
        FnKind::Method(t) => FnKind::Method(*map.get(&t).unwrap_or(&t)),
        FnKind::Static(t) => FnKind::Static(*map.get(&t).unwrap_or(&t)),
        FnKind::Constructor(t) => FnKind::Constructor(*map.get(&t).unwrap_or(&t)),
    };
    let funcs = src
        .funcs
        .iter()
        .map(|f| Function {
            name: f.name.clone(),
            kind: remap_kind(f.kind),
            is_async: f.is_async,
            params: f
                .params
                .iter()
                .map(|(n, t)| (n.clone(), t.remap(&map)))
                .collect(),
            result: f.result.as_ref().map(|t| t.remap(&map)),
        })
        .collect();
    r.interfaces.push(Iface {
        name,
        pkg: r.worlds[w].pkg,
        inline: Some(w),
        types: src.types.iter().map(|(n, t)| (n.clone(), map[t])).collect(),
        funcs,
        file: src.file,
    });
    id
}

fn add_entry(r: &mut Resolve, w: WorldId, import: bool, e: WorldEntry) {
    let same = |a: &WorldEntry, b: &WorldEntry| match (a, b) {
        (WorldEntry::Iface(x), WorldEntry::Iface(y)) => x == y,
        (WorldEntry::Func(x), WorldEntry::Func(y)) => x.name == y.name,
        _ => false,
    };
    let list = if import {
        &mut r.worlds[w].imports
    } else {
        &mut r.worlds[w].exports
    };
    if !list.iter().any(|x| same(x, &e)) {
        list.push(e);
    }
}
