//! Smaller bodies on the emitted text: constant ops (and `insertvalue`s of
//! constants) hoisted into the entry block once each, identical panic
//! blocks merged. LLVM folds them anyway; this only means less to read.

use rokc_core::{HashMap, HashSet};

/// Ops whose result is a constant when every operand is one.
const CONST_OPS: [&str; 6] = [
    "arith.constant ",
    "llvm.mlir.constant(",
    "llvm.mlir.zero ",
    "llvm.mlir.undef ",
    "llvm.mlir.addressof @",
    "llvm.insertvalue ",
];

/// `    %x = rest` (one result).
fn def(line: &str) -> Option<(&str, &str)> {
    let l = line.strip_prefix("    ")?;
    let (name, rest) = l.split_once(" = ")?;
    let ok = name.len() > 1
        && name.starts_with('%')
        && name[1..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    ok.then_some((name, rest))
}

/// `text` with every `%name` outside string literals passed to `f`.
fn map_values(text: &str, mut f: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut quoted, mut escaped) = (false, false);
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if quoted {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        if c == '%' {
            let n = rest[1..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            let name = &rest[..=n];
            match f(name) {
                Some(new) => out.push_str(&new),
                None => out.push_str(name),
            }
            rest = &rest[n + 1..];
            continue;
        }
        quoted = c == '"';
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Compact one function body (its entry block first, unlabeled).
#[must_use]
pub fn compact(body: &str) -> String {
    let mut rename: HashMap<String, String> = HashMap::default();
    let mut consts: HashSet<String> = HashSet::default();
    let mut seen: HashMap<String, String> = HashMap::default();
    let mut hoisted = String::new();
    let mut kept: Vec<&str> = Vec::new();
    for line in body.lines() {
        let hoist = def(line).and_then(|(name, rest)| {
            let op = CONST_OPS.iter().any(|p| rest.starts_with(p))
                && !rest.starts_with("llvm.mlir.addressof @rok_tls");
            if !op {
                return None;
            }
            let mut all_const = true;
            let rest = map_values(rest, |v| {
                let v = rename.get(v).map_or(v, String::as_str);
                all_const &= consts.contains(v);
                Some(v.to_owned())
            });
            all_const.then_some((name, rest))
        });
        let Some((name, rest)) = hoist else {
            kept.push(line);
            continue;
        };
        if let Some(first) = seen.get(&rest) {
            rename.insert(name.to_owned(), first.clone());
        } else {
            hoisted.push_str(&format!("    {name} = {rest}\n"));
            consts.insert(name.to_owned());
            seen.insert(rest, name.to_owned());
        }
    }
    let mut lines: Vec<String> = Vec::with_capacity(kept.len());
    for l in kept {
        lines.push(if l.contains('%') {
            map_values(l, |v| rename.get(v).cloned())
        } else {
            l.to_owned()
        });
    }
    let lines = merge_panic_blocks(lines);
    let mut out = hoisted;
    out.reserve(lines.iter().map(|l| l.len() + 1).sum());
    for l in lines {
        out.push_str(&l);
        out.push('\n');
    }
    out
}

/// One block per distinct `^rok_panicN` body; branches to a duplicate
/// go to the first.
fn merge_panic_blocks(lines: Vec<String>) -> Vec<String> {
    let label = |l: &str| {
        l.strip_prefix("  ^")
            .map(|_| l.trim_end_matches(':').trim().to_owned())
    };
    let mut first: HashMap<String, String> = HashMap::default();
    let mut alias: HashMap<String, String> = HashMap::default();
    let mut drop = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let Some(l) = label(&lines[i]).filter(|l| l.starts_with("^rok_panic")) else {
            i += 1;
            continue;
        };
        let end = (i + 1..lines.len())
            .find(|&j| lines[j].starts_with("  ^"))
            .unwrap_or(lines.len());
        let key = lines[i + 1..end].join("\n");
        match first.get(&key) {
            Some(f) => {
                alias.insert(l, f.clone());
                drop[i..end].iter_mut().for_each(|d| *d = true);
            }
            None => {
                first.insert(key, l);
            }
        }
        i = end;
    }
    if alias.is_empty() {
        return lines;
    }
    lines
        .into_iter()
        .zip(drop)
        .filter(|(_, d)| !d)
        .map(|(l, _)| {
            if !l.contains("^rok_panic") {
                return l;
            }
            let mut out = String::with_capacity(l.len());
            let mut rest = l.as_str();
            while let Some(at) = rest.find("^rok_panic") {
                out.push_str(&rest[..at]);
                let tail = &rest[at + "^rok_panic".len()..];
                let n = tail.bytes().take_while(u8::is_ascii_digit).count();
                let name = &rest[at..at + "^rok_panic".len() + n];
                out.push_str(alias.get(name).map_or(name, String::as_str));
                rest = &rest[at + name.len()..];
            }
            out.push_str(rest);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn constants_hoisted_once_and_panic_blocks_shared() {
        let body = "    %0 = llvm.mlir.constant(1 : i32) : i32
    %1 = llvm.load %p0 : !llvm.ptr -> i64
    %2 = arith.constant 7 : i64
    %3 = arith.addi %1, %2 : i64
    cf.cond_br %3, ^rok_panic1, ^rok_ok1
  ^rok_panic1:
    %4 = llvm.mlir.addressof @rok.str.a : !llvm.ptr
    %5 = llvm.mlir.undef : !s
    %6 = llvm.insertvalue %4, %5[0] : !s
    func.call @rok_panic_at(%6) : (!s) -> ()
    llvm.unreachable
  ^rok_ok1:
    %7 = arith.constant 7 : i64
    %8 = llvm.inline_asm \"mov %7\" %7 : i64
    cf.cond_br %8, ^rok_panic2, ^rok_ok2
  ^rok_panic2:
    %9 = llvm.mlir.addressof @rok.str.a : !llvm.ptr
    %10 = llvm.mlir.undef : !s
    %11 = llvm.insertvalue %9, %10[0] : !s
    func.call @rok_panic_at(%11) : (!s) -> ()
    llvm.unreachable
  ^rok_ok2:
    %12 = llvm.mlir.addressof @rok_tls_x : !llvm.ptr
    func.return
";
        let out = super::compact(body);
        assert_eq!(
            out,
            "    %0 = llvm.mlir.constant(1 : i32) : i32
    %2 = arith.constant 7 : i64
    %4 = llvm.mlir.addressof @rok.str.a : !llvm.ptr
    %5 = llvm.mlir.undef : !s
    %6 = llvm.insertvalue %4, %5[0] : !s
    %1 = llvm.load %p0 : !llvm.ptr -> i64
    %3 = arith.addi %1, %2 : i64
    cf.cond_br %3, ^rok_panic1, ^rok_ok1
  ^rok_panic1:
    func.call @rok_panic_at(%6) : (!s) -> ()
    llvm.unreachable
  ^rok_ok1:
    %8 = llvm.inline_asm \"mov %7\" %2 : i64
    cf.cond_br %8, ^rok_panic1, ^rok_ok2
  ^rok_ok2:
    %12 = llvm.mlir.addressof @rok_tls_x : !llvm.ptr
    func.return
"
        );
    }
}
