# Assemble a self-contained scrolex folder that runs where GTK is not installed.
# Usage: powershell -ExecutionPolicy Bypass -File ci\win-bundle.ps1 [-Config release]
param(
    [string]$GtkRoot = 'C:\gtk',
    [string]$Config  = 'release'
)
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $repo "target\$Config\scrolex.exe"
if (-not (Test-Path $exe)) { throw "build it first: $exe is missing" }

# -products is required or vswhere skips a Build Tools install.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products '*' -property installationPath
$dumpbin = Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" |
           Sort-Object FullName | Select-Object -Last 1
if (-not $dumpbin) { throw 'dumpbin not found' }

$pool = @{}
Get-ChildItem "$GtkRoot\bin\*.dll" | ForEach-Object { $pool[$_.Name.ToLower()] = $_.FullName }

# Walk the import tables. Anything outside the gvsbuild pool is a system dll and stays behind.
$seen  = @{}
$queue = New-Object System.Collections.Queue
$queue.Enqueue($exe)

# The svg loader is opened at runtime, so no import table ever names it.
$svgLoader = "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\pixbufloader_svg.dll"
if (Test-Path $svgLoader) { $queue.Enqueue($svgLoader) }

while ($queue.Count -gt 0) {
    $bin = $queue.Dequeue()
    foreach ($line in (& $dumpbin /dependents $bin 2>$null)) {
        if ($line -match '^\s{4}(\S+\.dll)\s*$') {
            $name = $matches[1].ToLower()
            if ($pool.ContainsKey($name) -and -not $seen.ContainsKey($name)) {
                $seen[$name] = $pool[$name]
                $queue.Enqueue($pool[$name])
            }
        }
    }
}

$dist = Join-Path $repo 'dist\scrolex'
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory -Force -Path $dist | Out-Null

Copy-Item $exe $dist
$seen.Values | ForEach-Object { Copy-Item $_ $dist }

# GTK derives its data prefix from the directory holding the dlls, so share\ and lib\ sit beside them.
New-Item -ItemType Directory -Force -Path "$dist\share\glib-2.0\schemas" | Out-Null
Copy-Item "$GtkRoot\share\glib-2.0\schemas\gschemas.compiled" "$dist\share\glib-2.0\schemas"
New-Item -ItemType Directory -Force -Path "$dist\share\icons" | Out-Null
Copy-Item "$GtkRoot\share\icons\Adwaita" "$dist\share\icons" -Recurse
Copy-Item "$GtkRoot\share\icons\hicolor" "$dist\share\icons" -Recurse
# loaders.cache stores relative paths, so this tree relocates as-is.
New-Item -ItemType Directory -Force -Path "$dist\lib" | Out-Null
Copy-Item "$GtkRoot\lib\gdk-pixbuf-2.0" "$dist\lib" -Recurse

foreach ($doc in 'LICENSE', 'README.md', 'THIRD_PARTY_LICENSES.md') {
    if (Test-Path "$repo\$doc") { Copy-Item "$repo\$doc" $dist }
}
if (Test-Path "$repo\licenses") { Copy-Item "$repo\licenses" $dist -Recurse }

$mb = [math]::Round(((Get-ChildItem $dist -Recurse -File | Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Output "bundled $($seen.Count) dlls, $mb MB -> $dist"

$version = (Select-String -Path "$repo\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$zip = Join-Path $repo "dist\scrolex-$version-x86_64-windows.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
# 7z, not Compress-Archive: that one writes backslash separators and broken directory
# permissions, which other unzip tools reject.
$sevenZip = @(
    "$env:ProgramFiles\7-Zip\7z.exe",
    "${env:ProgramFiles(x86)}\7-Zip\7z.exe",
    'C:\Program Files\7-Zip\7z.exe'
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $sevenZip) { $sevenZip = (Get-Command 7z.exe -ErrorAction SilentlyContinue).Source }
if (-not $sevenZip) { throw '7z.exe not found; install 7-Zip or add it to PATH' }
& $sevenZip a -tzip -mx=9 -bso0 -bsp0 $zip $dist | Out-Null
if ($LASTEXITCODE -ne 0) { throw "7z failed with $LASTEXITCODE" }
$zmb = [math]::Round(((Get-Item $zip).Length / 1MB), 1)
Write-Output "zipped $zmb MB -> $zip"
