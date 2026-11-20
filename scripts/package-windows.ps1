[CmdletBinding()]
param(
    [string] $InnoSetupCompiler
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($InnoSetupCompiler) {
    $compiler = (Resolve-Path -LiteralPath $InnoSetupCompiler).Path
} else {
    $command = Get-Command ISCC.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($command) {
        $compiler = $command.Path
    } else {
        $compiler = @(
            "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
            "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
            "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
        ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
        if (!$compiler) {
            throw 'Install Inno Setup 6 or pass the path to ISCC.exe via -InnoSetupCompiler.'
        }
    }
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$runtime = $null
if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
    $runtimePaths = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -find 'VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT\vcruntime140.dll'
    if ($LASTEXITCODE -ne 0) {
        throw "vswhere failed with exit code $LASTEXITCODE."
    }
    $runtime = $runtimePaths | Select-Object -First 1
}
if (!$runtime) {
    throw 'Install the x64 MSVC build tools with the Visual C++ Redistributable files.'
}

$repo = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'
Push-Location -LiteralPath $repo
try {
    $metadataJson = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE."
    }
    $metadata = $metadataJson | ConvertFrom-Json
    $package = $metadata.packages | Where-Object { $_.name -eq 'stroll' }

    & cargo build --release --locked --target $target --bin stroll
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE."
    }

    $binDir = Join-Path $metadata.target_directory "$target\release"
    $outputDir = Join-Path $metadata.target_directory 'windows'
    $outputName = "Stroll-Setup-$($package.version)-x64"
    $script = Join-Path $repo 'packaging\windows\stroll.iss'
    & $compiler "/DAppVersion=$($package.version)" "/DBinDir=$binDir" "/DVCRuntime=$runtime" `
        "/O$outputDir" "/F$outputName" $script
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup failed with exit code $LASTEXITCODE."
    }
    Get-Item -LiteralPath (Join-Path $outputDir "$outputName.exe")
} finally {
    Pop-Location
}
