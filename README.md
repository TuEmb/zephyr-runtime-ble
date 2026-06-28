# runtime-ble

A **loadable BLE runtime** for Zephyr, packaged as a Zephyr module. The BLE
stack is [TrouBLE](https://github.com/embassy-rs/trouble) (a Rust `no_std` BLE
host) + the Nordic **SoftDevice Controller**, compiled to a per-chip Rust
`staticlib` and linked into a Zephyr app through a thin C glue layer.

The library **owns the radio**, so the application is built with **`CONFIG_BT=n`**
(the Zephyr Bluetooth stack and MPSL are off). Runtime state is allocated from
the Zephyr heap on `runtime_ble_load()` and freed on `runtime_ble_unload()`, so
it costs ~no RAM until loaded.

It exposes a generic **Nordic-UART-style GATT peripheral** (RX write + TX notify)
behind a small C API ([`include/runtime_ble.h`](include/runtime_ble.h)).

Targets **Zephyr 4.4.1**.

## Layout

```
runtime-ble/
├── zephyr/module.yml        Zephyr module manifest
├── CMakeLists.txt           links lib/<chip>/libruntime_ble.a + glue.c
├── Kconfig                  CONFIG_RUNTIME_BLE (+ thread stack/priority)
├── include/runtime_ble.h    C API: init / load / unload / send + callbacks
├── glue/glue.c              Zephyr glue (thread, sem, alarm, hwinfo addr, IRQs)
├── lib/<chip>/              prebuilt staticlibs (built from rust/)
├── rust/                    the Rust crate (trouble + nrf-sdc)
│   └── src/
│       ├── lib.rs           C ABI + heap allocator + panic
│       ├── radio.rs         chip-agnostic: executor, NUS server, advertise loop
│       └── chip/<soc>.rs    per-chip MPSL/SDC + interrupt bring-up
├── scripts/build_lib.ps1    build + stage the staticlib for a chip
└── examples/<board>/        example apps
```

## Board support

Two chip families, one shared bring-up each (`rust/src/chip/nrf54l.rs`,
`rust/src/chip/nrf52.rs`). Which chips are possible is bounded by `nrf-sdc`.

| Chip | `--features` | Rust target | Example board | Status |
|---|---|---|---|---|
| nRF54L15 | `nrf54l15` | `thumbv8m.main-none-eabi` | `xiao_nrf54l15/nrf54l15/cpuapp`, `nrf54l15dk/nrf54l15/cpuapp` | ✅ HW-verified (advertise + config) |
| nRF54L10 | `nrf54l10` | `thumbv8m.main-none-eabi` | `nrf54l15dk/nrf54l10/cpuapp` | ✅ builds |
| nRF54L05 | `nrf54l05` | `thumbv8m.main-none-eabi` | `nrf54l15dk/nrf54l05/cpuapp` | ✅ builds (shares nRF54L15 lib) |
| nRF54LM20 | `nrf54lm20` | `thumbv8m.main-none-eabi` | `nrf54lm20dk/nrf54lm20/cpuapp` | lib builds; example untested |
| nRF52840 | `nrf52840` | `thumbv7em-none-eabihf` | `nrf52840dk/nrf52840` | ✅ builds |
| nRF52833 | `nrf52833` | `thumbv7em-none-eabihf` | `nrf52833dk/nrf52833` | lib builds |
| nRF52832 | `nrf52832` | `thumbv7em-none-eabihf` | `nrf52dk/nrf52832` | lib builds; 64 KB RAM is tight |

Notes:
- **nRF52 is hard-float** — the prebuilt lib is `thumbv7em-none-eabihf`, so the nRF52
  examples set `CONFIG_FPU=y` + `CONFIG_FP_HARDABI=y` (already in their prj.conf).
- nRF54L05/L10 reuse the nRF54L15 staticlib (same die / SDC `NRF54L15_XXAA`).
- Only nRF54L15 is flash-tested here (the only board on the rig); the rest are
  build-verified — flash-test when you have the hardware.

## Build

### 1. Prerequisites
- **west**, **Zephyr SDK 1.0.x** (or a `gnuarmemb` arm-none-eabi toolchain), CMake, Ninja, Python.
- **Rust 1.92** (1.90 miscompiles nrf-sdc): `rustup toolchain install 1.92`,
  `rustup target add thumbv8m.main-none-eabi thumbv7em-none-eabihf`.
- **LLVM/clang** (for the nrf-sdc/nrf-mpsl bindgen step).

### 2. Examples are standalone west apps (no `-DZEPHYR_EXTRA_MODULES`)
Each `examples/<chip>/` is an **independent application** with its own
[`west.yml`](examples/nrf54l15/west.yml) that pulls **Zephyr 4.4.1 + the
`zephyr-runtime-ble` module via git**. west auto-discovers runtime-ble as a Zephyr
module (it has `zephyr/module.yml`), so nothing is passed on the command line.

Use an example as its own git repo (e.g. copy `examples/nrf54l15` out), then:
```sh
west init -l nrf54l15      # the example dir is the manifest repo
west update                # clones Zephyr v4.4.1 + zephyr-runtime-ble (+ its prebuilt libs)
west zephyr-export
west build -p always -b xiao_nrf54l15/nrf54l15/cpuapp nrf54l15
west flash
```
> Board per chip: see the table above (e.g. `nrf54l15dk/nrf54l10/cpuapp`, `nrf52840dk/nrf52840`).
> With a `gnuarmemb` toolchain add `-- -DZEPHYR_TOOLCHAIN_VARIANT=gnuarmemb -DGNUARMEMB_TOOLCHAIN_PATH=<dir>`.
> Pin the runtime-ble version with the `revision:` in the example's `west.yml`.

### 3. (Re)building the per-chip staticlib
The prebuilt `lib/<chip>/libruntime_ble.a` is committed (so `west update` brings it). Rebuild it
only after editing `rust/`, from a clone of this library repo:
```powershell
.\scripts\build_lib.ps1 -Chip nrf54l15   # stages lib/nrf54l15/libruntime_ble.a
```

## Try it
Scan with the **nRF Connect** mobile app for `RUNTIME-BLE`, connect, find the
Nordic UART Service (`6e400001-…`), enable notifications on TX (`6e400003`), and
write bytes to RX (`6e400002`) — the example echoes them back on TX.

## API
```c
runtime_ble_init(&cfg);   // device name, manufacturer id, callbacks
runtime_ble_load();       // bring radio up, advertise (fast 30-60 ms)
runtime_ble_send(buf, n); // notify the connected central
runtime_ble_unload();     // tear down, free all session RAM
```
`cfg.callbacks`: `on_connected`, `on_disconnected`, `on_data` (RX bytes),
`on_log`. Callbacks run on the BLE runtime thread — keep them short.

## Adding a chip
1. Add a `<chip> = ["_radio", "embassy-nrf/<chip>", "nrf-sdc/<chip>"]` feature
   in `rust/Cargo.toml`.
2. Implement `rust/src/chip/<chip>.rs` (`Irqs` + `runtime_irq_*` shims +
   `build_sdc` + `run`) — use `chip/nrf54l15.rs` as the template.
3. Add the matching IRQ branch in `glue/glue.c` and the SoC case in `CMakeLists.txt`.
4. `.\scripts\build_lib.ps1 -Chip <chip>` and add `examples/<board>/`.
