# Wrap the bundled folder as an MSIX package and sign it for local testing.
# Run ci\win-bundle.ps1 first. Usage: powershell -ExecutionPolicy Bypass -File ci\win-msix.ps1
param(
    [string]$Config = 'release'
)
$ErrorActionPreference = 'Stop'

$repo     = Split-Path -Parent $PSScriptRoot
$payload  = Join-Path $repo 'dist\scrolex'
$assets   = Join-Path $repo 'resources\windows\assets'
$manifest = Join-Path $repo 'resources\windows\AppxManifest.xml'
if (-not (Test-Path $payload)) { throw "run ci\win-bundle.ps1 first: $payload is missing" }

# The SDK tools are not on PATH. Take the newest kit that has all three.
$kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
$sdk = Get-ChildItem $kits -Directory |
       Where-Object { $_.Name -match '^10\.' } |
       Sort-Object { [version]$_.Name } |
       ForEach-Object { Join-Path $_.FullName 'x64' } |
       Where-Object { Test-Path (Join-Path $_ 'makeappx.exe') } |
       Select-Object -Last 1
if (-not $sdk) { throw "makeappx.exe not found under $kits" }
$makeappx = Join-Path $sdk 'makeappx.exe'
$makepri  = Join-Path $sdk 'makepri.exe'
$signtool = Join-Path $sdk 'signtool.exe'

$version = (Select-String -Path "$repo\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
# MSIX wants four parts and the Store rejects a non-zero revision.
$appxVersion = "$version.0"

$stage = Join-Path $repo 'dist\msix'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item "$payload\*" $stage -Recurse
Copy-Item $assets "$stage\assets" -Recurse

$xml = [xml](Get-Content $manifest)
$xml.Package.Identity.Version = $appxVersion
$xml.Save("$stage\AppxManifest.xml")
$publisher = $xml.Package.Identity.Publisher

# Index only the manifest and the assets. The payload holds no resources, and indexing
# the whole icon theme takes minutes.
$priSrc = Join-Path $repo 'dist\pri'
if (Test-Path $priSrc) { Remove-Item $priSrc -Recurse -Force }
New-Item -ItemType Directory -Force -Path $priSrc | Out-Null
Copy-Item "$stage\AppxManifest.xml" $priSrc
Copy-Item $assets "$priSrc\assets" -Recurse
& $makepri createconfig /cf "$priSrc\priconfig.xml" /dq en-US /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri createconfig failed with $LASTEXITCODE" }
& $makepri new /pr $priSrc /cf "$priSrc\priconfig.xml" /of "$stage\resources.pri" /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri new failed with $LASTEXITCODE" }

$msix = Join-Path $repo "dist\scrolex-$version-x64.msix"
if (Test-Path $msix) { Remove-Item $msix -Force }
& $makeappx pack /d $stage /p $msix /o
if ($LASTEXITCODE -ne 0) { throw "makeappx failed with $LASTEXITCODE" }

# Windows installs a package only when it trusts the signature. A self-signed certificate
# is enough to test. The Store replaces this signature with its own.
$cert = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -eq $publisher -and $_.NotAfter -gt (Get-Date) } |
        Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type Custom -Subject $publisher `
        -KeyUsage DigitalSignature -CertStoreLocation Cert:\CurrentUser\My `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
    Write-Output "created a self-signed certificate: $($cert.Thumbprint)"
}
$cer = Join-Path $repo 'dist\scrolex-test-cert.cer'
Export-Certificate -Cert $cert -FilePath $cer -Force | Out-Null

& $signtool sign /fd SHA256 /sha1 $cert.Thumbprint $msix
if ($LASTEXITCODE -ne 0) { throw "signtool failed with $LASTEXITCODE" }

$mb = [math]::Round(((Get-Item $msix).Length / 1MB), 1)
Write-Output "packed and signed $mb MB -> $msix"
Write-Output "to install, run these two in an elevated powershell:"
Write-Output "  Import-Certificate -FilePath '$cer' -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
Write-Output "  Add-AppxPackage -Path '$msix'"
