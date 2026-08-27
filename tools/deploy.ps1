<#
.SYNOPSIS
    Builds Pubsplash in release mode and replaces a portable install with it.

.DESCRIPTION
    The local equivalent of the release workflow's "Assemble portable ZIP" step,
    without the tag, the installer or the upload. It:

      1. Refuses to touch an install that is running, or that is an *installed*
         copy rather than a portable one.
      2. Runs `cargo build --release` (and `cargo test` with -Test).
      3. Regenerates readme.html and changelog.html, which Help > Open Readme
         and Help > View Changelog hand to the browser. Best effort: these need
         `marked`, reached through whichever of marked/pnpm/npx this machine
         has, and finding none of them only warns.
      4. Replaces the shipped files, moving the old ones aside first so a
         failure half way through rolls back rather than leaving a broken folder.

    It only ever writes the files the portable ZIP ships. user_data\ -- settings,
    logs, crash dumps, recordings, staged updates -- and anything else you keep
    in the folder are left alone, which is the same rule updater::apply_portable
    follows and the reason a portable install can be updated in place at all.

    Keep the file list below in step with .github/workflows/release.yml; it is
    the same set, and a missing helper breaks Tools > Sound Pack Manager or the
    VST plugin scan without any obvious sign of why.

.PARAMETER Target
    The portable install to replace. Created, with a portable.txt marker, if it
    does not exist yet.

    Defaults to $env:PUBSPLASH_DEPLOY_TARGET, and failing that to
    "stuff\software\pubsplash" under your user profile. Derived rather than
    written out so this file carries no one machine's user name.

.PARAMETER Force
    Stop a Pubsplash that is running out of the target folder instead of
    refusing. Without this the script says which process is in the way and
    changes nothing -- Windows will not replace a running exe.

.PARAMETER SkipBuild
    Deploy whatever is already in target\release. For repeating a deploy that
    failed on the copy, or shipping a build you made by hand.

.PARAMETER SkipDocs
    Do not regenerate readme.html or changelog.html. Whatever copies are already
    in the repo root are still deployed; this skips the conversion, not the copy.

.PARAMETER Test
    Run `cargo test` before building. Off by default because tests build under
    the dev profile, so it is a second full compile rather than a few seconds.

.PARAMETER DryRun
    Print what would be built and copied without building or copying.

.EXAMPLE
    ./tools/deploy.ps1
    Builds release and replaces the install at the default path.

.EXAMPLE
    ./tools/deploy.ps1 -Force
    The same, stopping Pubsplash first if it is running.

.EXAMPLE
    ./tools/deploy.ps1 -SkipBuild -SkipDocs
    Copies the existing target\release binaries over and nothing else.
#>
[CmdletBinding()]
param(
    [string]$Target,
    [switch]$Force,
    [switch]$SkipBuild,
    [switch]$SkipDocs,
    [switch]$Test,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Resolve everything from the script's own location so it works from any
# working directory, exactly as tools/release-changelog.ps1 does.
$repoRoot   = Split-Path -Parent $PSScriptRoot
$releaseDir = Join-Path $repoRoot 'target\release'

# Every user-facing binary, matching the `Assemble portable ZIP` step in
# .github/workflows/release.yml. pubsplash.exe resolves its helpers as siblings
# of itself. gen-help is a dev tool and is deliberately absent; the default
# sound pack and help.toml are compiled into the exe, so there are no loose
# assets to copy.
$Binaries = @(
    'pubsplash.exe',
    'pubsplash-scan.exe',
    'pubsplash-soundpack.exe',
    'pubsplash-update.exe',
    'soundpack.exe'
)

# Generated from the Markdown sources rather than kept in the repo, which is why
# they are gitignored. Deployed only if they exist.
$Docs = @(
    @{ Html = 'readme.html';    Markdown = 'README.md' },
    @{ Html = 'changelog.html'; Markdown = 'changelog.md' }
)

# How to turn Markdown into HTML, in preference order: an installed `marked`
# first, then the two package runners that fetch it on demand. Which of those
# works is a property of the machine rather than of the repo -- an npx that is a
# pnpm-only shim refuses outright -- so all of them are tried rather than one
# being assumed. The workflow installs marked globally and needs none of this.
$MarkdownConverters = @(
    @{ Command = 'marked'; Arguments = @() },
    @{ Command = 'pnpm';   Arguments = @('dlx', 'marked') },
    @{ Command = 'npx';    Arguments = @('--yes', 'marked') }
)

# Written into a folder that has no marker yet. Same text as the workflow's, so
# a folder this script creates and one unzipped from a release agree.
$PortableMarkerText = @'
This is the portable build of Pubsplash. Everything it needs is in this
folder; nothing is installed and nothing is written to the registry.

Move the whole folder wherever you like and run pubsplash.exe. Your
settings, logs, crash dumps and recordings go in the user_data folder
beside it, so the folder is the whole installation: carry it on a USB
stick, copy it to another machine, or delete it, and nothing is left
behind anywhere else. Updates replace the program files here and leave
user_data alone.

Saved passwords and API keys are the one exception. Windows encrypts
them for the account that entered them, so on another machine or user
account they read as blank and have to be entered again.

Do not delete this file: Pubsplash reads it to tell a portable copy
from an installed one, both when it updates itself and when it decides
where to keep your settings.
'@

function Get-PackageVersion {
    # The first `version = ` in Cargo.toml is the package's own; the dependency
    # tables come later and must not be matched. Same rule as the workflow's
    # `grep -m1`.
    $match = Select-String -Path (Join-Path $repoRoot 'Cargo.toml') -Pattern '^version = "(.*)"' |
        Select-Object -First 1
    if ($match) { return $match.Matches[0].Groups[1].Value }
    return 'unknown'
}

function Fail {
    param([string]$Message)

    # A plain one-line error rather than PowerShell's exception trace. Every
    # condition that uses this is an expected and actionable one -- something is
    # running, something is missing, the build failed -- and the trace is four
    # lines of noise in front of the sentence that matters, which is worse again
    # read aloud. `throw` is kept for the copy step, where the catch has a
    # rollback to run first.
    Write-Host $Message -ForegroundColor Red
    exit 1
}

function Convert-Doc {
    param([string]$Markdown, [string]$Html)

    foreach ($converter in $MarkdownConverters) {
        if (-not (Get-Command $converter.Command -ErrorAction SilentlyContinue)) { continue }

        # Converted to a scratch file and moved into place only once it has
        # content, so a runner that prints its refusal and writes nothing -- or
        # one that dies half way -- cannot leave a truncated page for the copy
        # step to deploy. GetTempFileName creates it empty, which is exactly the
        # "nothing happened" state being tested for.
        $scratch = [System.IO.Path]::GetTempFileName()
        try {
            & $converter.Command @($converter.Arguments) $Markdown -o $scratch 2>&1 | Out-Null
            if ((Get-Item -LiteralPath $scratch).Length -gt 0) {
                Move-Item -LiteralPath $scratch -Destination $Html -Force
                return $converter.Command
            }
        } catch {
            # Try the next runner; the caller reports only a total failure.
        } finally {
            if (Test-Path -LiteralPath $scratch) {
                Remove-Item -LiteralPath $scratch -Force -ErrorAction SilentlyContinue
            }
        }
    }
    return $null
}

function Get-BlockingProcesses {
    param([string]$Root)

    $prefix = $Root.TrimEnd('\') + '\'
    $names = $Binaries | ForEach-Object { [System.IO.Path]::GetFileNameWithoutExtension($_) }
    $running = Get-Process -Name $names -ErrorAction SilentlyContinue
    if (-not $running) { return @() }

    # Filtered by path, not by name: another Pubsplash somewhere else on the
    # machine holds no lock on the files being replaced here, and `soundpack` is
    # a common enough name to belong to something unrelated. Reading .Path
    # throws for a process this account cannot open, which is answer enough.
    return @($running | Where-Object {
        $path = $null
        try { $path = $_.Path } catch { $path = $null }
        $path -and $path.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)
    })
}

function Assert-TargetIsPortable {
    param([string]$Root)

    # An NSIS install writes uninstall.exe into $INSTDIR and no marker; the
    # portable ZIP writes the marker and no uninstaller. src/update/install_kind.rs
    # tells them apart the same way. Overwriting an installed copy's files by
    # hand leaves the uninstaller and the registry describing a build that is no
    # longer there, so it is refused rather than half-supported.
    if (Test-Path (Join-Path $Root 'uninstall.exe')) {
        Fail "$Root is an installed copy (it has uninstall.exe), not a portable one. Deploy to a portable folder, or uninstall it first."
    }
}

# --- Work out what is going to happen --------------------------------------

# Where a deploy goes when -Target was not passed. Set PUBSPLASH_DEPLOY_TARGET to
# send it somewhere else permanently; otherwise it lands under the profile of
# whoever is running it. Derived rather than written out because a path typed in
# here would carry one machine's user name into the repo, and would point
# everyone else's deploy at a folder that is not theirs.
if (-not $Target) {
    if ($env:PUBSPLASH_DEPLOY_TARGET) {
        $Target = $env:PUBSPLASH_DEPLOY_TARGET
    } elseif ($env:USERPROFILE) {
        $Target = Join-Path $env:USERPROFILE 'stuff\software\pubsplash'
    } else {
        Fail 'Nowhere to deploy to: pass -Target, or set PUBSPLASH_DEPLOY_TARGET.'
    }
}

$version = Get-PackageVersion
Write-Host "Deploying Pubsplash $version to $Target" -ForegroundColor Cyan

$targetExists = Test-Path $Target
if ($targetExists) {
    # Resolved so the process check below compares like with like, whatever
    # relative path or casing was passed in.
    $Target = (Resolve-Path -LiteralPath $Target).Path
    Assert-TargetIsPortable $Target

    # Wrapped at the call site: PowerShell unrolls an empty array returned from
    # a function into nothing at all, and StrictMode then refuses `.Count` on
    # the $null that lands here.
    $blocking = @(Get-BlockingProcesses $Target)
    if ($blocking.Count -gt 0) {
        $description = ($blocking | ForEach-Object { "$($_.ProcessName) (pid $($_.Id))" }) -join ', '
        if (-not $Force) {
            Fail "Pubsplash is running from $Target - $description. Windows will not replace a running exe. Close it, or pass -Force."
        }
        if ($DryRun) {
            Write-Host "Would stop: $description" -ForegroundColor Yellow
        } else {
            Write-Host "Stopping $description" -ForegroundColor Yellow
            $blocking | Stop-Process -Force
            # The single-instance mutex is released by the kernel on exit, so
            # waiting for the handles to go is all that is needed before the
            # files can be replaced.
            foreach ($process in $blocking) {
                if (-not $process.WaitForExit(10000)) {
                    Fail "$($process.ProcessName) (pid $($process.Id)) did not exit."
                }
            }
        }
    }
} else {
    Write-Host "$Target does not exist; it will be created as a fresh portable install." -ForegroundColor Yellow
}

# --- Build ------------------------------------------------------------------

Push-Location $repoRoot
try {
    if ($Test) {
        if ($DryRun) {
            Write-Host 'Would run: cargo test' -ForegroundColor Yellow
        } else {
            Write-Host 'Running tests...' -ForegroundColor Cyan
            cargo test
            if ($LASTEXITCODE -ne 0) { Fail "cargo test exited with $LASTEXITCODE; nothing was deployed." }
        }
    }

    if ($SkipBuild) {
        Write-Host 'Skipping the build; using whatever is in target\release.' -ForegroundColor Yellow
    } elseif ($DryRun) {
        Write-Host 'Would run: cargo build --release' -ForegroundColor Yellow
    } else {
        Write-Host 'Building release...' -ForegroundColor Cyan
        cargo build --release
        if ($LASTEXITCODE -ne 0) { Fail "cargo build --release exited with $LASTEXITCODE; nothing was deployed." }
    }

    # --- Documentation ------------------------------------------------------
    # Only a warning when it fails: stale help pages are worth mentioning and
    # are no reason to withhold a working build.
    if (-not $SkipDocs -and -not $DryRun) {
        foreach ($doc in $Docs) {
            $html = Join-Path $repoRoot $doc.Html
            $used = Convert-Doc -Markdown (Join-Path $repoRoot $doc.Markdown) -Html $html
            if ($used) {
                Write-Host "Generated $($doc.Html) with $used" -ForegroundColor Green
            } else {
                $state = if (Test-Path $html) {
                    'the existing copy will be deployed as it stands'
                } else {
                    'it will not be deployed, so the installed copy keeps whatever it already had'
                }
                Write-Host "Could not generate $($doc.Html) - no working Markdown converter; $state." -ForegroundColor Yellow
            }
        }
    } elseif ($SkipDocs) {
        Write-Host 'Skipping readme.html and changelog.html.' -ForegroundColor Yellow
    }
} finally {
    Pop-Location
}

# --- Assemble the file list -------------------------------------------------

$plan = @()
foreach ($name in $Binaries) {
    $plan += [pscustomobject]@{ Name = $name; Source = Join-Path $releaseDir $name }
}
foreach ($doc in $Docs) {
    $source = Join-Path $repoRoot $doc.Html
    if (Test-Path $source) {
        $plan += [pscustomobject]@{ Name = $doc.Html; Source = $source }
    }
}

# Checked as a set before anything is moved: the cheapest possible way to avoid
# a half-replaced folder is to find out that a file is missing while the folder
# is still untouched.
$missing = @($plan | Where-Object { -not (Test-Path $_.Source) })
if ($missing.Count -gt 0) {
    $names = ($missing | ForEach-Object { $_.Name }) -join ', '
    Fail "Missing from $releaseDir - $names. Build first, or drop -SkipBuild."
}

if ($DryRun) {
    Write-Host "`nWould deploy to $Target" -ForegroundColor Cyan
    $plan | ForEach-Object {
        [pscustomobject]@{ File = $_.Name; Bytes = (Get-Item $_.Source).Length }
    } | Format-Table -AutoSize | Out-String -Width 100 | Write-Host
    Write-Host 'Nothing else in the folder would be touched, user_data included.' -ForegroundColor Cyan
    exit 0
}

# --- Replace ----------------------------------------------------------------

if (-not $targetExists) {
    New-Item -ItemType Directory -Path $Target | Out-Null
    $Target = (Resolve-Path -LiteralPath $Target).Path
}

# Written before the binaries so a folder created here is portable from its very
# first run. Without it, data_dir would put settings in %LOCALAPPDATA% instead.
$marker = Join-Path $Target 'portable.txt'
if (-not (Test-Path $marker)) {
    Set-Content -LiteralPath $marker -Value $PortableMarkerText -Encoding UTF8
    Write-Host 'Wrote portable.txt (this folder had no marker).' -ForegroundColor Yellow
}

# The old files are moved aside rather than overwritten so a failure part way
# through can be undone -- the same reason updater::apply_portable uses a
# .pubsplash-old folder. Only the files in $plan are ever moved, so user_data
# and anything else living here is untouched.
$aside = Join-Path $Target '.pubsplash-old'
if (Test-Path $aside) { Remove-Item -LiteralPath $aside -Recurse -Force }
New-Item -ItemType Directory -Path $aside | Out-Null

$movedAside = New-Object System.Collections.Generic.List[string]
try {
    foreach ($item in $plan) {
        $destination = Join-Path $Target $item.Name
        if (Test-Path $destination) {
            Move-Item -LiteralPath $destination -Destination (Join-Path $aside $item.Name) -Force
            $movedAside.Add($item.Name) | Out-Null
        }
        Copy-Item -LiteralPath $item.Source -Destination $destination -Force
    }
} catch {
    Write-Host "Deploy failed part way through; putting the old files back." -ForegroundColor Red
    foreach ($name in $movedAside) {
        $destination = Join-Path $Target $name
        if (Test-Path $destination) { Remove-Item -LiteralPath $destination -Force -ErrorAction SilentlyContinue }
        Move-Item -LiteralPath (Join-Path $aside $name) -Destination $destination -Force
    }
    Remove-Item -LiteralPath $aside -Recurse -Force -ErrorAction SilentlyContinue
    throw
}

Remove-Item -LiteralPath $aside -Recurse -Force

# --- Report -----------------------------------------------------------------

Write-Host "`nDeployed Pubsplash $version to $Target" -ForegroundColor Green
$plan | ForEach-Object {
    $item = Get-Item (Join-Path $Target $_.Name)
    [pscustomobject]@{ File = $item.Name; Bytes = $item.Length; Written = $item.LastWriteTime }
} | Format-Table -AutoSize | Out-String -Width 100 | Write-Host
Write-Host 'user_data and everything else in the folder was left alone.' -ForegroundColor Green

# Explicit, because a script that ends without one exits with the status of the
# last *native* command it ran -- which is cargo, or a Markdown runner that was
# allowed to fail. A deploy that got this far succeeded, and anything calling
# this script needs to be able to tell.
exit 0
