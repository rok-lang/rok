//! Shell completion scripts generated from a tool's usage text (what
//! `--help` prints), so the two cannot drift.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The shells a script can be generated for.
pub const SHELLS: [&str; 4] = ["bash", "zsh", "fish", "powershell"];

/// Candidates for a word that the usage text does not list.
#[derive(Clone, Debug)]
pub enum Values {
    /// The lines this command prints, run by the shell when completing.
    Run(String),
    /// The tool's commands.
    Commands,
}

/// What a tool's command line takes.
#[derive(Default, Debug, Clone)]
pub struct Spec {
    pub prog: String,
    /// Commands in usage order, each with a short description.
    pub commands: Vec<(String, String)>,
    /// `cmd` -> its subcommands.
    pub subs: BTreeMap<String, Vec<String>>,
    /// `cmd` or `cmd sub` -> its flags.
    pub flags: BTreeMap<String, Vec<String>>,
    /// Flags taken anywhere.
    pub global: Vec<String>,
    /// Global flags whose value is the next word.
    pub global_values: Vec<String>,
    /// Keyed by `+` (a `+<toolchain>` first word), `cmd` or `cmd sub`
    /// (the word after it) or a flag (its value).
    pub values: Vec<(String, Values)>,
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// The sections of a usage text: a line `name: ..` at column 0 opens
/// one, whose body is the rest of that line and the indented lines after.
fn sections(usage: &str) -> Vec<(&str, Vec<&str>)> {
    let mut out: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut open = false;
    for line in usage.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(' ') {
            if let Some(s) = out.last_mut().filter(|_| open) {
                s.1.push(line);
            }
            continue;
        }
        open = false;
        if let Some((name, rest)) = line.split_once(':') {
            let body = if rest.trim().is_empty() {
                vec![]
            } else {
                vec![rest]
            };
            out.push((name, body));
            open = true;
        }
    }
    out
}

/// An entry of a section body: its head (first line, trimmed) and the
/// text of all its lines.
struct Entry<'a> {
    head: &'a str,
    text: String,
}

/// The entries of a section body, and the lines of prose between them
/// (attached to the entry before). An entry is a line at the body's least
/// indent with a description (two spaces in) or more indented lines
/// after it; prose starts with a capital and runs on at that indent.
fn entries<'a>(body: &[&'a str]) -> Vec<(Entry<'a>, String)> {
    let Some(min) = body.iter().map(|l| indent(l)).min() else {
        return vec![];
    };
    let mut out: Vec<(Entry<'a>, String)> = Vec::new();
    let mut prose = false;
    for (i, l) in body.iter().enumerate() {
        let t = l.trim();
        if indent(l) > min {
            if let Some((e, p)) = out.last_mut() {
                let to = if prose { p } else { &mut e.text };
                to.push(' ');
                to.push_str(t);
            }
            continue;
        }
        let more = body.get(i + 1).is_some_and(|n| indent(n) > min);
        let described = t.contains("  ");
        // A heading inside the section (`feature options:`).
        if t.ends_with(':') && !described && !more {
            prose = false;
            continue;
        }
        prose = t.starts_with(|c: char| c.is_ascii_uppercase()) || prose && !described && !more;
        if prose {
            if let Some((_, p)) = out.last_mut() {
                p.push(' ');
                p.push_str(t);
            }
        } else {
            out.push((
                Entry {
                    head: t,
                    text: t.to_owned(),
                },
                String::new(),
            ));
        }
    }
    out
}

fn is_word(w: &str) -> bool {
    w.starts_with(|c: char| c.is_ascii_lowercase())
        && w.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The commands an entry head names and their subcommands:
/// `cmd [sub|sub..] ..`, `a | b | c ..` or `a, b`.
fn names(head: &str) -> (Vec<&str>, Vec<&str>) {
    let head = head.split("  ").next().unwrap_or("");
    let toks: Vec<&str> = head.split_whitespace().collect();
    let Some(first) = toks.first() else {
        return (vec![], vec![]);
    };
    if toks.get(1) == Some(&"|") {
        let mut cmds = Vec::new();
        let mut i = 0;
        while let Some(t) = toks.get(i).filter(|t| is_word(t)) {
            cmds.push(*t);
            if toks.get(i + 1) != Some(&"|") {
                break;
            }
            i += 2;
        }
        return (cmds, vec![]);
    }
    if first.ends_with(',') {
        let cmds: Vec<&str> = toks.iter().map(|t| t.trim_end_matches(',')).collect();
        return if cmds.iter().all(|w| is_word(w)) {
            (cmds, vec![])
        } else {
            (vec![], vec![])
        };
    }
    let Some(cmds) = alternatives(first) else {
        return (vec![], vec![]);
    };
    (
        cmds,
        toks.get(1)
            .and_then(|t| alternatives(t))
            .unwrap_or_default(),
    )
}

/// The words of `a|b|c`, if each is one.
fn alternatives(t: &str) -> Option<Vec<&str>> {
    let v: Vec<&str> = t.split('|').collect();
    v.iter().all(|w| is_word(w)).then_some(v)
}

/// The flag a word of usage text spells (`--name`, `-x`), if any.
fn flag(word: &str) -> Option<String> {
    let w = word.trim_start_matches(['[', '(']);
    let body = w.strip_prefix("--").or_else(|| w.strip_prefix('-'))?;
    if !body.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let name: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let name = name.trim_end_matches('-');
    Some(format!("{}{name}", &w[..w.len() - body.len()]))
}

/// The flags in usage text, outside `code` spans.
fn flags_in(text: &str) -> Vec<String> {
    let mut plain = String::new();
    for (i, part) in text.split('`').enumerate() {
        if i % 2 == 0 {
            plain.push_str(part);
            plain.push(' ');
        }
    }
    let mut out = Vec::new();
    for w in plain.split(|c: char| c.is_whitespace() || matches!(c, '|' | ';')) {
        if let Some(f) = flag(w.trim_end_matches(',')) {
            push_new(&mut out, f);
        }
    }
    out
}

/// The flags of an option entry's head that take the next word as value:
/// all before the first word that is not a flag, and any `--x=<v>`.
fn value_flags(head: &str) -> Vec<String> {
    let head = head.split("  ").next().unwrap_or("");
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for w in head.split(|c: char| c.is_whitespace() || c == '|') {
        let w = w.trim_end_matches(',');
        if w.is_empty() {
            continue;
        }
        match flag(w) {
            Some(f) if w.contains('=') => push_new(&mut out, f),
            Some(f) => seen.push(f),
            None => {
                for f in seen {
                    push_new(&mut out, f);
                }
                break;
            }
        }
    }
    out
}

fn push_new(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// The flags of section `name` of a usage text.
#[must_use]
pub fn section_flags(usage: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (n, body) in sections(usage) {
        if n == name {
            for l in body {
                for f in flags_in(l) {
                    push_new(&mut out, f);
                }
            }
        }
    }
    out
}

impl Spec {
    /// Read a usage text: commands and their flags from the sections
    /// `commands` accepts, global flags (and commands, `version, help`)
    /// from those `global` accepts.
    #[must_use]
    pub fn parse(
        prog: &str,
        usage: &str,
        commands: impl Fn(&str) -> bool,
        global: impl Fn(&str) -> bool,
    ) -> Self {
        let mut s = Self {
            prog: prog.to_owned(),
            ..Self::default()
        };
        for (name, body) in sections(usage) {
            let is_global = global(name);
            if !is_global && !commands(name) {
                continue;
            }
            // Option entries of a command section apply to its commands.
            let mut shared = Vec::new();
            let mut cmds = Vec::new();
            for (e, prose) in entries(&body) {
                if e.head.starts_with('-') {
                    if is_global {
                        for f in value_flags(e.head) {
                            push_new(&mut s.global_values, f);
                        }
                        for f in flags_in(e.head) {
                            push_new(&mut s.global, f);
                        }
                    } else {
                        shared.extend(flags_in(e.head));
                    }
                    continue;
                }
                let (names, subs) = names(e.head);
                let flags = flags_in(&format!("{} {prose}", e.text));
                let about = describe(&e);
                for c in &names {
                    if !s.commands.iter().any(|(n, _)| n == c) {
                        s.commands.push(((*c).to_owned(), about.clone()));
                    }
                    push_new(&mut cmds, (*c).to_owned());
                    let keys: Vec<String> = if subs.is_empty() {
                        vec![(*c).to_owned()]
                    } else {
                        let list = s.subs.entry((*c).to_owned()).or_default();
                        for sub in &subs {
                            push_new(list, (*sub).to_owned());
                        }
                        subs.iter().map(|sub| format!("{c} {sub}")).collect()
                    };
                    for k in keys {
                        s.add_flags(&k, &flags);
                    }
                }
            }
            for c in cmds {
                s.add_flags(&c, &shared);
            }
        }
        s
    }

    pub fn add_flags(&mut self, key: &str, flags: &[String]) {
        let list = self.flags.entry(key.to_owned()).or_default();
        for f in flags {
            push_new(list, f.clone());
        }
        if list.is_empty() {
            self.flags.remove(key);
        }
    }

    /// The completion script for `shell`, if it is one of `SHELLS`.
    #[must_use]
    pub fn render(&self, shell: &str) -> Option<String> {
        // A global flag is offered once, not again per command.
        let mut s = self.clone();
        for v in s.flags.values_mut() {
            v.retain(|f| !self.global.contains(f));
        }
        s.flags.retain(|_, v| !v.is_empty());
        Some(match shell {
            "bash" => s.bash(),
            "zsh" => s.zsh(),
            "fish" => s.fish(),
            "powershell" => s.powershell(),
            _ => return None,
        })
    }

    fn command_names(&self) -> String {
        let v: Vec<&str> = self.commands.iter().map(|(n, _)| n.as_str()).collect();
        v.join(" ")
    }

    /// `_<prog>_subs`, `_<prog>_flags`, `_<prog>_values`: shared by the
    /// bash and zsh scripts.
    fn sh_tables(&self) -> String {
        let p = &self.prog;
        let mut o = format!("_{p}_subs() {{\n  case $1 in\n");
        for (c, subs) in &self.subs {
            let _ = writeln!(o, "    '{c}') echo '{}' ;;", subs.join(" "));
        }
        let _ = write!(o, "  esac\n}}\n_{p}_flags() {{\n  case $1 in\n");
        for (k, flags) in &self.flags {
            let _ = writeln!(o, "    '{k}') echo '{}' ;;", flags.join(" "));
        }
        let _ = write!(o, "  esac\n}}\n_{p}_values() {{\n  case $1 in\n");
        for (k, v) in &self.values {
            let what = match v {
                Values::Run(cmd) => format!("{cmd} 2>/dev/null"),
                Values::Commands => format!("echo '{}'", self.command_names()),
            };
            let _ = writeln!(o, "    '{k}') {what} ;;");
        }
        o.push_str("  esac\n}\n");
        o
    }

    /// The `case` arm that skips the value of a global flag, if any.
    fn sh_skip(&self, arm: &str) -> String {
        if self.global_values.is_empty() {
            return String::new();
        }
        format!("      {}) {arm} ;;\n", self.global_values.join(" | "))
    }

    fn bash(&self) -> String {
        let p = &self.prog;
        let mut o = format!("# bash completion for {p}: `{p} completions bash`\n");
        o.push_str(&self.sh_tables());
        let _ = write!(
            o,
            "_{p}() {{
  local cur=${{COMP_WORDS[COMP_CWORD]}} prev=${{COMP_WORDS[COMP_CWORD - 1]}}
  local cmd='' sub='' key words w i
  for ((i = 1; i < COMP_CWORD; i++)); do
    w=${{COMP_WORDS[i]}}
    case $w in
{skip}      -* | +*) ;;
      *) if [[ -z $cmd ]]; then cmd=$w; elif [[ -z $sub ]]; then sub=$w; fi ;;
    esac
  done
  COMPREPLY=()
  if [[ $prev == -* ]]; then
    words=$(_{p}_values \"$prev\")
    if [[ -n $words ]]; then
      COMPREPLY=($(compgen -W \"$words\" -- \"$cur\"))
      return
    fi
    case $prev in
{ret}    esac
  fi
  key=$cmd
  [[ -n $sub ]] && key=\"$cmd $sub\"
  if [[ -z $cmd ]]; then
    case $cur in
      +*)
        words=''
        for w in $(_{p}_values +); do words+=\" +$w\"; done
        ;;
      -*) words='{global}' ;;
      *) words='{commands}' ;;
    esac
  elif [[ $cur == -* ]]; then
    words=\"$(_{p}_flags \"$key\") {global}\"
    [[ -n $sub ]] && words+=\" $(_{p}_flags \"$cmd\")\"
  elif [[ -z $sub && -n $(_{p}_subs \"$cmd\") ]]; then
    words=\"$(_{p}_subs \"$cmd\") $(_{p}_values \"$cmd\")\"
  else
    words=$(_{p}_values \"$key\")
  fi
  COMPREPLY=($(compgen -W \"$words\" -- \"$cur\"))
}}
complete -o bashdefault -o default -F _{p} {p}
",
            skip = self.sh_skip("((i++))"),
            ret = self.sh_skip("return"),
            global = self.global.join(" "),
            commands = self.command_names(),
        );
        o
    }

    fn zsh(&self) -> String {
        let p = &self.prog;
        let mut o = format!("#compdef {p}\n# zsh completion for {p}: `{p} completions zsh`\n");
        o.push_str(&self.sh_tables());
        let mut described = String::new();
        for (n, about) in &self.commands {
            let about = about.replace('\'', "'\\''").replace(':', "\\:");
            let _ = write!(described, "\n    '{n}:{about}'");
        }
        let _ = write!(
            o,
            "_{p}() {{
  local cmd='' sub='' key w i
  local -a cand cmds
  for ((i = 2; i < CURRENT; i++)); do
    w=${{words[i]}}
    case $w in
{skip}      -* | +*) ;;
      *) if [[ -z $cmd ]]; then cmd=$w; elif [[ -z $sub ]]; then sub=$w; fi ;;
    esac
  done
  local prev=${{words[CURRENT - 1]}}
  if [[ $prev == -* ]]; then
    cand=($(_{p}_values \"$prev\"))
    if (( $#cand )); then
      compadd -a cand
      return
    fi
    case $prev in
{ret}    esac
  fi
  key=$cmd
  [[ -n $sub ]] && key=\"$cmd $sub\"
  if [[ -z $cmd ]]; then
    case $PREFIX in
      +*)
        cand=($(_{p}_values +))
        compadd -- ${{cand/#/+}}
        ;;
      -*) compadd -- {global} ;;
      *)
        cmds=({described}
        )
        _describe -t commands '{p} command' cmds
        ;;
    esac
  elif [[ $PREFIX == -* ]]; then
    cand=($(_{p}_flags \"$key\") {global})
    [[ -n $sub ]] && cand+=($(_{p}_flags \"$cmd\"))
    compadd -a cand
  elif [[ -z $sub && -n $(_{p}_subs \"$cmd\") ]]; then
    compadd -- $(_{p}_subs \"$cmd\") $(_{p}_values \"$cmd\")
  else
    cand=($(_{p}_values \"$key\"))
    if (( $#cand )); then
      compadd -a cand
    else
      _files
    fi
  fi
}}
if [[ $funcstack[1] == _{p} ]]; then
  _{p} \"$@\"
else
  compdef _{p} {p}
fi
",
            skip = self.sh_skip("((i++))"),
            ret = self.sh_skip("_files; return"),
            global = self.global.join(" "),
        );
        o
    }

    /// A fish `complete` option for flag `f`.
    fn fish_flag(&self, f: &str) -> String {
        let opt = if let Some(l) = f.strip_prefix("--") {
            format!("-l {l}")
        } else if f.len() == 2 {
            format!("-s {}", &f[1..])
        } else {
            format!("-o {}", &f[1..])
        };
        match self.values.iter().find(|(k, _)| k == f) {
            Some((_, v)) => format!("{opt} -x -a '{}'", self.fish_values(v)),
            None if self.global_values.iter().any(|g| g == f) => format!("{opt} -r"),
            None => opt,
        }
    }

    fn fish_values(&self, v: &Values) -> String {
        match v {
            Values::Run(cmd) => format!("({cmd} 2>/dev/null)"),
            Values::Commands => self.command_names(),
        }
    }

    fn fish(&self) -> String {
        let p = &self.prog;
        let skip = if self.global_values.is_empty() {
            String::new()
        } else {
            format!(
                "            case {}\n                set skip 1\n",
                self.global_values.join(" ")
            )
        };
        let mut o = format!(
            "# fish completion for {p}: `{p} completions fish`
# The words before the cursor that are not flags (or their values).
function __{p}_words
    set -l w (commandline -opc)
    set -e w[1]
    set -l skip 0
    for a in $w
        if test $skip = 1
            set skip 0
            continue
        end
        switch $a
{skip}            case '-*' '+*'
            case '*'
                echo $a
        end
    end
end
# The words so far are exactly $argv.
function __{p}_at
    set -l w (__{p}_words)
    test \"$w\" = \"$argv\"
end
# The words so far start with $argv.
function __{p}_in
    set -l w (__{p}_words)
    set -l n (count $argv)
    test (count $w) -ge $n; and test \"$w[1..$n]\" = \"$argv\"
end
complete -c {p} -n __{p}_at -f
"
        );
        if let Some((_, v)) = self.values.iter().find(|(k, _)| k == "+") {
            let run = match v {
                Values::Run(cmd) => format!("{cmd} 2>/dev/null"),
                Values::Commands => format!("string split ' ' '{}'", self.command_names()),
            };
            let _ = write!(
                o,
                "function __{p}_plus
    string match -q -- '+*' (commandline -ct); and {run} | string replace -r '^' '+'
end
complete -c {p} -n __{p}_at -f -a '(__{p}_plus)'
"
            );
        }
        for (n, about) in &self.commands {
            let about = about.replace('\\', "\\\\").replace('\'', "\\'");
            let _ = writeln!(o, "complete -c {p} -n __{p}_at -f -a {n} -d '{about}'");
        }
        for (c, subs) in &self.subs {
            let _ = writeln!(
                o,
                "complete -c {p} -n '__{p}_at {c}' -f -a '{}'",
                subs.join(" ")
            );
        }
        for (k, v) in &self.values {
            if k == "+" || k.starts_with('-') {
                continue;
            }
            let _ = writeln!(
                o,
                "complete -c {p} -n '__{p}_at {k}' -f -a '{}'",
                self.fish_values(v)
            );
        }
        for (k, flags) in &self.flags {
            for f in flags {
                let _ = writeln!(o, "complete -c {p} -n '__{p}_in {k}' {}", self.fish_flag(f));
            }
        }
        for f in &self.global {
            let _ = writeln!(o, "complete -c {p} {}", self.fish_flag(f));
        }
        o
    }

    fn powershell(&self) -> String {
        let p = &self.prog;
        let list = |v: &[String]| -> String {
            let q: Vec<String> = v.iter().map(|s| format!("'{s}'")).collect();
            format!("@({})", q.join(", "))
        };
        let names: Vec<String> = self.commands.iter().map(|(n, _)| n.clone()).collect();
        let table = |m: &BTreeMap<String, Vec<String>>| -> String {
            let rows: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("        '{k}' = {}", list(v)))
                .collect();
            format!("@{{\n{}\n    }}", rows.join("\n"))
        };
        let mut values = String::new();
        for (k, v) in &self.values {
            let what = match v {
                Values::Run(cmd) => format!("{cmd} 2>$null"),
                Values::Commands => "$commands".to_owned(),
            };
            let _ = write!(values, "\n            '{k}' {{ {what} }}");
        }
        format!(
            "# PowerShell completion for {p}: `{p} completions powershell`
Register-ArgumentCompleter -Native -CommandName {p}, {p}.exe -ScriptBlock {{
    param($wordToComplete, $commandAst, $cursorPosition)
    $commands = {commands}
    $globalFlags = {global}
    $valueFlags = {skip}
    $subs = {subs}
    $flags = {flags}
    $values = {{
        param($key)
        switch ($key) {{{values}
        }}
    }}
    $words = @($commandAst.CommandElements | Select-Object -Skip 1 | ForEach-Object {{ $_.ToString() }})
    if ($wordToComplete -and $words.Count) {{ $words = @($words | Select-Object -First ($words.Count - 1)) }}
    $cmd = ''; $sub = ''; $skipNext = $false
    foreach ($w in $words) {{
        if ($skipNext) {{ $skipNext = $false; continue }}
        if ($valueFlags -contains $w) {{ $skipNext = $true; continue }}
        if ($w -like '-*' -or $w -like '+*') {{ continue }}
        if (-not $cmd) {{ $cmd = $w }} elseif (-not $sub) {{ $sub = $w }}
    }}
    $prev = if ($words.Count) {{ $words[-1] }} else {{ '' }}
    $key = if ($sub) {{ \"$cmd $sub\" }} else {{ $cmd }}
    $cand = @()
    if ($prev -like '-*') {{
        $cand = @(& $values $prev)
        if (-not $cand.Count -and $valueFlags -contains $prev) {{ return }}
    }}
    if (-not $cand.Count) {{
        if (-not $cmd) {{
            if ($wordToComplete -like '+*') {{ $cand = @(& $values '+' | ForEach-Object {{ \"+$_\" }}) }}
            elseif ($wordToComplete -like '-*') {{ $cand = $globalFlags }}
            else {{ $cand = $commands }}
        }} elseif ($wordToComplete -like '-*') {{
            $cand = @($flags[$key]) + $globalFlags
            if ($sub) {{ $cand += @($flags[$cmd]) }}
        }} elseif (-not $sub -and $subs.ContainsKey($cmd)) {{
            $cand = @($subs[$cmd]) + @(& $values $cmd)
        }} else {{
            $cand = @(& $values $key)
        }}
    }}
    $cand | Where-Object {{ $_ -and $_ -like \"$wordToComplete*\" }} | ForEach-Object {{
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }}
}}
",
            commands = list(&names),
            global = list(&self.global),
            skip = list(&self.global_values),
            subs = table(&self.subs),
            flags = table(&self.flags),
        )
    }
}

/// A command's short description: its entry's description up to the
/// first `:`, `;` or `(`, at most about 50 characters.
fn describe(e: &Entry<'_>) -> String {
    let after = e
        .head
        .split_once("  ")
        .map(|(_, d)| d.trim())
        .filter(|d| !d.is_empty());
    let d = after.unwrap_or_else(|| e.text[e.head.len()..].trim());
    let d = d.split([':', ';', '(']).next().unwrap_or("");
    let mut words = String::new();
    for w in d.split_whitespace() {
        if words.len() + w.len() > 50 {
            break;
        }
        if !words.is_empty() {
            words.push(' ');
        }
        words.push_str(w);
    }
    words
        .trim_end_matches([',', ';', ':', '('])
        .replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    const USAGE: &str = "\
usage: t [options] <command>

things:
  new [--lib] <name>   make one
  add <name> [--registry <url>]
                       add one (--dev: as a dev one)
  build | check [path] ...
  An aside that runs
  on into another line with --keep.
  env create|remove <name>
                       an environment
  tc install <v> --from <dir>
                       install one

  feature options:
  --features <f>       the features

options:
  --env <name>    an environment
  -q, --quiet     less output
  version, help

environment: X
";

    fn spec() -> Spec {
        Spec::parse("t", USAGE, |n| n == "things", |n| n.starts_with("options"))
    }

    #[test]
    fn reads_commands_and_flags() {
        let s = spec();
        let names: Vec<&str> = s.commands.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "new", "add", "build", "check", "env", "tc", "version", "help"
            ]
        );
        assert_eq!(s.commands[0].1, "make one");
        assert_eq!(s.commands[1].1, "add one");
        assert_eq!(s.subs["env"], ["create", "remove"]);
        assert_eq!(s.subs["tc"], ["install"]);
        assert_eq!(s.flags["new"], ["--lib", "--features"]);
        assert_eq!(s.flags["add"], ["--registry", "--dev", "--features"]);
        assert_eq!(s.flags["build"], ["--keep", "--features"]);
        assert!(!s.flags.contains_key("feature"));
        assert_eq!(s.flags["tc install"], ["--from"]);
        assert!(!s.flags.contains_key("env create"));
        assert_eq!(s.global, ["--env", "-q", "--quiet"]);
        assert_eq!(s.global_values, ["--env"]);
    }

    #[test]
    fn flag_words() {
        assert_eq!(flag("[--lib]").as_deref(), Some("--lib"));
        assert_eq!(flag("--emit=<kind>[,<kind>...]").as_deref(), Some("--emit"));
        assert_eq!(flag("-O2+").as_deref(), Some("-O2"));
        assert_eq!(flag("--"), None);
        assert_eq!(flag("-"), None);
        assert_eq!(flags_in("x `--not` --yes"), ["--yes"]);
        assert_eq!(value_flags("-A|-W|-D <lint|group>  x"), ["-A", "-W", "-D"]);
        assert_eq!(value_flags("--color=auto|always  x"), ["--color"]);
        assert!(value_flags("-q, --quiet     less").is_empty());
    }

    #[test]
    fn renders_every_shell() {
        let s = spec();
        for sh in SHELLS {
            let out = s.render(sh).unwrap();
            assert!(out.contains("tc"), "{sh}");
        }
        assert!(s.render("csh").is_none());
    }
}
