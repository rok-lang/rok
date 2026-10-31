# Benchmarks

`bench/*.rok` are Rok benchmarks; `bench/c/*.c` are C counterparts of
some of them (same algorithm, same output). Not run by CI.

| benchmark        | what it stresses                                         | C |
|------------------|----------------------------------------------------------|---|
| `nbody`          | f64 arithmetic over parallel `List<f64>`                 | yes |
| `fannkuch`       | small integer lists, permutations, prefix reversals       | yes |
| `spectral_norm`  | calls + f64 loops                                         | yes |
| `binary_trees`   | short-lived trait objects, GC throughput                  | yes |
| `json_tokenizer` | building a 6 MB `str` with `StrBuf`, byte-wise scanning   | yes |
| `word_count`     | `split_whitespace` + `Dict<str, i64>` counting            | yes |
| `throw_parse`    | recursive throwing calls (`try`), errors caught per expr  | yes |
| `throw_wide`     | throwing calls with a `str` result and a two-word error   |   |
| `channel_contended` | 4 producer threads into one receiver, unbounded and bounded channels | |
| `file_rw`        | 16-byte `File` writes, reads and seeks (syscall-bound)    |   |
| `alloc_*`, `gc_*`, `store_heavy`, `wb_threads`, `timers`, `fstring_heavy` | allocation, collector, write barriers, timers, f-strings | |

## Running

```sh
python3 bench/bench.py                 # all, best of 3, record + compare
python3 bench/bench.py nbody --runs 5  # only names containing "nbody"
python3 bench/bench.py --no-record     # measure and compare, keep no record
python3 bench/bench.py --threshold 5   # regression threshold in % (default 3)
```

The runner builds the release `rokc` and runtime, compiles each
`bench/*.rok` at `-O2` and each `bench/c/*.c` with `clang -O2` into
`target/bench/`, and runs every program N times under `/usr/bin/time -l`
(macOS; on Linux GNU `time`, user time and RSS only). Per benchmark it
keeps the best (lowest) of the N runs of:

- **instructions retired**: the primary metric; nearly noise-free, so
  small regressions show;
- **user CPU seconds**;
- **peak RSS** (KiB).

Each run appends one JSON line to `bench/history.jsonl`: the git commit
(and whether the tree was dirty), commit subject, date, host, run count
and the results by benchmark (`c/<name>` for the C programs). The table
printed at the end compares against the previous line of the history:
the change in instructions (user time when no instruction count is
available) above the threshold is flagged `REGRESSION`; `vs C` is the
Rok / C instruction ratio. A Rok program whose output differs from its C
counterpart is flagged `OUTPUT MISMATCH`. Either makes the exit status 1.

Compare only records from the same host: instruction counts depend on
the CPU and the C compiler.

`ROK_GC_STATS=1` makes a Rok program print collections, pause totals
and collector CPU, then blocks / resident KiB / evacuations / bytes
moved at exit:

```sh
ROK_GC_STATS=1 target/bench/gc_old_heap
```
