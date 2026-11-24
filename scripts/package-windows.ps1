# Paquet Windows à distribuer : installeur + version portable, avec ASIO (Dante VSC) et WASAPI.
#
#   powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1
#
# Résultat dans dist\ :
#   TracksPlus-Setup-<version>.exe          installeur (menu Démarrer, désinstallation…)
#   TracksPlus-<version>-windows-portable.zip   exécutable seul, sans installation
#
# Prérequis (une fois) : Rust (MSVC), Visual Studio Build Tools (C++), LLVM pour libclang
# (winget install LLVM.LLVM) et Inno Setup 6 (winget install JRSoftware.InnoSetup).
# Le SDK ASIO de Steinberg est téléchargé automatiquement dans target\asio-sdk s'il manque.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "== TracksPlus $version"

# --- SDK ASIO
$sdk = if ($env:CPAL_ASIO_DIR) { $env:CPAL_ASIO_DIR } else { Join-Path $root 'target\asio-sdk' }
if (-not (Test-Path (Join-Path $sdk 'host\asiodrivers.h'))) {
    Write-Host "== Téléchargement du SDK ASIO (steinberg.net)"
    $tmp = Join-Path $root 'target\asio-sdk-download'
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $tmp | Out-Null
    Invoke-WebRequest -Uri 'https://www.steinberg.net/asiosdk' -OutFile (Join-Path $tmp 'asiosdk.zip')
    Expand-Archive (Join-Path $tmp 'asiosdk.zip') -DestinationPath (Join-Path $tmp 'x') -Force
    $header = Get-ChildItem (Join-Path $tmp 'x') -Recurse -Filter asiodrivers.h | Select-Object -First 1
    if (-not $header) { throw "asiodrivers.h introuvable dans le SDK ASIO" }
    Remove-Item -Recurse -Force $sdk -ErrorAction SilentlyContinue
    Move-Item $header.Directory.Parent.FullName $sdk
    Remove-Item -Recurse -Force $tmp
}
$env:CPAL_ASIO_DIR = $sdk

# --- libclang (génération des liaisons ASIO)
if (-not $env:LIBCLANG_PATH) {
    $llvm = Join-Path $env:ProgramFiles 'LLVM\bin'
    if (-not (Test-Path (Join-Path $llvm 'libclang.dll'))) { throw "libclang introuvable : winget install LLVM.LLVM" }
    $env:LIBCLANG_PATH = $llvm
}

# --- Compilation
Write-Host "== Compilation (ASIO + WASAPI)"
cargo build --release --features asio
if ($LASTEXITCODE -ne 0) { throw "La compilation a échoué" }
$exe = Join-Path $root 'target\release\tracksplus.exe'

# L'exécutable doit démarrer sur un PC vierge : aucune dépendance au Visual C++ Redistributable.
$bytes = [System.IO.File]::ReadAllBytes($exe)
$text = [System.Text.Encoding]::ASCII.GetString($bytes)
foreach ($dll in 'vcruntime140.dll', 'msvcp140.dll') {
    if ($text.IndexOf($dll, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "L'exécutable dépend de $dll (runtime non statique)" }
}
# Clé de registre des pilotes ASIO : présente seulement si le SDK ASIO est compilé dedans.
if ($text.IndexOf('software\asio', [StringComparison]::OrdinalIgnoreCase) -lt 0) {
    throw "ASIO absent de l'exécutable"
}

# --- dist\
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item $exe (Join-Path $dist 'TracksPlus.exe') -Force

Write-Host "== Version portable"
$stage = Join-Path $root "target\portable\TracksPlus"
Remove-Item -Recurse -Force (Split-Path $stage) -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $stage 'Licences') | Out-Null
Copy-Item $exe (Join-Path $stage 'TracksPlus.exe')
Copy-Item 'packaging\windows\LISEZMOI.txt' $stage
Copy-Item 'assets\fonts\Inter-LICENSE.txt' (Join-Path $stage 'Licences\Inter (police) - OFL.txt')
$zip = Join-Path $dist "TracksPlus-$version-windows-portable.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path $stage -DestinationPath $zip

Write-Host "== Installeur"
$iscc = @(
    (Get-Command iscc -ErrorAction SilentlyContinue).Source,
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 introuvable : winget install JRSoftware.InnoSetup" }
& $iscc /Q "/DAppVersion=$version" "/DExe=$exe" 'packaging\windows\TracksPlus.iss'
if ($LASTEXITCODE -ne 0) { throw "La création de l'installeur a échoué" }

Write-Host ""
Write-Host "OK :"
Get-ChildItem $dist -Filter "TracksPlus*$version*" | ForEach-Object { "  dist\{0}  ({1:N1} Mo)" -f $_.Name, ($_.Length / 1MB) }
