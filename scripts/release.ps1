# Gera o instalador assinado e o latest.json que a atualização automática lê.
#
#   .\scripts\release.ps1             -> build + instalador + latest.json (não publica nada)
#   .\scripts\release.ps1 -SkipBuild  -> refaz instalador e latest.json do build existente
#   .\scripts\release.ps1 -Publish    -> também cria a release vX.Y.Z no GitHub (gh CLI)
#
# O build roda com poucas tarefas e prioridade baixa para não travar o PC.
# A chave privada fica em ~/.tauri/antijanja.key (fora do projeto). Sem ela não dá para
# assinar e os apps instalados recusam a atualização. Guarde uma cópia em lugar seguro.
#
#   -Version 0.3.3   -> usa essa versão (grava no Cargo.toml antes de compilar)
#   -Target <commit> -> commit onde a tag vX.Y.Z é criada (padrão: o branch padrão)
#   -Notes "..."     -> texto da release
#
# Normalmente quem roda isto é a action .github/workflows/release.yml, a cada PR da
# dev mergeado na main.
param([switch]$SkipBuild, [switch]$Publish, [int]$Jobs = 4, [string]$Version = "", [string]$Target = "", [string]$Notes = "")
$ErrorActionPreference = "Stop"
$Repo = "lennoncorrea/antijanja"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

if ($Version) {
  if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Versão inválida: $Version" }
  # Troca só a primeira linha "version = ..." (a do [package]).
  $Lines = [IO.File]::ReadAllLines((Join-Path $Root "Cargo.toml"))
  $i = [Array]::FindIndex($Lines, [Predicate[string]]{ param($l) $l -match '^version = "' })
  $Lines[$i] = "version = `"$Version`""
  [IO.File]::WriteAllLines((Join-Path $Root "Cargo.toml"), $Lines)  # UTF-8 sem BOM
} else {
  $Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
}
if (-not $Notes) { $Notes = "AntiJanja $Version" }
$Exe = Join-Path $Root "target\release\antijanja.exe"
$OutDir = Join-Path $Root "target\release\bundle"
$Setup = Join-Path $OutDir "AntiJanja_${Version}_x64-setup.exe"
$Key = if ($env:TAURI_SIGNING_PRIVATE_KEY_PATH) { $env:TAURI_SIGNING_PRIVATE_KEY_PATH } else { Join-Path $HOME ".tauri\antijanja.key" }
if (-not (Test-Path $Key)) { throw "Chave de assinatura não encontrada em $Key." }

if (-not $SkipBuild) {
  Write-Host "Compilando $Version (até $Jobs tarefas, prioridade baixa)..."
  $env:CARGO_BUILD_JOBS = "$Jobs"
  cmd /c "start `"`" /belownormal /b /wait cargo build --release"
  if ($LASTEXITCODE -ne 0) { throw "cargo build falhou" }
}
if (-not (Test-Path $Exe)) { throw "Não achei $Exe. Rode sem -SkipBuild." }

# NSIS: o que veio com o Tauri, um instalado no sistema ou no PATH.
$Makensis = @(
  (Join-Path $env:LOCALAPPDATA "tauri\NSIS\makensis.exe"),
  (Join-Path $env:LOCALAPPDATA "tauri\NSIS\Bin\makensis.exe"),
  "${env:ProgramFiles(x86)}\NSIS\makensis.exe",
  "$env:ProgramFiles\NSIS\makensis.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Makensis) { $Makensis = (Get-Command makensis -ErrorAction SilentlyContinue).Source }
if (-not $Makensis) { throw "NSIS (makensis) não encontrado." }

New-Item -ItemType Directory -Force $OutDir | Out-Null
& $Makensis /V2 "/DVERSION=$Version" "/DEXE=$Exe" "/DOUTFILE=$Setup" (Join-Path $Root "installer\antijanja.nsi")
if ($LASTEXITCODE -ne 0) { throw "makensis falhou" }

# Assinatura minisign (mesmo formato de antes; a CLI do Tauri só é usada para assinar).
$Password = if ($env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) { $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD } else { "" }
# Versão EXATA da CLI: este comando recebe a chave privada, então nunca deixamos o npx
# baixar sozinho uma versão nova (um pacote comprometido no npm roubaria a chave).
# Para atualizar: escolha uma versão com algumas semanas de publicada e troque aqui.
$TauriCli = "@tauri-apps/cli@2.12.4"
# "--password=" junto: o PowerShell 5.1 descarta argumentos vazios (a chave não tem senha).
npx --yes $TauriCli signer sign -f $Key "--password=$Password" $Setup
if ($LASTEXITCODE -ne 0) { throw "falha ao assinar" }
$Signature = (Get-Content "$Setup.sig" -Raw).Trim()

$Latest = [ordered]@{
  # Os apps antigos (2.x e 3.x) só leem "version" e só atualizam para uma versão maior
  # que a deles: fica fixo em 3.999.0. A versão real (0.x) vai em "app_version".
  version     = "3.999.0"
  app_version = $Version
  notes       = "AntiJanja $Version"
  pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
  platforms = [ordered]@{
    "windows-x86_64" = [ordered]@{
      signature = $Signature
      url       = "https://github.com/$Repo/releases/download/v$Version/AntiJanja_${Version}_x64-setup.exe"
    }
  }
}
$LatestPath = Join-Path $OutDir "latest.json"
[IO.File]::WriteAllText($LatestPath, ($Latest | ConvertTo-Json -Depth 5))  # UTF-8 sem BOM

Write-Host "`nPronto:`n  $Setup`n  $LatestPath"
if ($Publish) {
  $TargetArgs = if ($Target) { @("--target", $Target) } else { @() }
  gh release create "v$Version" $Setup $LatestPath --repo $Repo --title "AntiJanja $Version" --notes $Notes @TargetArgs
  if ($LASTEXITCODE -ne 0) { throw "gh release create falhou" }
} else {
  Write-Host "`nPara publicar: .\scripts\release.ps1 -SkipBuild -Publish"
}
