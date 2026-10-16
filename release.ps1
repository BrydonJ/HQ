<#
.SYNOPSIS
  Builds a signed HQ installer and publishes it as the release that installed copies update to.

.DESCRIPTION
  1. Sets the version in Cargo.toml and tauri.conf.json.
  2. Builds the NSIS installer with the Tauri CLI, signed with the updater key.
  3. Commits the version change, tags it, and pushes.
  4. Publishes the installer, its signature and latest.json as a GitHub release in -Repo.
     Installed copies of HQ read latest.json from the newest release there and update
     themselves (see src/update.rs).

  Mac: the Mac build runs on GitHub (the "Build HQ for macOS" workflow, run with this same
  version), then `gh run download <run-id> -n hq-macos -D mac-artifacts`. Pass that folder as
  -MacArtifacts and the release also gets the .dmg and the updater tarball, and latest.json gets
  darwin-aarch64 and darwin-x86_64 entries (one universal tarball serves both). Without
  -MacArtifacts the release is Windows-only, as before.

  The private key never enters the repo. If it is lost, installed copies can no longer update
  and every user must install the next version by hand.

.EXAMPLE
  .\release.ps1 0.2.1
  .\release.ps1 0.3.0 -Notes "Adds @-file mentions."
#>
param(
  [Parameter(Mandatory = $true)] [string] $Version,
  # Release notes. By default, the subjects of the commits since the previous tag.
  [string] $Notes,
  # Where installers are published. Must match the endpoint in tauri.conf.json.
  [string] $Repo = 'BrydonJ/HQ-releases',
  [string] $Key = "$env:USERPROFILE\.tauri\hq-updater.key",
  # A folder holding the Mac build's HQ_<version>_universal.dmg, HQ.app.tar.gz and HQ.app.tar.gz.sig.
  [string] $MacArtifacts
)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Invoke-Checked([string] $what, [scriptblock] $run) {
  & $run
  if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)." }
}

# Rewrites the first match only, without adding a BOM or changing line endings.
function Set-FirstMatch([string] $path, [string] $pattern, [string] $replacement) {
  $text = [IO.File]::ReadAllText("$PSScriptRoot\$path")
  $regex = [regex]::new($pattern, 'Multiline')
  if (-not $regex.IsMatch($text)) { throw "No version found in $path." }
  [IO.File]::WriteAllText("$PSScriptRoot\$path", $regex.Replace($text, $replacement, 1), [Text.UTF8Encoding]::new($false))
}

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Version must look like 1.2.3.' }
$tag = "v$Version"
if (git status --porcelain) { throw 'Commit or stash your changes first.' }
if (git tag --list $tag) { throw "Tag $tag already exists." }
if (-not (Test-Path $Key)) { throw "Signing key not found: $Key" }
$endpoint = "https://github.com/$Repo/releases/latest/download/latest.json"
if (-not (Select-String -Path tauri.conf.json -SimpleMatch $endpoint -Quiet)) {
  throw "tauri.conf.json does not use $endpoint, so installed copies would never see this release."
}
Invoke-Checked 'gh auth status' { gh auth status *> $null }

# The Mac files are checked up front, so a wrong folder fails before anything is built or tagged.
$macFiles = $null
if ($MacArtifacts) {
  function Find-Mac([string] $name) {
    $found = Get-ChildItem -Path $MacArtifacts -Recurse -File -Filter $name -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $found) { throw "$name not found under $MacArtifacts (is it the artifact of a Mac build for $Version?)." }
    $found
  }
  $macFiles = [pscustomobject]@{
    Dmg = Find-Mac "HQ_${Version}_universal.dmg"
    Tar = Find-Mac 'HQ.app.tar.gz'
    Sig = Find-Mac 'HQ.app.tar.gz.sig'
  }
}

if (-not $Notes) {
  $previous = git tag --sort=-creatordate --list 'v*' | Select-Object -First 1
  $range = if ($previous) { "$previous..HEAD" } else { 'HEAD' }
  $Notes = (git log $range --no-merges --pretty='- %s') -join "`n"
}

Set-FirstMatch Cargo.toml '^version = "[^"]+"' "version = `"$Version`""
Set-FirstMatch tauri.conf.json '"version": "[^"]+"' "`"version`": `"$Version`""

# A separate target folder, so the build never collides with a running dev copy of HQ.
$env:CARGO_TARGET_DIR = "$PSScriptRoot\target\dist"
$env:TAURI_SIGNING_PRIVATE_KEY = $Key
# Same toolchain workaround as build.cmd: the newest Visual Studio here lacks msvcrt.lib.
$vcvars = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
$setup = if (Test-Path $vcvars) { "call `"$vcvars`" >nul && " } else { '' }
Invoke-Checked 'Build' { cmd /c "$setup npx --yes @tauri-apps/cli@2 build" }

$bundle = "$env:CARGO_TARGET_DIR\release\bundle\nsis"
$installer = Get-Item "$bundle\HQ_${Version}_x64-setup.exe"
$signature = [IO.File]::ReadAllText("$($installer.FullName).sig").Trim()
$latest = [ordered]@{
  version   = $Version
  notes     = $Notes
  pub_date  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
  platforms = [ordered]@{
    'windows-x86_64' = [ordered]@{
      signature = $signature
      url       = "https://github.com/$Repo/releases/download/$tag/$($installer.Name)"
    }
  }
}
$assets = @($installer.FullName, "$($installer.FullName).sig")
if ($macFiles) {
  # Both Mac architectures update from the one universal tarball.
  $macEntry = [ordered]@{
    signature = ([IO.File]::ReadAllText($macFiles.Sig.FullName)).Trim()
    url       = "https://github.com/$Repo/releases/download/$tag/$($macFiles.Tar.Name)"
  }
  $latest.platforms['darwin-aarch64'] = $macEntry
  $latest.platforms['darwin-x86_64'] = $macEntry
  $assets += @($macFiles.Dmg.FullName, $macFiles.Tar.FullName, $macFiles.Sig.FullName)
}
$latestPath = "$bundle\latest.json"
[IO.File]::WriteAllText($latestPath, ($latest | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))

if (git status --porcelain) { Invoke-Checked 'git commit' { git commit -q -am "Release $tag" } }
Invoke-Checked 'git tag' { git tag -a $tag -m "HQ $tag" }
Invoke-Checked 'git push' { git push -q origin HEAD $tag }
Invoke-Checked 'gh release create' {
  gh release create $tag --repo $Repo --title "HQ $tag" --notes $Notes @assets $latestPath
}
Write-Host "Published HQ $tag to https://github.com/$Repo/releases/tag/$tag"
