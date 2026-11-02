//! Seeded fuzzing of the ROKON reader / writer: random value trees must
//! survive `to_string` -> `parse` -> `to_string` (and the one-line form),
//! and mutations of their text must parse or fail, never panic or hang.
//! Budget `ROK_FUZZ_SECS` (default 2), seed `ROK_FUZZ_SEED`.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rokc_rokon::{Field, Kind, Value, parse, to_line, to_string};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn budget() -> Duration {
    let secs = std::env::var("ROK_FUZZ_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0_f64);
    Duration::from_secs_f64(secs)
}

fn seed() -> u64 {
    std::env::var("ROK_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x0005_eed0_0b0e)
}

const NAMES: &[&str] = &["a", "name", "x_1", "Config", "é", "dep", "r#type"];
const STRS: &[&str] = &[
    "",
    "plain",
    "quote \" back \\",
    "nl \n tab \t cr \r nul \0",
    "bell \u{7} c1 \u{85} rlo \u{202e}",
    "한글 😀",
    "{x}",
    "//",
];

fn value(rng: &mut Rng, depth: u32) -> Value {
    let scalar = depth > 3 || rng.below(3) == 0;
    let kind = if scalar {
        match rng.below(8) {
            0 => Kind::None,
            1 => Kind::Bool(rng.below(2) == 0),
            2 => Kind::Int(match rng.below(4) {
                0 => i128::from(rng.next() as i64),
                1 => i128::from(rng.next() as u8),
                2 => -i128::from(rng.below(1000) as u32),
                _ => i128::from(i64::MIN),
            }),
            3 => {
                let f = f64::from_bits(rng.next());
                Kind::Float(if f.is_finite() { f } else { 0.5 })
            }
            4 => Kind::Char(['a', '\'', '\\', '\n', '\0', '한', '😀', '\u{85}'][rng.below(8)]),
            _ => Kind::Str(STRS[rng.below(STRS.len())].to_owned()),
        }
    } else {
        let n = rng.below(5);
        let vs = |rng: &mut Rng| (0..n).map(|_| value(rng, depth + 1)).collect::<Vec<_>>();
        match rng.below(6) {
            0 => Kind::List(vs(rng)),
            1 => Kind::Tuple(vs(rng)),
            // Scalar keys, never `none`: `{ none: 1 }` reads as a block
            // (a documented limit).
            2 => Kind::Dict(
                (0..n)
                    .map(|_| {
                        let k = value(rng, 4);
                        let k = if matches!(k.kind, Kind::None) {
                            Value::new(Kind::Int(0))
                        } else {
                            k
                        };
                        (k, value(rng, depth + 1))
                    })
                    .collect(),
            ),
            3 => Kind::Struct {
                name: (rng.below(2) == 0).then(|| "Config".to_owned()),
                fields: fields(rng, n, depth),
            },
            4 => {
                let k = rng.below(2);
                Kind::Variant {
                    path: "Mode::Fast".into(),
                    args: vs(rng),
                    named: fields(rng, k, depth),
                    braces: false,
                }
            }
            _ => Kind::Variant {
                path: "Shape::Rect".into(),
                args: Vec::new(),
                named: fields(rng, n.max(1), depth),
                braces: true,
            },
        }
    };
    Value::new(kind)
}

fn fields(rng: &mut Rng, n: usize, depth: u32) -> Vec<Field> {
    (0..n)
        .map(|i| {
            Field::new(
                &format!("{}{i}", NAMES[rng.below(4)]),
                value(rng, depth + 1),
            )
        })
        .collect()
}

/// Mutate `text`: delete, duplicate or replace a few characters.
fn mutate(rng: &mut Rng, text: &str) -> String {
    const BITS: &[char] = &[
        '{', '}', '[', ']', '(', ')', ',', ':', '"', '\'', '\\', '-', '.', 'e', '0', '9', 'x', '_',
        '/', '*', '\n', ' ', '#', 'r', 'f', '😀',
    ];
    let mut cs: Vec<char> = text.chars().collect();
    for _ in 0..=rng.below(3) {
        if cs.is_empty() {
            break;
        }
        let i = rng.below(cs.len());
        let j = (i + 1 + rng.below(6)).min(cs.len());
        match rng.below(4) {
            0 => {
                cs.drain(i..j);
            }
            1 => {
                let dup: Vec<char> = cs[i..j].to_vec();
                cs.splice(i..i, dup);
            }
            2 => cs[i] = BITS[rng.below(BITS.len())],
            _ => cs.insert(i, BITS[rng.below(BITS.len())]),
        }
    }
    cs.into_iter().collect()
}

/// One input: `Err` describes a failure.
fn one(i: u64) -> Result<(), String> {
    let mut rng = Rng(seed() ^ i.wrapping_mul(0xa076_1d64_78bd_642f));
    let v = value(&mut rng, 0);
    let text = to_string(&v);
    let back = parse(&text)
        .map_err(|e| format!("canonical text does not parse: {}\n{text}", e.message))?;
    let again = to_string(&back);
    if again != text {
        return Err(format!(
            "round trip changed the text:\n{text}\n---\n{again}"
        ));
    }
    let line = to_line(&v);
    match parse(&line) {
        Ok(b) if to_string(&b) == text => {}
        _ => return Err(format!("one-line form does not read back:\n{line}")),
    }
    let bad = mutate(&mut rng, &text);
    let r = std::panic::catch_unwind(|| {
        // Whatever reads must write and read back the same.
        let v = parse(&bad).ok()?;
        let t = to_string(&v);
        let ok = parse(&t).is_ok_and(|w| to_string(&w) == t);
        (!ok).then(|| {
            let why = if t.contains("inf") || t.contains("NaN") {
                "float literal out of range reads as infinity"
            } else {
                "re-read differs"
            };
            format!("{why}:\n{bad}\n---\n{t}")
        })
    });
    match r {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e),
        Err(_) => Err(format!("panic on:\n{bad}")),
    }
}

#[test]
fn rokon_round_trip_and_mutations() {
    let deadline = Instant::now() + budget();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut i = 0u64;
        let mut fails = Vec::new();
        while Instant::now() < deadline {
            let _ = tx.send(i);
            if let Err(e) = one(i) {
                fails.push(format!("input {i}: {e}"));
                if fails.len() >= 5 {
                    break;
                }
            }
            i += 1;
        }
        (i, fails)
    });
    // Watchdog: an input that runs 10 s is a hang.
    let mut last = 0;
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(i) => last = i,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("input {last} hangs (seed {})", seed()),
        }
    }
    let (n, fails) = worker.join().unwrap();
    println!("rokon fuzz: {n} inputs, seed {}", seed());
    assert!(fails.is_empty(), "{}", fails.join("\n\n"));
}
