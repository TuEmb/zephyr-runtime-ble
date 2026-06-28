# Building the `runtime_ble` staticlib

This crate compiles to a per-chip static library
(`libruntime_ble.a`) — the Rust BLE stack (TrouBLE + the Nordic SoftDevice
Controller) that the Zephyr module links against. You only need this when you
change `rust/`; the prebuilt `lib/<chip>/libruntime_ble.a` is committed.

## Requirements

- **Rust 1.92** — 1.90 miscompiles `nrf-sdc` (boot BusFault). Newer than 1.92
  is fine *except* it would want `fixed` 1.31 (we pin `=1.30.0` for 1.92).
  ```sh
  rustup toolchain install 1.92
  rustup target add thumbv8m.main-none-eabi thumbv7em-none-eabihf
  ```
- **LLVM / clang** — `nrf-sdc-sys` / `nrf-mpsl-sys` run `bindgen` over the
  SoftDevice Controller + MDK headers. clang must see a freestanding `stdint.h`
  so `uint32_t` & co. resolve.

## Chip → feature → target

Build with **exactly one** chip feature. The target follows the core:

| Chip | `--features` | `--target` |
|---|---|---|
| nRF54L15 / L10 / L05 / LM20 | `nrf54l15` / `nrf54l10` / `nrf54l05` / `nrf54lm20` | `thumbv8m.main-none-eabi` (M33, soft-float) |
| nRF52840 / 52833 / 52832 | `nrf52840` / `nrf52833` / `nrf52832` | `thumbv7em-none-eabihf` (M4F, hard-float) |

> nRF54L05/L10 share the same die as nRF54L15 (SDC `NRF54L15_XXAA`); you can
> just copy the `nrf54l15` build into `lib/nrf54l05/` and `lib/nrf54l10/`
> instead of rebuilding.

## Build

The two environment variables point `bindgen`'s clang at a freestanding
`stdint.h` and the matching clang resource headers. Set them, run `cargo
build`, then copy the resulting `.a` into `lib/<chip>/`.

### Linux / macOS (bash)

```sh
CHIP=nrf54l15
TARGET=thumbv8m.main-none-eabi   # nrf52*: thumbv7em-none-eabihf

# clang for bindgen (apt: llvm-dev libclang-dev clang; brew: llvm)
export LIBCLANG_PATH="$(llvm-config --libdir 2>/dev/null || echo /usr/lib/llvm-18/lib)"
export BINDGEN_EXTRA_CLANG_ARGS="-ffreestanding -include stdint.h -isystem $(clang -print-resource-dir)/include"

cargo +1.92 build --release --no-default-features --features "$CHIP" --target "$TARGET"

mkdir -p ../lib/$CHIP
cp target/$TARGET/release/libruntime_ble.a ../lib/$CHIP/
```

### Windows (PowerShell)

```powershell
$Chip   = "nrf54l15"
$Target = "thumbv8m.main-none-eabi"   # nrf52*: thumbv7em-none-eabihf
$Llvm   = "C:/Program Files/LLVM"     # adjust if installed elsewhere

$clangInc = (Get-ChildItem "$Llvm/lib/clang" -Directory | Select-Object -First 1).FullName
$env:LIBCLANG_PATH = "$Llvm/bin"
$env:BINDGEN_EXTRA_CLANG_ARGS = "-ffreestanding -include stdint.h -isystem `"$clangInc/include`""

cargo +1.92-x86_64-pc-windows-msvc build --release --no-default-features --features $Chip --target $Target

New-Item -ItemType Directory -Force "../lib/$Chip" | Out-Null
Copy-Item "target/$Target/release/libruntime_ble.a" "../lib/$Chip/" -Force
```

The Zephyr build (`west build`) then picks up `lib/<chip>/libruntime_ble.a`
via the module's `CMakeLists.txt`.

## Role variants (optional)

The default lib is the lean **peripheral + GATT-server** build. Extra roles are
compile-time Cargo features, baked into a separate lib whose filename encodes the
roles — `CMakeLists.txt` selects it from the matching `CONFIG_RUNTIME_BLE_*`:

| Cargo features | Staged as | Selected by |
|---|---|---|
| (none) | `libruntime_ble.a` | default |
| `central` | `libruntime_ble_central.a` | `CONFIG_RUNTIME_BLE_CENTRAL=y` |
| `l2cap` | `libruntime_ble_l2cap.a` | `CONFIG_RUNTIME_BLE_L2CAP=y` |
| `central,l2cap` | `libruntime_ble_central_l2cap.a` | both of the above |

The filename suffix is `_central` then `_l2cap`, in that order (matching
`CMakeLists.txt`).

Add the feature to the build and stage under the matching name, e.g. on Linux:
```sh
cargo +1.92 build --release --no-default-features --features "$CHIP,central" --target "$TARGET"
cp target/$TARGET/release/libruntime_ble.a ../lib/$CHIP/libruntime_ble_central.a
```

## Layout

```
rust/
├── Cargo.toml            crate + per-chip features (+ pinned git deps)
└── src/
    ├── lib.rs            C ABI, heap allocator, panic, GATT config types
    ├── radio.rs          chip-agnostic: executor, runtime GATT, advertise loop
    └── chip/
        ├── nrf54l.rs     nRF54L bring-up (MPSL/SDC + IRQ shims, CRACEN RNG)
        └── nrf52.rs      nRF52 bring-up (MPSL/SDC + IRQ shims, RNG peripheral)
```

## Troubleshooting

- **`uint32_t` / `stdint.h` not found during bindgen** — `BINDGEN_EXTRA_CLANG_ARGS`
  isn't pointing at a freestanding clang resource dir (or `LIBCLANG_PATH` is a
  wrong-arch clang, e.g. an ESP/xtensa toolchain).
- **link error "uses VFP register arguments"** (nRF52) — the lib is hard-float
  (`thumbv7em-none-eabihf`); the consuming app must set `CONFIG_FPU=y` +
  `CONFIG_FP_HARDABI=y`.
- **boot BusFault / hard fault on a clean build** — you built with rustc 1.90;
  use 1.92.
