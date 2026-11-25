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
# Prérequis : GitHub CLI connecté (winget install GitHub.cli, puis gh auth login), les
# prérequis de scripts\package-windows.ps1, et la clé privée de signature de l'éditeur
# (.signing\release-signing-key.txt dans ce dossier, jamais envoyé sur GitHub ; sinon
# %USERPROFILE%\.tracksplus\release-signing-key.txt, ou $env:TRACKSPLUS_SIGNING_KEY).
# Chaque fichier publié est accompagné de sa signature (<fichier>.sig) : l'application
# refuse toute mise à jour qui n'est pas signée par cette clé. Sans elle, plus aucune mise
# à jour automatique n'est possible : gardez-en une copie de sauvegarde hors de ce PC.
param(
    [Parameter(Mandatory = $true)][string]$Notes
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Notes)) { throw "Notes de version introuvables : $Notes" }
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw "GitHub CLI introuvable : winget install GitHub.cli" }
$key = if ($env:TRACKSPLUS_SIGNING_KEY) { $env:TRACKSPLUS_SIGNING_KEY }
       elseif (Test-Path (Join-Path $root '.signing\release-signing-key.txt')) { Join-Path $root '.signing\release-signing-key.txt' }
       else { Join-Path $env:USERPROFILE '.tracksplus\release-signing-key.txt' }
# La clé ne doit jamais partir sur GitHub.
git check-ignore -q $key 2>$null
if ($key.StartsWith($root, [StringComparison]::OrdinalIgnoreCase) -and $LASTEXITCODE -ne 0) { throw "La clé $key n'est pas exclue de Git (.gitignore)" }
if (-not (Test-Path $key)) { throw "Clé de signature introuvable : $key (sans elle, les mises à jour seraient refusées)" }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$tag = "v$version"
Write-Host "== Publication de TracksPlus $version ($tag)"

# Tout doit être commité et poussé : la release pointe sur ce qui est sur GitHub.
if (git status --porcelain) { throw "Des modifications ne sont pas commitées." }
git fetch --quiet origin
$branch = git rev-parse --abbrev-ref HEAD
if ((git rev-parse HEAD) -ne (git rev-parse "origin/$branch")) { throw "La branche $branch n'est pas poussée sur GitHub." }
# (« release not found » attendu : jamais une erreur bloquante, Windows PowerShell 5.1 compris.)
$ErrorActionPreference = 'Continue'
gh release view $tag *> $null
$exists = $LASTEXITCODE -eq 0
$ErrorActionPreference = 'Stop'
if ($exists) { throw "La release $tag existe déjà : montez la version dans Cargo.toml." }

& (Join-Path $PSScriptRoot 'package-windows.ps1')
if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) { throw "La construction du paquet a échoué" }

$setup = Join-Path $root "dist\TracksPlus-Setup-$version.exe"
$zip = Join-Path $root "dist\TracksPlus-$version-windows-portable.zip"
foreach ($f in $setup, $zip) { if (-not (Test-Path $f)) { throw "Fichier manquant : $f" } }

Write-Host "== Signature de l'éditeur"
$sigs = foreach ($f in $setup, $zip) {
    # Sortie de l'outil affichée, jamais ajoutée à la liste des fichiers à publier.
    cargo run -q --release --example sign_release -- sign $key $f $version | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "Signature impossible : $f" }
    if (-not (Test-Path "$f.sig")) { throw "Signature absente : $f.sig" }
    "$f.sig"
}
$sigs = @($sigs | Select-Object -Unique)

Write-Host "== Création de la release $tag"
gh release create $tag $setup $zip @sigs --target (git rev-parse HEAD) --title "TracksPlus $version" --notes-file $Notes
if ($LASTEXITCODE -ne 0) { throw "La création de la release a échoué" }
Write-Host "OK : https://github.com/ElementV2/tracksplus/releases/tag/$tag"
