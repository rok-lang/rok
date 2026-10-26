# Install a rok toolchain into $env:ROK_HOME (default %USERPROFILE%\.rok,
# spec 9.2.19) on Windows: $ROK_HOME\toolchains\<version>-<triple>\,
# made the default, with the proxies $ROK_HOME\bin\{rok,rokc,rokls}.exe,
# and $ROK_HOME\bin put on the user's PATH. The counterpart of install.sh.
#
#   powershell -ExecutionPolicy Bypass -c "irm https://rok-lang.org/install.ps1 | iex"
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Archive rok-<version>-<triple>.zip
#   rok-<version>-<triple>\install.ps1          (from an unpacked archive)
#
# Environment: ROK_HOME, ROK_VERSION (default below), ROK_DIST (where
# releases are downloaded from). Nothing else is needed: rokc links with
# its own linker and import libraries (no MSVC or Windows SDK).
# -NoModifyPath leaves the user's PATH alone and says what to add.
param([string]$Archive, [string]$Version, [switch]$NoModifyPath)
$ErrorActionPreference = 'Stop'

if (-not $Version) { $Version = if ($env:ROK_VERSION) { $env:ROK_VERSION } else { '0.1.1' } }
$dist = if ($env:ROK_DIST) { $env:ROK_DIST } else { 'https://github.com/rok-lang/rok/releases' }
$home_ = if ($env:ROK_HOME) { $env:ROK_HOME } else { Join-Path $env:USERPROFILE '.rok' }

function HostTriple {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { 'x86_64-pc-windows-msvc' }
        'ARM64' { 'aarch64-pc-windows-msvc' }
        default { throw "no rok toolchain for the $env:PROCESSOR_ARCHITECTURE architecture" }
    }
}

$supported = 'aarch64-apple-darwin', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-pc-windows-msvc', 'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl'

# Download $Uri to $OutFile: $false on a 404, an error on anything else.
function Download([string]$Uri, [string]$OutFile) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $OutFile
        return $true
    } catch {
        $r = $_.Exception.Response
        if ($r -and [int]$r.StatusCode -eq 404) { return $false }
        if ($r) { throw "cannot download ${Uri}: HTTP $([int]$r.StatusCode)" }
        throw "cannot download $Uri (no network, or the host is unreachable?): $($_.Exception.Message)"
    }
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) "rok-install-$([Guid]::NewGuid())"
New-Item -ItemType Directory $tmp | Out-Null
try {
    # An unpacked toolchain around this script installs itself.
    $here = if ($PSScriptRoot) { $PSScriptRoot } else { '' }
    if (-not $Archive -and $here -and (Test-Path "$here\bin\rok.exe") -and (Test-Path "$here\bin\rokc.exe")) {
        $src = $here
    } else {
        if (-not $Archive) {
            $triple = HostTriple
            $url = "$dist/download/v$Version/rok-$Version-$triple.zip"
            Write-Output "downloading $url"
            $Archive = Join-Path $tmp 'rok.zip'
            if (-not (Download $url $Archive)) {
                # No such host, or no such version.
                if ($supported -contains $triple) {
                    throw "rok $Version has no release archive at $url; check the version (-Version, ROK_VERSION)"
                }
                throw ("there is no rok $Version release archive for $triple`n" +
                    "  release archives are built for: $($supported -join ' ')`n" +
                    "  on this host, build rok from source: see `"Building from source`" in`n" +
                    "  https://github.com/rok-lang/rok#building-from-source")
            }
            if (-not (Download "$url.sha256" "$Archive.sha256")) {
                throw "$url.sha256 is missing (HTTP 404), so the archive cannot be checked; report it at https://github.com/rok-lang/rok/issues"
            }
        }
        if (-not (Test-Path $Archive)) { throw "$Archive does not exist" }
        # A download is checked against its release's .sha256, an archive
        # file against the one beside it, if any.
        if (Test-Path "$Archive.sha256") {
            $want = ((Get-Content -Raw "$Archive.sha256").Trim() -split '\s+')[0].ToLower()
            $got = (Get-FileHash -Algorithm SHA256 $Archive).Hash.ToLower()
            if ($got -ne $want) {
                throw "$Archive has SHA-256 $got, but its .sha256 says $want (a damaged or altered download)"
            }
        }
        Expand-Archive -Path $Archive -DestinationPath "$tmp\files"
        $bin = Get-ChildItem -Directory "$tmp\files\*\bin" | Select-Object -First 1
        if (-not $bin) { throw "$Archive is not a rok toolchain (no bin\)" }
        $src = $bin.Parent.FullName
    }

    $full = (& "$src\bin\rokc.exe" --version).Split(' ')[1]
    if ($LASTEXITCODE) { throw "cannot run $src\bin\rokc.exe" }
    # The toolchain's own rok installs it (and checks it).
    $env:ROK_HOME = $home_
    & "$src\bin\rok.exe" -q toolchain install $full --from $src
    if ($LASTEXITCODE) { throw 'rok toolchain install failed' }
    & "$src\bin\rok.exe" -q toolchain default $full
    if ($LASTEXITCODE) { throw 'rok toolchain default failed' }
    & "$home_\bin\rok.exe" --version | Out-Null
    if ($LASTEXITCODE) { throw "the installed proxy $home_\bin\rok.exe does not run" }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Output ''
Write-Output "rok $full is installed in $home_"
$binDir = Join-Path $home_ 'bin'
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = if ($userPath) { $userPath.Split(';') } else { @() }
if ($NoModifyPath) {
    if ($parts -notcontains $binDir) { Write-Output "Add $binDir to your PATH to use it." }
} elseif ($parts -notcontains $binDir) {
    [Environment]::SetEnvironmentVariable('Path', (@($binDir) + $parts | Where-Object { $_ }) -join ';', 'User')
    Write-Output "Added $binDir to your PATH (new terminals see it)."
}
