# Update a 0.1.x install to a staged release set on
# Windows: the archives dist-native.sh left in -Dist, served over http
# in the GitHub releases' layout. The counterpart of test-update.sh; three
# updates, each on a fresh ROK_HOME holding the 0.1.x release:
#
#   - its `rok toolchain update`, then the staged rok builds and runs a
#     project the 0.1.x rok made and locked (a git dependency), and
#     leaves its rok.lock in lock format 2;
#   - its install.ps1 -Version <staged> from the staged set;
#   - the staged archive's own install.ps1 -Version <staged>.
#
#   pwsh scripts/test-update.ps1 -Dist <dist>
#
# ROK_UPDATE_FROM names the 0.1.x release (default 0.1.6), downloaded
# from GitHub; ROK_UPDATE_PORT the local port (default 8765).
param([Parameter(Mandatory)][string]$Dist)
$ErrorActionPreference = 'Stop'

$Dist = (Resolve-Path $Dist).Path
$v = (Get-Content -Raw (Join-Path $Dist 'rok-version')).Trim()
$from = if ($env:ROK_UPDATE_FROM) { $env:ROK_UPDATE_FROM } else { '0.1.6' }
$port = if ($env:ROK_UPDATE_PORT) { $env:ROK_UPDATE_PORT } else { '8765' }
$gh = 'https://github.com/rok-lang/rok'
$triple = 'x86_64-pc-windows-msvc'
$name = "rok-$v-$triple"
if (-not (Test-Path (Join-Path $Dist "$name.zip"))) { throw "no $name.zip in $Dist" }
# Before the version bump the staged set is the newest release itself,
# which `rok toolchain update` takes as up to date.
if ($v -eq $from) {
    $from = '0.1.5'
    Write-Output "test-update.ps1: the staged set is $v, the release to update from; updating from $from"
}
$p = $v.Split('.')
$series = if ($p[0] -eq '0') { "0.$($p[1])" } else { $p[0] }

function Step([string]$Text) { Write-Output "`n== $Text" }
# Run a native command; fail on a non-zero exit.
function Must {
    $exe = $args[0]
    $rest = @($args | Select-Object -Skip 1)
    & $exe @rest
    if ($LASTEXITCODE) { throw "$args failed ($LASTEXITCODE)" }
}
# Run install script $Script with $Params; fail when it does.
function Install([string]$Script, [hashtable]$Params) {
    $global:LASTEXITCODE = 0
    & $Script @Params -NoModifyPath -Quiet
    if ($LASTEXITCODE) { throw "$Script failed ($LASTEXITCODE)" }
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) "rok-test-update-$([Guid]::NewGuid())"
New-Item -ItemType Directory $tmp | Out-Null
$env:ROK_NO_MODIFY_PATH = '1'
$env:NO_COLOR = '1'
Remove-Item Env:ROK_TOOLCHAIN -ErrorAction SilentlyContinue
$server = $null
try {
    Step "rok $from ($triple) from $gh"
    $base = Join-Path $tmp 'base'
    New-Item -ItemType Directory $base | Out-Null
    $oldPs1 = Join-Path $base 'install.ps1'
    Invoke-WebRequest -UseBasicParsing -OutFile $oldPs1 "https://raw.githubusercontent.com/rok-lang/rok/v$from/install.ps1"
    $oldZip = Join-Path $base "rok-$from-$triple.zip"
    Invoke-WebRequest -UseBasicParsing -OutFile $oldZip "$gh/releases/download/v$from/rok-$from-$triple.zip"

    # The proxies in $RokHome run rok $Want by default, and the list has it.
    function CheckDefault([string]$RokHome, [string]$Want) {
        $env:ROK_HOME = $RokHome
        Push-Location $tmp
        try {
            $got = (& "$RokHome\bin\rokc.exe" --version | Select-Object -First 1)
            if ($got -ne "rokc $Want" -and -not $got.StartsWith("rokc $Want ")) {
                throw "$RokHome\bin\rokc.exe --version says '$got', not rokc $Want"
            }
            $list = (& "$RokHome\bin\rok.exe" toolchain list) -join "`n"
            if (-not $list.Contains("$Want-$triple (default)")) { throw "${RokHome}: $Want-$triple is not the default: $list" }
        } finally { Pop-Location }
    }
    # A ROK_HOME with rok $from as the default, installed by its install.ps1.
    function InstallFrom([string]$RokHome) {
        $env:ROK_HOME = $RokHome
        Install $oldPs1 @{ Archive = $oldZip }
        CheckDefault $RokHome $from
    }

    Step "staged rok $v on http://127.0.0.1:$port"
    $serve = Join-Path $tmp 'serve'
    $rel = Join-Path $serve "download\v$v"
    New-Item -ItemType Directory $rel, (Join-Path $serve 'latest\download') | Out-Null
    Copy-Item (Join-Path $Dist "$name.zip"), (Join-Path $Dist "$name.zip.sha256") $rel
    Copy-Item (Join-Path $Dist 'rok-version') (Join-Path $serve 'latest\download')
    $server = Start-Process python -PassThru -NoNewWindow -ArgumentList '-m', 'http.server', $port, '--bind', '127.0.0.1', '--directory', $serve
    $url = "http://127.0.0.1:$port"
    for ($i = 0; ; $i++) {
        try { Invoke-WebRequest -UseBasicParsing "$url/latest/download/rok-version" | Out-Null; break }
        catch { if ($i -ge 50) { throw "the local server does not answer: $_" }; Start-Sleep -Milliseconds 200 }
    }

    Step "rok ${from}: rok toolchain update"
    $h = Join-Path $tmp 'update'
    InstallFrom $h
    # A project rok $from made and locked: a git dependency.
    $dep = Join-Path $tmp 'greet'
    New-Item -ItemType Directory "$dep\src" | Out-Null
    Set-Content "$dep\src\lib.rok" "pub fn greet() -> i32 {`n    return 1;`n}"
    Must git -C $dep init -q
    Must git -C $dep add .
    Must git -C $dep -c user.name=rok -c user.email=rok@localhost commit -qm greet
    Push-Location $tmp
    Must "$h\bin\rok.exe" -q new app
    Pop-Location
    $app = Join-Path $tmp 'app'
    Set-Content "$app\src\main.rok" "use greet::greet;`nprintln(f`"{greet()}`");"
    Push-Location $app
    try {
        Must "$h\bin\rok.exe" -q add greet --git $dep
        Must "$h\bin\rok.exe" -q build
        if (-not (Test-Path rok.lock)) { throw "rok $from wrote no rok.lock" }

        $env:ROK_DIST = $url
        Must "$h\bin\rok.exe" toolchain update
        Remove-Item Env:ROK_DIST
        CheckDefault $h $v
        $env:ROK_HOME = $h
        if (-not ((& "$h\bin\rok.exe" toolchain list) -join "`n").Contains("$from-$triple")) {
            throw "rok $from is no longer installed"
        }

        # The project moves to the staged series and builds as it was locked.
        # [IO.File] resolves a relative path against the process, not the
        # location: name the file in full.
        $manifest = Join-Path $app 'rok.rokon'
        $m = [IO.File]::ReadAllText($manifest) -replace 'toolchain: "[^"]*"', "toolchain: `"$series`""
        [IO.File]::WriteAllText($manifest, $m)
        Must "$h\bin\rok.exe" build
        $out = (& "$h\bin\rok.exe" -q run) -join "`n"
        if ($LASTEXITCODE -or $out -ne '1') { throw "the project prints '$out', not 1" }
        if (-not (Test-Path rok.lock)) { throw "rok $v removed rok.lock" }
        $lock = Get-Content -Raw rok.lock
        if ($lock -notmatch '(?m)^Lock \{\s*version: 2,') { throw "rok $v did not write rok.lock in lock format 2:`n$lock" }
        if ($lock -match 'vers:') { throw "rok $v left a 0.1 field (vers) in rok.lock:`n$lock" }
    } finally { Pop-Location }

    Step "rok ${from}: its install.ps1 -Version $v"
    $h = Join-Path $tmp 'install-old'
    InstallFrom $h
    $env:ROK_DIST = $url
    Install $oldPs1 @{ Version = $v }
    Remove-Item Env:ROK_DIST
    CheckDefault $h $v

    Step "rok ${from}: $name\install.ps1 -Version $v"
    $h = Join-Path $tmp 'install-new'
    InstallFrom $h
    $unpacked = Join-Path $tmp 'unpacked'
    Expand-Archive (Join-Path $Dist "$name.zip") $unpacked
    $env:ROK_DIST = $url
    $env:ROK_HOME = $h
    # Copied out of the archive, so it downloads instead of installing
    # the toolchain around it.
    Copy-Item "$unpacked\$name\install.ps1" (Join-Path $tmp 'install.ps1')
    Install (Join-Path $tmp "install.ps1") @{ Version = $v }
    Remove-Item Env:ROK_DIST
    CheckDefault $h $v

    Write-Output "`ntest-update.ps1: rok $from updates to $v ($triple)"
} finally {
    if ($server) { Stop-Process -Id $server.Id -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
