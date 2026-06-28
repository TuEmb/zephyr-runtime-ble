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

**Not tied to a Zephyr version.** The prebuilt staticlib and the thin C glue use
only stable Zephyr APIs, so the module builds against any recent Zephyr —
verified on 4.1.x through 4.4.x. The examples just pin one release in their
`west.yml`; change it to whatever you build against.

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
│   ├── README.md            how to build the staticlib (Linux/macOS/Windows)
│   └── src/
│       ├── lib.rs           C ABI + heap allocator + panic
│       ├── radio.rs         chip-agnostic: executor, runtime GATT, advertise loop
│       └── chip/<soc>.rs    per-chip MPSL/SDC + interrupt bring-up
└── examples/<feature>/      example apps (gatt_server, gatt_client, l2cap_*)
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
- **west**, a recent **Zephyr SDK** (0.16+) (or a `gnuarmemb` arm-none-eabi toolchain), CMake, Ninja, Python.
- **Rust 1.92** (1.90 miscompiles nrf-sdc): `rustup toolchain install 1.92`,
  `rustup target add thumbv8m.main-none-eabi thumbv7em-none-eabihf`.
- **LLVM/clang** (for the nrf-sdc/nrf-mpsl bindgen step).

### 2. Examples are standalone west apps (no `-DZEPHYR_EXTRA_MODULES`)
The [`examples/`](examples/) are organized **by feature** (`gatt_server`,
`gatt_client`, `l2cap_peripheral`, `l2cap_central`) and each builds for **any
supported board**. Every example is an independent application with its own
`west.yml` that pulls **Zephyr + the `zephyr-runtime-ble` module via git**; west
auto-discovers runtime-ble as a Zephyr module (it has `zephyr/module.yml`), so
nothing is passed on the command line. See [`examples/README.md`](examples/README.md).

Use an example as its own git repo (e.g. copy `examples/gatt_server` out), then:
```sh
west init -l gatt_server   # the example dir is the manifest repo
west update                # clones the pinned Zephyr + zephyr-runtime-ble (+ its prebuilt libs)
west zephyr-export
west build -p always -b xiao_nrf54l15/nrf54l15/cpuapp gatt_server
west flash
```
> Board per chip: see the table above (e.g. `nrf54l15dk/nrf54l10/cpuapp`, `nrf52840dk/nrf52840`).
> Board-specific config (nRF52 hard-float etc.) is applied from the example's `boards/<board>.conf`.
> With a `gnuarmemb` toolchain add `-- -DZEPHYR_TOOLCHAIN_VARIANT=gnuarmemb -DGNUARMEMB_TOOLCHAIN_PATH=<dir>`.

### 3. (Re)building the per-chip staticlib
The prebuilt `lib/<chip>/libruntime_ble.a` is committed (so `west update` brings it). Rebuild it
only after editing `rust/` — see [`rust/README.md`](rust/README.md) for the per-platform
(Linux / macOS / Windows) cargo + bindgen recipe.

## Try it
Scan with the **nRF Connect** mobile app for `RUNTIME-BLE`, connect, find the
example's custom vendor service (`e54c0001-…`), enable notifications on TX
(`e54c0003`), and write bytes to RX (`e54c0002`) — they are echoed back on TX.

## API — everything is configured from C
The GATT layout and the advertising/GAP parameters are all set in
`runtime_ble_config_t` — no Rust rebuild needed to define your own services.
```c
/* Declare your GATT (or leave services NULL for a built-in NUS). */
static const runtime_ble_char_def_t chrs[] = {
    { rx_uuid, 16, RUNTIME_BLE_PROP_WRITE | RUNTIME_BLE_PROP_WRITE_NR, 244 },
    { tx_uuid, 16, RUNTIME_BLE_PROP_NOTIFY, 244 },
};
static const runtime_ble_service_def_t svcs[] = {
    { svc_uuid, 16, chrs, 2 },
};
static const runtime_ble_config_t cfg = {
    .device_name = "RUNTIME-BLE", .manufacturer_id = 0xFFFF,
    .adv_interval_min_ms = 30, .adv_interval_max_ms = 60,
    .services = svcs, .num_services = 1,
    .callbacks = { .on_write = on_write, .on_connected = on_conn, ... },
};
runtime_ble_init(&cfg);
runtime_ble_load();              // bring radio up + advertise
runtime_ble_notify(1, buf, n);   // notify characteristic #1 (TX)
runtime_ble_unload();            // tear down, free session RAM
```
Characteristics are addressed by **flat index** (declaration order). Callbacks:
`on_connected`, `on_disconnected`, `on_write(chr, …)` (or `on_data` for the
built-in NUS RX), `on_log`. They run on the BLE thread — keep them short.

## Roles: peripheral (default) and central
By default the runtime is a **peripheral** (advertise + GATT server, above). It
can also be a **central / GATT client** — build the central-capable lib
(`CONFIG_RUNTIME_BLE_CENTRAL=y`, links `libruntime_ble_central.a`) and set
`config.role = RUNTIME_BLE_ROLE_CENTRAL`:
```c
runtime_ble_connect(addr);              // or config.peer_address to auto-connect
runtime_ble_client_discover(svc, 16);   // -> on_discovered(handle, …)
runtime_ble_client_subscribe(handle);   // -> on_notification(handle, …)
runtime_ble_client_write(handle, buf, n);
runtime_ble_client_read(handle);        // -> on_read(handle, …)
```
See [`examples/gatt_client/`](examples/gatt_client/) (HW-verified
against the peripheral echo example). The role is feature-gated so peripheral-
only apps stay on the lean default lib (see [`rust/README.md`](rust/README.md)).

## Adding a chip
1. Add a `<chip> = ["_radio", "embassy-nrf/<chip>", "nrf-sdc/<chip>"]` feature
   in `rust/Cargo.toml`.
2. Reuse a family bring-up (`rust/src/chip/nrf54l.rs` or `nrf52.rs`) or add one
   (`Irqs` + `runtime_irq_*` shims + `build_sdc` + `run`).
3. Add the matching IRQ branch in `glue/glue.c` and the SoC case in `CMakeLists.txt`.
4. Build the staticlib ([`rust/README.md`](rust/README.md)); the feature examples build for it once its prebuilt lib exists.
