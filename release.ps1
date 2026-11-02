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
  5. Publishes the same release, with the same files and notes, in -SourceRepo, so both repos
     always list the same releases.

  -Notes is required: a release cannot go out without change notes written for its users.

  Mac: the Mac build runs on GitHub (the "Build HQ for macOS" workflow, run with this same
  version), then `gh run download <run-id> -n hq-macos -D mac-artifacts`. Pass that folder as
  -MacArtifacts and the release also gets the .dmg and the updater tarball, and latest.json gets
  darwin-aarch64 and darwin-x86_64 entries (one universal tarball serves both). Without
  -MacArtifacts the release is Windows-only, as before.

  The private key never enters the repo. If it is lost, installed copies can no longer update
  and every user must install the next version by hand.

.EXAMPLE
  .\release.ps1 0.3.0 -Notes "- Adds @-file mentions."
#>
param(
  [Parameter(Mandatory = $true)] [string] $Version,
  # Release notes in Markdown, written for the people who install HQ.
  [Parameter(Mandatory = $true)] [string] $Notes,
  # Where installers are published. Must match the endpoint in tauri.conf.json.
  [string] $Repo = 'BrydonJ/HQ-releases',
  # HQ's own repo, which gets a copy of every release.
  [string] $SourceRepo = 'BrydonJ/HQ',
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
if (-not $Notes.Trim()) { throw 'Release notes are required: pass -Notes with what changed.' }
if (git status --porcelain) { throw 'Commit or stash your changes first.' }
if (git tag --list $tag) { throw "Tag $tag already exists." }
if (-not (Test-Path $Key)) { throw "Signing key not found: $Key" }
$endpoint = "https://github.com/$Repo/releases/latest/download/latest.json"
if (-not (Select-String -Path tauri.conf.json -SimpleMatch $endpoint -Quiet)) {
  throw "tauri.conf.json does not use $endpoint, so installed copies would never see this release."
}
Invoke-Checked 'gh auth status' { gh auth status *> $null }
# Both repos are checked before anything is built, so a release never lands in only one of them.
foreach ($r in $Repo, $SourceRepo) {
  Invoke-Checked "gh repo view $r" { gh repo view $r --json name *> $null }
  # 'Continue', because Windows PowerShell 5.1 turns gh's "release not found" on stderr into an error.
  $ErrorActionPreference = 'Continue'
  gh release view $tag --repo $r --json tagName *> $null
  $exists = $LASTEXITCODE -eq 0
  $ErrorActionPreference = 'Stop'
  if ($exists) { throw "$r already has a release $tag." }
}

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
# Through a file, because Windows PowerShell 5.1 mangles quotes in multi-line native arguments.
$notesPath = "$bundle\notes.md"
[IO.File]::WriteAllText($notesPath, $Notes, [Text.UTF8Encoding]::new($false))

if (git status --porcelain) { Invoke-Checked 'git commit' { git commit -q -am "Release $tag" } }
Invoke-Checked 'git tag' { git tag -a $tag -m "HQ $tag" }
Invoke-Checked 'git push' { git push -q origin HEAD $tag }
Invoke-Checked 'gh release create' {
  gh release create $tag --repo $Repo --title "HQ $tag" --notes-file $notesPath @assets $latestPath
}
Write-Host "Published HQ $tag to https://github.com/$Repo/releases/tag/$tag"
try {
  Invoke-Checked "gh release create in $SourceRepo" {
    gh release create $tag --repo $SourceRepo --verify-tag --title "HQ $tag" --notes-file $notesPath @assets $latestPath
  }
} catch {
  throw "$_ $tag is live in $Repo only. Finish it with: gh release create $tag --repo $SourceRepo --verify-tag --title `"HQ $tag`" --notes-file `"$notesPath`" $($assets -join ' ') `"$latestPath`""
}
Write-Host "Published HQ $tag to https://github.com/$SourceRepo/releases/tag/$tag"
