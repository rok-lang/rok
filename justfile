# Development tasks. Run `just` to list them.
# Needs: shfmt, shellcheck, pwsh + PSScriptAnalyzer (shell/PowerShell),
# and the Rust toolchain (rustfmt, clippy).

_default:
    @just --list

# Format Rust and shell in place.
fmt: fmt-rust fmt-sh

fmt-rust:
    cargo fmt --all

# shfmt reads indent width and options from .editorconfig.
fmt-sh:
    find . -name '*.sh' -not -path './target/*' -exec shfmt -w {} +

# Fail if anything is unformatted (for CI).
fmt-check:
    cargo fmt --all --check
    find . -name '*.sh' -not -path './target/*' -exec shfmt -d {} +

# The rok standard library, formatted by rokc itself (needs a built rokc).
fmt-rok:
    "${CARGO_TARGET_DIR:-target}/debug/rokc" fmt library

# Lint everything.
lint: lint-sh lint-ps clippy

lint-sh:
    find . -name '*.sh' -not -path './target/*' -exec shellcheck --severity=warning {} +

# PSScriptAnalyzer over the .ps1 scripts; exits non-zero on any finding.
lint-ps:
    #!/usr/bin/env pwsh
    $found = Get-ChildItem -Recurse -Filter *.ps1 |
        Where-Object { $_.FullName -notmatch '[/\\]target[/\\]' } |
        ForEach-Object { Invoke-ScriptAnalyzer -Path $_.FullName -Settings ./PSScriptAnalyzerSettings.psd1 }
    $found | Format-Table -AutoSize
    if ($found) { exit 1 }

clippy:
    cargo clippy --all-targets -- -D warnings

# Fail if the release version disagrees across Cargo.toml and the install scripts.
version-check:
    ./check-version.sh
