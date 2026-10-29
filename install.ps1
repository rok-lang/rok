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
# releases are downloaded from), ROK_NO_MODIFY_PATH, NO_COLOR. Nothing
# else is needed: rokc links with its own linker and import libraries (no
# MSVC or Windows SDK).
#
# It puts $ROK_HOME\bin first on the user's PATH and writes the PowerShell
# completions to $ROK_HOME\completions\rok.ps1, loaded from the profile.
# -NoModifyPath (or ROK_NO_MODIFY_PATH=1) leaves PATH and the profile
# alone and says what to add.
#
# Output is that of install.sh and `rok toolchain update`: status words
# right-aligned, green on a console unless NO_COLOR is set. -Quiet prints
# errors only, -Verbose adds notes (URLs, paths).
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
    Justification = 'Status lines for a person, colored on a console.')]
param([string]$Archive, [string]$Version, [switch]$NoModifyPath, [switch]$Quiet, [switch]$Verbose)
$ErrorActionPreference = 'Stop'

if (-not $Version) { $Version = if ($env:ROK_VERSION) { $env:ROK_VERSION } else { '0.1.3' } }
$dist = if ($env:ROK_DIST) { $env:ROK_DIST } else { 'https://github.com/rok-lang/rok/releases' }
$home_ = if ($env:ROK_HOME) { $env:ROK_HOME } else { Join-Path $env:USERPROFILE '.rok' }
if ($env:ROK_NO_MODIFY_PATH -and $env:ROK_NO_MODIFY_PATH -ne '0') { $NoModifyPath = $true }
$color = -not $env:NO_COLOR
$quiet_ = [bool]$Quiet
$verbose_ = [bool]$Verbose

# A status line: $Word right-aligned, then $Text.
function Status([string]$Word, [string]$Text) {
    if ($quiet_) { return }
    if ($color) {
        Write-Host ('{0,12}' -f $Word) -ForegroundColor Green -NoNewline
        Write-Host " $Text"
    } else {
        Write-Host ('{0,12} {1}' -f $Word, $Text)
    }
}
function Note([string]$Text) { if ($verbose_) { Write-Host "        note: $Text" } }
function Info([string]$Text) { if (-not $quiet_) { Write-Host $Text } }
function Err([string]$Text) {
    if ($color) {
        Write-Host 'install.ps1: ' -NoNewline
        Write-Host 'error' -ForegroundColor Red -NoNewline
        Write-Host ": $Text"
    } else {
        Write-Host "install.ps1: error: $Text"
    }
}

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

try {
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
                Status 'Downloading' "rok $Version ($triple)"
                Note "from $url"
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
                Note "SHA-256 matches $Archive.sha256"
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
} catch {
    Err $_.Exception.Message
    # Run as a file: a failing exit code; under iex, leave the shell open.
    if ($PSCommandPath) { exit 1 }
    return
}

# $Path as ~\.. when under the user's profile directory, for display only.
function Shown([string]$Path) {
    if ($env:USERPROFILE -and $Path.StartsWith("$env:USERPROFILE\", [StringComparison]::OrdinalIgnoreCase)) {
        return '~' + $Path.Substring($env:USERPROFILE.Length)
    }
    $Path
}
$shown = Shown $home_
Status 'Installed' "rok $full to $shown"
Note "toolchain in $home_\toolchains, proxies in $home_\bin"

# Whether this PowerShell runs profile scripts (the execution policy
# without this process's -ExecutionPolicy).
function ProfilesRun {
    $policy = 'Undefined'
    foreach ($scope in 'MachinePolicy', 'UserPolicy', 'CurrentUser', 'LocalMachine') {
        $p = Get-ExecutionPolicy -Scope $scope
        if ($p -ne 'Undefined') { $policy = $p; break }
    }
    # Undefined: Restricted on Windows PowerShell, RemoteSigned on 7.
    if ($policy -eq 'Undefined') { return $PSVersionTable.PSEdition -eq 'Core' }
    $policy -notin 'Restricted', 'AllSigned'
}

Info ''
$binDir = Join-Path $home_ 'bin'
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = if ($userPath) { $userPath.Split(';') } else { @() }
if ($NoModifyPath) {
    if ($parts -notcontains $binDir) {
        Info "Add $shown\bin to your PATH (then open a new terminal)."
        Info ''
    }
} else {
    $changed = @()
    if ($parts -notcontains $binDir) {
        [Environment]::SetEnvironmentVariable('Path', (@($binDir) + $parts | Where-Object { $_ }) -join ';', 'User')
        $changed += 'the user PATH'
    }
    # The completions of rok and rokc; a toolchain too old for
    # `completions` leaves them out.
    $completions = Join-Path $home_ 'completions\rok.ps1'
    New-Item -ItemType Directory -Force (Split-Path $completions) | Out-Null
    $script = foreach ($t in 'rok', 'rokc') {
        $out = & "$binDir\$t.exe" completions powershell 2>$null
        if (-not $LASTEXITCODE) { $out }
    }
    Set-Content -Path $completions -Value $script -Encoding UTF8
    Note "PowerShell completions in $completions"
    $quoted = $completions.Replace("'", "''")
    $line = "if (Test-Path '$quoted') { . '$quoted' }"
    $userProfile = $PROFILE.CurrentUserAllHosts
    if (-not (ProfilesRun)) {
        Info "PowerShell does not run profile scripts here (execution policy), so for"
        Info "completions add this line to $(Shown $userProfile) once it does:"
        Info "  $line"
        Info ''
    } elseif (-not ((Test-Path $userProfile) -and (Get-Content $userProfile) -contains $line)) {
        New-Item -ItemType Directory -Force (Split-Path $userProfile) | Out-Null
        Add-Content -Path $userProfile -Value $line -Encoding UTF8
        $changed += Shown $userProfile
    }
    if ($changed) {
        Status 'Modified' ($changed -join ', ')
        Info ''
    }
    if ($env:Path.Split(';') -notcontains $binDir) {
        Info 'A new terminal finds rok on PATH; to use it in this one, run:'
        Info "  `$env:Path = `"$binDir;`$env:Path`""
        Info ''
    }
}
Info 'Next steps:'
Info '  rok --version'
Info '  rok new hello; cd hello; rok run'
