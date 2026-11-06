# Builds beacon.exe (release) and packs it into Beacon-<version>.msi in target\installer.
# Needs the .NET SDK (6 or later); WiX comes from NuGet on the first build.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
cargo build --release --manifest-path "$root\Cargo.toml"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "$root\target" }
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
dotnet build "$PSScriptRoot\Beacon.wixproj" -c Release -p:BeaconVersion=$version -p:BeaconExe="$target\release\beacon.exe" -p:OutputPath="$target\installer\" -p:BaseIntermediateOutputPath="$target\installer\obj\"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Move-Item -Force "$target\installer\Beacon.msi" "$target\installer\Beacon-$version.msi"
Write-Host "Built $target\installer\Beacon-$version.msi"
