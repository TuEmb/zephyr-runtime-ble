<#
  build_lib.ps1 — build the runtime-ble Rust staticlib for a chip and stage it
  under lib/<chip>/libruntime_ble.a (where the Zephyr CMake picks it up).

  Usage:   .\scripts\build_lib.ps1 -Chip nrf54l15
           .\scripts\build_lib.ps1 -Chip nrf52840   # WIP

  Requirements:
   - rustc 1.92 (1.90 miscompiles nrf-sdc -> boot BusFault). Install with:
       rustup toolchain install 1.92
       rustup target add thumbv8m.main-none-eabi thumbv7em-none-eabihf
   - LLVM/clang (for the nrf-sdc/nrf-mpsl bindgen step). Adjust $LlvmDir below
     if your LLVM is not at "C:/Program Files/LLVM" or not version 18.
#>
param(
  [Parameter(Mandatory=$true)]
  [ValidateSet("nrf54l15","nrf54l10","nrf54l05","nrf54lm20","nrf52840","nrf52833","nrf52832")]
  [string]$Chip,
  [string]$Toolchain = "1.92-x86_64-pc-windows-msvc",
  [string]$LlvmDir   = "C:/Program Files/LLVM"
)
$ErrorActionPreference = "Stop"

$root = Split-Path $PSScriptRoot -Parent
$rust = Join-Path $root "rust"

# nRF54L = Cortex-M33 soft-float; nRF52 = Cortex-M4F hard-float.
$target = if ($Chip -like "nrf54l*") { "thumbv8m.main-none-eabi" } else { "thumbv7em-none-eabihf" }

# nrf-sdc-sys / nrf-mpsl-sys run bindgen over the SoftDevice Controller + MDK
# headers. Point clang at LLVM's freestanding stdint so uint32_t etc. resolve
# (the default LIBCLANG_PATH on some setups is the ESP xtensa clang — wrong arch).
$clangInc = Get-ChildItem (Join-Path $LlvmDir "lib/clang") -Directory | Select-Object -First 1
$env:LIBCLANG_PATH = (Join-Path $LlvmDir "bin")
$env:BINDGEN_EXTRA_CLANG_ARGS = "-ffreestanding -include stdint.h -isystem `"$($clangInc.FullName)/include`""

Push-Location $rust
try {
  & cargo "+$Toolchain" build --release --no-default-features --features $Chip --target $target
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally {
  Pop-Location
}

$src = Join-Path $rust "target/$target/release/libruntime_ble.a"
$dstDir = Join-Path $root "lib/$Chip"
New-Item -ItemType Directory -Force $dstDir | Out-Null
Copy-Item $src (Join-Path $dstDir "libruntime_ble.a") -Force
Write-Host "OK -> $dstDir/libruntime_ble.a"
