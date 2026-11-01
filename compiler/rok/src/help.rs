//! `rok help`: the command list, and each command's own help. Commands
//! rok passes to rokc take their text from rokc's table, written as `rok`.

use rokc_core::cli::{self, CmdHelp, ROKC_COMMANDS};
use rokc_core::term::{self, Depth, Stream};

/// rok's global options, anywhere before `--`.
pub const GLOBAL: &[(&str, &str)] = &[
    (
        "--env <name>",
        "work in that environment (REPL, Jupyter, install)",
    ),
    ("--offline", "never fetch (git dependencies must be cached)"),
    ("--locked", "fail instead of changing rok.lock"),
    ("--cwd <dir>", "change directory first"),
    ("--color <when>", "auto|always|never (--no-color: never)"),
    ("-q, --quiet", "less output"),
    ("-v, --verbose", "more output"),
];

/// rok's feature options, for the commands that resolve dependencies.
pub const FEATURES: &[(&str, &str)] = &[
    (
        "--features <f,..>",
        "the package's features, or `<dep>/<f>` of a dependency",
    ),
    ("--all-features", "every feature of the package"),
    ("--no-default-features", "not its `default` feature"),
];

/// The commands that take the feature options.
pub const WITH_FEATURES: [&str; 12] = [
    "build", "check", "run", "test", "bench", "lint", "doc", "emit", "repl", "lsp", "jupyter",
    "deps",
];

/// The command list of `rok --help`, in groups.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "packages",
        &["new", "init", "add", "remove", "update", "deps", "search"],
    ),
    (
        "building",
        &[
            "build", "run", "check", "test", "bench", "lint", "fmt", "doc", "clean", "emit", "repl",
        ],
    ),
    (
        "stones",
        &["install", "uninstall", "publish", "yank", "login", "logout"],
    ),
    (
        "toolchains and tools",
        &[
            "toolchain",
            "env",
            "jupyter",
            "lsp",
            "completions",
            "demangle",
            "profdata",
            "version",
            "help",
        ],
    ),
];

/// The rokc commands rok runs; their help is rokc's.
const FROM_ROKC: [&str; 14] = [
    "build", "run", "check", "test", "bench", "lint", "fmt", "doc", "clean", "emit", "lsp",
    "demangle", "profdata", "help",
];

/// rok's own commands.
pub const ROK_COMMANDS: &[CmdHelp] = &[
    CmdHelp {
        name: "new",
        summary: "create a package",
        usage: &["new [--lib] [--bin] <name>"],
        about: "\
Create the directory <name> with rok.rokon and src/main.rok (--bin, the
default), src/lib.rok (--lib) or both, and make it a git repository.",
        examples: &[
            ("rok new hello", "a program: hello/src/main.rok"),
            ("rok new geom --lib", "a library: geom/src/lib.rok"),
        ],
        flags: &[
            ("--bin", "src/main.rok (the default)"),
            ("--lib", "src/lib.rok"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "init",
        summary: "make a directory a package",
        usage: &["init [--lib] [--bin] [dir]", "init --script <file.rok>"],
        about: "\
Make <dir> (default: the current one) a package, as `rok new` does. With
--script, a script's directory becomes a package: its //! ```rokon blocks
become rok.rokon and its files move under src/, the script being
src/main.rok.",
        examples: &[
            ("rok init", "the current directory"),
            ("rok init --script tool.rok", "turn a script into a package"),
        ],
        flags: &[
            ("--bin", "src/main.rok (the default)"),
            ("--lib", "src/lib.rok"),
            ("--script <file.rok>", "from a script"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "add",
        summary: "add a dependency",
        usage: &[
            "add <name>[@<req>] [options]",
            "add <name> --path <dir> [options]",
            "add <name> --git <url> [--rev|--tag|--branch <r>] [options]",
            "add --native <name>[@<req>] [--registry <url>]",
            "add ... <file.rok>",
        ],
        about: "\
Add a dependency to rok.rokon and resolve it: a stone from the index
(default requirement: ^ its newest version), a directory or a git
repository. Naming a script last adds it to the script's //! ```rokon
block instead (made at the top when missing).",
        examples: &[
            ("rok add json", "the newest json stone"),
            ("rok add json@^0.1", "a version requirement"),
            ("rok add geom --path ../geom", "a package beside this one"),
            ("rok add json tool.rok", "into a script's block"),
        ],
        flags: &[
            (
                "--registry <url>",
                "another stone index (default: rokstones.io's;\na mirror of it goes in \
                 $ROK_HOME/sources.rokon)",
            ),
            ("--path <dir>", "a local package"),
            ("--git <url>", "a git repository"),
            ("--rev, --tag, --branch <r>", "which commit of it"),
            ("--native", "a native package (prebuilt C library)"),
            ("--dev", "to dev_deps (tests and benchmarks only)"),
            ("--features <f,..>", "features of the dependency to enable"),
            ("--no-default-features", "not its `default` feature"),
            ("--optional", "enabled by a new feature of its name"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "remove",
        summary: "remove a dependency",
        usage: &["remove [--native] <name> [<file.rok>]"],
        about: "\
Remove a dependency from rok.rokon, or from a script's //! ```rokon block
when a script is named.",
        examples: &[
            ("rok remove json", ""),
            ("rok remove json tool.rok", "from a script's block"),
        ],
        flags: &[("--native", "a native package")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "update",
        summary: "update dependencies in rok.lock",
        usage: &["update [name...]"],
        about: "\
Resolve git dependencies and stones again, all or the named ones, and
rewrite rok.lock.",
        examples: &[("rok update", "everything"), ("rok update json", "one")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "deps",
        summary: "print the resolved dependencies",
        usage: &["deps [--dev] [--format json] [feature options]"],
        about: "\
Print the resolved dependencies as name=lib.rok, each after its own
dependencies and followed by its features (--extern-feature=<name>=<f>).
Native packages print as `# native <name> <version>` and their compiler
arguments. --format json prints one JSON object per line instead: a
package or native record each, then a summary.",
        examples: &[
            ("rok deps", ""),
            ("rok deps --dev", "with the dev dependencies"),
            ("rok deps --format json", "for tools"),
        ],
        flags: &[
            ("--dev", "with dev_deps"),
            ("--format <human|json>", "the output form (default human)"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "search",
        summary: "find stones in the index",
        usage: &["search [--native] <query>"],
        about: "The stones (or native packages) whose name contains <query>.",
        examples: &[("rok search json", "")],
        flags: &[("--native", "native packages")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "install",
        summary: "install an executable or a library",
        usage: &[
            "install <name>[@<req>] [--registry <url>] [options]",
            "install [<name>] --path <dir | file.rok> [options]",
            "install [<name>] --git <url> [--rev|--tag|--branch <r>] [options]",
            "install --native <name>[@<req>]",
            "install --list",
        ],
        about: "\
An executable package (src/main.rok) is built --release into
$ROK_HOME/bin; a library (src/lib.rok only, or with --lib) goes into the
active environment, for `rok repl` and `rok jupyter`; a package with
both needs --bin or --lib to say which.
A script file is
built into $ROK_HOME/bin. An executable already installed from the same
stone version or git commit is kept unless --force; one from a path is
always rebuilt.",
        examples: &[
            ("rok install fmtx", "an executable stone"),
            ("rok install --path .", "the package here"),
            ("rok install json --lib", "a library for the REPL"),
            ("rok install --list", "what is installed"),
        ],
        flags: &[
            ("--registry <url>", "another stone index"),
            ("--path <dir | file.rok>", "a local package or script"),
            ("--git <url>", "a git repository"),
            ("--rev, --tag, --branch <r>", "which commit of it"),
            ("--features <f,..>", "features to enable"),
            ("--no-default-features", "not its `default` feature"),
            ("--lib", "install the library, not the executable"),
            ("--bin", "install the executable, not the library"),
            ("--force", "rebuild even if installed"),
            ("--native", "a native package into the environment"),
            ("--list", "the environment's libraries and the executables"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "uninstall",
        summary: "remove installed executables or libraries",
        usage: &["uninstall [--native] <name>..."],
        about: "\
Remove each from the active environment, or else from $ROK_HOME/bin.",
        examples: &[("rok uninstall fmtx", "")],
        flags: &[("--native", "a native package")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "publish",
        summary: "publish this package to the index",
        usage: &[
            "publish [--dry-run] [--registry <url> | --index-dir <dir>]",
            "publish --native <file> [--dry-run] [--registry <url> | --index-dir <dir>]",
        ],
        about: "\
Make the index entry for this package: its git remote `origin` (an
https:// URL), HEAD commit and tree, dependencies, features, `toolchain`
(the pin, else this rok's series) and `links`; sources are not uploaded.
It is committed to the index checkout <dir>, or without --index-dir sent
to the registry API of rokstones.io or of --registry <url>, with the
manifest's descriptive fields, the README and the packed size (log in
first with `rok login`, or set $ROK_REGISTRY_TOKEN for rokstones.io).
--dry-run only checks it, against <dir> if given.",
        examples: &[
            ("rok publish --dry-run", "check what would be published"),
            ("rok publish", "publish it"),
        ],
        flags: &[
            ("--dry-run", "only check"),
            ("--registry <url>", "another index than rokstones.io's"),
            ("--index-dir <dir>", "a checkout of the index"),
            (
                "--native <file>",
                "the native package `Native { .. }` in <file>",
            ),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "yank",
        summary: "mark a version yanked",
        usage: &[
            "yank [--native] <name>@<version> [--undo] [--message <why>] [--registry <url> | \
             --index-dir <dir>]",
        ],
        about: "\
Mark a version yanked, so new resolutions skip it: committed to the index
checkout <dir>, or through the registry API of rokstones.io or of
--registry <url> (log in with `rok login`), which keeps --message.",
        examples: &[
            ("rok yank num@1.2.0", "yank num 1.2.0"),
            ("rok yank num@1.2.0 --message \"wrong sums\"", "and say why"),
            ("rok yank num@1.2.0 --undo", "take it back"),
        ],
        flags: &[
            ("--undo", "un-yank it"),
            ("--message <why>", "why it is yanked (at most 512 bytes)"),
            ("--registry <url>", "another index than rokstones.io's"),
            ("--index-dir <dir>", "a checkout of the index"),
            ("--native", "a native package"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "login",
        summary: "keep a registry token",
        usage: &["login [--registry <url>] [<token>]"],
        about: "\
Keep a registry token for one index (default: rokstones.io's) in
$ROK_HOME/tokens.rokon, after checking its account may publish; it is
sent only to that index. Get one from <api>/me (e.g.
https://rokstones.io/me); without <token> it is read from standard input.",
        examples: &[
            ("rok login", "paste the token"),
            (
                "rok login --registry https://example.com/index",
                "a token for another index",
            ),
        ],
        flags: &[("--registry <url>", "another index than rokstones.io's")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "logout",
        summary: "remove the registry token",
        usage: &["logout [--registry <url> | --all]"],
        about: "\
Remove the token `rok login` kept for the index (default: rokstones.io's),
or with --all every token.",
        examples: &[
            ("rok logout", ""),
            ("rok logout --all", "every index's token"),
        ],
        flags: &[
            ("--registry <url>", "another index than rokstones.io's"),
            ("--all", "every index"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "env",
        summary: "named environments",
        usage: &[
            "env create|remove|use <name>",
            "env list",
            "env export <name> <dir>",
            "env import <name> <dir | rok.rokon>",
            "env shell [name]",
        ],
        about: "\
An environment holds the libraries `rok install` puts there, for the REPL,
Jupyter and scripts outside a package. The active one is --env, else
$ROK_ENV, else the one `rok env use` chose, else `default`. `list` marks
the active one with `*`; `shell` starts $SHELL with ROK_ENV=<name>.",
        examples: &[
            ("rok env create sci", "a new environment"),
            ("rok env use sci", "make it the active one"),
            (
                "rok env export sci out/",
                "its rok.rokon and rok.lock into out/",
            ),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "toolchain",
        summary: "install and pick toolchains",
        usage: &[
            "toolchain list",
            "toolchain install <version> [--from <dir | .tar[.gz] | .zip | url>]",
            "toolchain default [<version>]",
            "toolchain remove <version>",
            "toolchain clean",
            "toolchain update [<version>]",
        ],
        about: "\
Toolchains live in $ROK_HOME/toolchains/<version>-<triple>/ ($ROK_HOME:
default ~/.rok); the $ROK_HOME/bin proxies run the default one. `update`
installs the newest release, or <version>, from $ROK_DIST (default:
https://github.com/rok-lang/rok/releases), checked against its .sha256,
and makes it the default; it is rolled back if the proxies then do not
run it. <version> is a version (0.1.6), a series (0.1, the newest 0.1.x)
or a requirement (^0.1.5). `install` without --from downloads one and
keeps the default. `clean` removes every toolchain but the default. A
first argument +<version> runs one command with that toolchain, as does
$ROK_TOOLCHAIN, a script's or rok.rokon's `toolchain`.",
        examples: &[
            ("rok toolchain update", "the newest release"),
            ("rok toolchain update 0.1", "the newest 0.1.x"),
            (
                "rok toolchain install 0.1.4",
                "download 0.1.4 beside the default",
            ),
            ("rok toolchain default 0.1.4", "switch"),
            ("rok +0.1.4 build", "one build with 0.1.4"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "jupyter",
        summary: "the Jupyter kernel",
        usage: &[
            "jupyter install [--env <name>]",
            "jupyter --connection-file <file>",
        ],
        about: "\
`install` installs the `rok` Jupyter kernel; with --env it is `rok-<env>`,
running in that environment. Jupyter itself runs the kernel with
--connection-file.",
        examples: &[
            ("rok jupyter install", "the `rok` kernel"),
            ("rok --env sci jupyter install", "the `rok-sci` kernel"),
        ],
        flags: &[("--connection-file <file>", "the file Jupyter wrote")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "repl",
        summary: "start an interactive session",
        usage: &["repl [path] [options]"],
        about: "\
Each cell is compiled and run. In the session, :type <expr> shows a type,
:reset starts over, :exit leaves and :add <dep> adds a dependency (`rok
add` in a package, else `rok install`). A program path runs in the
session first; a library is loaded and `use`d. Outside a package the
session uses the active environment's libraries (see `rok help env`).
Bare `rok` is `rok repl`.",
        examples: &[
            ("rok", "an empty session"),
            ("rok repl src/main.rok", "start from a program's bindings"),
            ("rok --env sci repl", "with the sci environment"),
        ],
        flags: &[("-q, --quiet", "no banner")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "completions",
        summary: "print a shell completion script",
        usage: &["completions bash|zsh|fish|powershell"],
        about: "\
Print the completion script for that shell on standard output. install.sh
and install.ps1 put it in place; `rok toolchain default` and `update`
rewrite it.",
        examples: &[(
            "rok completions fish > ~/.config/fish/completions/rok.fish",
            "",
        )],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "version",
        summary: "print the version",
        usage: &["version [-v] [--format json]"],
        about: "\
Print the version (also `rok --version`); with -v, also where rok and rokc
are, the toolchain and the host, as `key: value` lines. --format json prints
one JSON object instead.",
        examples: &[
            ("rok version -v", ""),
            ("rok version -v --format json", "for tools"),
        ],
        flags: &[("--format <human|json>", "the output form (default human)")],
        ..CmdHelp::EMPTY
    },
];

/// The help of rok command `cmd`: its own, else rokc's.
#[must_use]
pub fn find(cmd: &str) -> Option<&'static CmdHelp> {
    cli::find(ROK_COMMANDS, cmd).or_else(|| {
        FROM_ROKC
            .contains(&cmd)
            .then(|| cli::find(ROKC_COMMANDS, cmd))
            .flatten()
    })
}

/// Every command `rok help` lists, in its order.
pub fn all() -> impl Iterator<Item = &'static CmdHelp> {
    GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .filter_map(|n| find(n))
}

/// `rok help <cmd>`.
#[must_use]
pub fn command(cmd: &str) -> Option<String> {
    let h = find(cmd)?;
    let extra: &[(&str, &[(&str, &str)])] = if WITH_FEATURES.contains(&cmd) {
        &[("feature options (rok.rokon's `features`)", FEATURES)]
    } else {
        &[]
    };
    Some(cli::render_help("rok", h, extra))
}

/// `rok --help`: the commands, one line each, and the global options.
#[must_use]
pub fn short_usage() -> String {
    short_usage_in(term::depth(Stream::Stdout))
}

#[must_use]
pub fn short_usage_in(d: Depth) -> String {
    use std::fmt::Write as _;
    let mut o = format!(
        "{} rok [options] <command> [args]
       rok [options] <file.rok> [args]   run a script
       rok [options]                     start the REPL
",
        cli::heading(d, "usage")
    );
    for (group, names) in GROUPS {
        let rows: Vec<(String, &str)> = names
            .iter()
            .filter_map(|n| find(n))
            .map(|h| (h.name.to_owned(), h.summary))
            .collect();
        let _ = write!(
            o,
            "\n{}\n{}",
            cli::heading(d, group),
            cli::rows_from(d, &rows, 14, 26)
        );
    }
    let rows: Vec<(String, &str)> = GLOBAL
        .iter()
        .map(|(f, w)| ((*f).to_owned(), *w))
        .chain([("+<version>".to_owned(), "(first) use that toolchain")])
        .collect();
    let _ = write!(
        o,
        "\n{}\n{}",
        cli::heading(d, "options (anywhere before `--`)"),
        cli::rows_from(d, &rows, 14, 26)
    );
    o.push_str("\none command's help: `rok help <command>`, e.g. `rok help build`\n");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_command_has_help() {
        for (_, names) in GROUPS {
            for n in *names {
                let h = find(n).unwrap_or_else(|| panic!("no help for {n}"));
                assert_eq!(h.name, *n);
                assert!(h.usage[0].starts_with(n), "{n}");
                assert!(!h.examples.is_empty(), "{n}");
                for (ex, _) in h.examples {
                    assert!(ex.starts_with("rok") || ex.starts_with("{tool}"), "{ex}");
                }
            }
        }
        assert!(find("bindgen").is_none());
    }

    #[test]
    fn short_and_plain() {
        let u = short_usage_in(Depth::Off);
        assert!(u.lines().count() < 60, "{u}");
        assert!(u.contains("\n  build  ") && !u.contains("rokc"), "{u}");
    }
}
