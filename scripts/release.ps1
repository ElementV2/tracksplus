# Publie une version sur GitHub (Releases) : c'est là que l'application cherche ses mises à jour.
#
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Notes notes.md
#
# Étapes : vérifie que tout est commité et poussé, construit l'installeur et la version
# portable (scripts\package-windows.ps1), puis crée la release « v<version> » avec ces deux
# fichiers et les notes de version (Markdown).
#
# Avant de lancer : monter la version dans Cargo.toml (ex. 0.3.0), commiter, pousser.
# L'application compare cette version à la sienne : une version plus récente est proposée
# à tous les utilisateurs (pastille « Version x.y.z » en haut à droite).
#
# Prérequis : GitHub CLI connecté (winget install GitHub.cli, puis gh auth login), et les
# prérequis de scripts\package-windows.ps1.
param(
    [Parameter(Mandatory = $true)][string]$Notes
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Notes)) { throw "Notes de version introuvables : $Notes" }
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw "GitHub CLI introuvable : winget install GitHub.cli" }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$tag = "v$version"
Write-Host "== Publication de TracksPlus $version ($tag)"

# Tout doit être commité et poussé : la release pointe sur ce qui est sur GitHub.
if (git status --porcelain) { throw "Des modifications ne sont pas commitées." }
git fetch --quiet origin
$branch = git rev-parse --abbrev-ref HEAD
if ((git rev-parse HEAD) -ne (git rev-parse "origin/$branch")) { throw "La branche $branch n'est pas poussée sur GitHub." }
gh release view $tag 2>$null | Out-Null
if ($LASTEXITCODE -eq 0) { throw "La release $tag existe déjà : montez la version dans Cargo.toml." }

& (Join-Path $PSScriptRoot 'package-windows.ps1')
if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) { throw "La construction du paquet a échoué" }

$setup = Join-Path $root "dist\TracksPlus-Setup-$version.exe"
$zip = Join-Path $root "dist\TracksPlus-$version-windows-portable.zip"
foreach ($f in $setup, $zip) { if (-not (Test-Path $f)) { throw "Fichier manquant : $f" } }

Write-Host "== Création de la release $tag"
gh release create $tag $setup $zip --target (git rev-parse HEAD) --title "TracksPlus $version" --notes-file $Notes
if ($LASTEXITCODE -ne 0) { throw "La création de la release a échoué" }
Write-Host "OK : https://github.com/ElementV2/tracksplus/releases/tag/$tag"
