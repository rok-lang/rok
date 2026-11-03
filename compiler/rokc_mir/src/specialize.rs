//! Closure specialization: a call passing a function value whose code
//! is known (a closure or named fn built in the caller) to a function
//! that calls it goes to a copy of the callee where that code is
//! constant, so the call through it becomes direct (and inlinable).

use rokc_core::HashMap;

use rokc_ir_common::{FunctionId, LocalId, MONO_MARK};

use crate::{MirFunction, MirTy, Operand, RValue, Statement};

/// Copies of one function at most: bounds the code growth.
const MAX_COPIES: usize = 8;

/// A function value `{code, env}` (`ty::fat_fn`).
fn is_fn_value(ty: &MirTy) -> bool {
    matches!(ty, MirTy::Tuple(ps) if matches!(ps.as_slice(), [MirTy::Fn(..), MirTy::GcRef]))
}

/// Locals of `f` assigned exactly once.
fn single_defs(f: &MirFunction) -> HashMap<LocalId, &RValue> {
    let mut defs: HashMap<LocalId, Option<&RValue>> = HashMap::default();
    for s in f.blocks.iter().flat_map(|b| &b.stmts) {
        if let Statement::Assign { target, rvalue } = s {
            defs.entry(*target)
                .and_modify(|d| *d = None)
                .or_insert(Some(rvalue));
        }
    }
    for b in &f.blocks {
        for p in &b.params {
            defs.insert(*p, None);
        }
    }
    defs.into_iter()
        .filter_map(|(l, d)| Some((l, d?)))
        .collect()
}

/// The code of function value `l` when known in `f`: built here from a
/// function's address, or (`known`) a parameter `f` was specialized on.
fn known_code(
    l: LocalId,
    defs: &HashMap<LocalId, &RValue>,
    known: &HashMap<LocalId, FunctionId>,
    depth: u32,
) -> Option<FunctionId> {
    if let Some(g) = known.get(&l) {
        return Some(*g);
    }
    if depth == 0 {
        return None;
    }
    match defs.get(&l)? {
        RValue::Use(Operand::Copy(x)) => known_code(*x, defs, known, depth - 1),
        RValue::StructInit { ty, fields } if is_fn_value(ty) => match fields.first()? {
            Operand::Copy(c) => match defs.get(c)? {
                RValue::FnAddr { callee, .. } => Some(*callee),
                _ => None,
            },
            Operand::Const(_) => None,
        },
        _ => None,
    }
}

/// Parameters of `f` that are function values never written: copies of
/// `f` may take their code as known.
fn fixed_fn_params(f: &MirFunction) -> Vec<usize> {
    let written: rokc_core::HashSet<LocalId> = f
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            Statement::Assign { target, .. } => Some(*target),
            _ => None,
        })
        .collect();
    f.params
        .iter()
        .enumerate()
        .filter(|(i, p)| is_fn_value(&f.param_tys[*i]) && !written.contains(p))
        .map(|(i, _)| i)
        .collect()
}

/// Specialize the calls of every function in `functions` (see the
/// module doc); `next_id` is the first unused function id. Returns each
/// copy with the function it copies.
pub fn closures(
    functions: &mut Vec<MirFunction>,
    mut next_id: u32,
) -> Vec<(FunctionId, FunctionId)> {
    let mut made = Vec::new();
    let index: HashMap<FunctionId, usize> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id, i))
        .collect();
    let fixed: HashMap<FunctionId, Vec<usize>> = functions
        .iter()
        .filter(|f| {
            f.facts.linkage == rokc_ir_common::Linkage::Defined
                && f.facts.export_name.is_none()
                && !f.blocks.is_empty()
        })
        .map(|f| (f.id, fixed_fn_params(f)))
        .filter(|(_, ps)| !ps.is_empty())
        .collect();
    // (callee, parameter, code) → copy.
    let mut copies: HashMap<(FunctionId, usize, FunctionId), FunctionId> = HashMap::default();
    let mut count: HashMap<FunctionId, usize> = HashMap::default();
    let mut built: rokc_core::HashSet<FunctionId> = rokc_core::HashSet::default();
    // Functions to scan, with the parameters known in each.
    let mut work: Vec<(usize, HashMap<LocalId, FunctionId>)> = (0..functions.len())
        .map(|i| (i, HashMap::default()))
        .collect();
    while let Some((fi, known)) = work.pop() {
        let mut rewrites: Vec<(usize, usize, FunctionId)> = Vec::new();
        {
            let f = &functions[fi];
            let defs = single_defs(f);
            for (bi, b) in f.blocks.iter().enumerate() {
                for (si, s) in b.stmts.iter().enumerate() {
                    let Statement::Assign {
                        rvalue: RValue::Call { callee, args, .. },
                        ..
                    } = s
                    else {
                        continue;
                    };
                    let Some(ps) = fixed.get(callee) else {
                        continue;
                    };
                    // The first parameter with a known code.
                    let hit = ps.iter().find_map(|&p| match args.get(p)? {
                        Operand::Copy(l) => Some((p, known_code(*l, &defs, &known, 4)?)),
                        Operand::Const(_) => None,
                    });
                    let Some((p, g)) = hit else { continue };
                    let key = (*callee, p, g);
                    if let Some(id) = copies.get(&key) {
                        rewrites.push((bi, si, *id));
                        continue;
                    }
                    let n = count.entry(*callee).or_default();
                    if *n >= MAX_COPIES {
                        continue;
                    }
                    *n += 1;
                    let id = FunctionId(next_id);
                    next_id += 1;
                    copies.insert(key, id);
                    rewrites.push((bi, si, id));
                }
            }
        }
        for (bi, si, id) in rewrites {
            if let Statement::Assign {
                rvalue: RValue::Call { callee, .. },
                ..
            } = &mut functions[fi].blocks[bi].stmts[si]
            {
                *callee = id;
            }
        }
        // Build the copies made for this function's calls.
        let mut pending: Vec<((FunctionId, usize, FunctionId), FunctionId)> = copies
            .iter()
            .filter(|(_, id)| !built.contains(*id))
            .map(|(k, id)| (*k, *id))
            .collect();
        pending.sort_by_key(|(_, id)| id.0);
        for ((orig, p, g), id) in pending {
            built.insert(id);
            let mut copy = functions[index[&orig]].clone();
            copy.id = id;
            made.push((id, orig));
            let args = copy.facts.mono_args.take().unwrap_or_default();
            let tag = format!("{args}|fn{p}={}", functions[index[&g]].name);
            copy.name = format!("{}{MONO_MARK}{tag}", copy.path());
            copy.facts.mono_args = Some(tag);
            let param = copy.params[p];
            // Its code: `g`.
            for s in copy.blocks.iter_mut().flat_map(|b| &mut b.stmts) {
                if let Statement::Assign { rvalue, .. } = s {
                    if let RValue::FieldRead {
                        target: Operand::Copy(t),
                        index: 0,
                        field_ty,
                    } = rvalue
                    {
                        if *t == param {
                            *rvalue = RValue::FnAddr {
                                callee: g,
                                fn_ty: field_ty.clone(),
                            };
                        }
                    }
                }
            }
            functions.push(copy);
            work.push((functions.len() - 1, HashMap::from_iter([(param, g)])));
        }
    }
    made
}
