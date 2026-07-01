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

## Quick Start

The fastest path: leave `services = NULL` to get a built-in **Nordic UART
Service** (RX write `6e400002`, TX notify `6e400003`). Bytes a central writes
arrive on `on_data`; send back with `runtime_ble_send()`.

```c
#include "runtime_ble.h"

static void on_data(const uint8_t *data, size_t len, void *user) {
    runtime_ble_send(data, len);          // echo it back over TX notify
}

void main(void) {
    static const runtime_ble_config_t cfg = {
        .device_name = "RUNTIME-BLE",
        .services = NULL,                 // NULL -> built-in NUS
        .callbacks = { .on_data = on_data },
    };
    runtime_ble_init(&cfg);
    runtime_ble_load();                   // radio up + advertising
}
```

Minimal `prj.conf` (the four runtime-ble essentials — see [Integration](#integration--config) for why):

```ini
CONFIG_RUNTIME_BLE=y
CONFIG_BT=n                              # trouble owns the radio
CONFIG_DYNAMIC_THREAD_PREFER_ALLOC=y     # stack-from-heap strategy (choice symbol)
CONFIG_HEAP_MEM_POOL_SIZE=65536          # one session ~38 KB; see heap table
```

Build and flash an example (full steps in [Build](#build)):

```sh
cd examples/gatt_server && west init -l . && west update && west zephyr-export
west build -p always -b xiao_nrf54l15/nrf54l15/cpuapp . && west flash
```

Scan with **nRF Connect** for `RUNTIME-BLE`, connect, and the echo round-trips.
For user-defined GATT, central/client, L2CAP, security and the full call list,
see **[API.md](API.md)**.

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

## Integration / config

runtime-ble needs four coupled Kconfig options set in the **app's** `prj.conf`
(they cannot all be defaulted by the module — `DYNAMIC_THREAD_PREFER_ALLOC` is a
choice symbol that must be resolved by the application):

| Option | Why |
|---|---|
| `CONFIG_RUNTIME_BLE=y` | enable the module |
| `CONFIG_BT=n` | trouble owns the radio — the Zephyr BT stack + MPSL must be off |
| `CONFIG_DYNAMIC_THREAD_PREFER_ALLOC=y` | the runtime allocates its thread stack from the heap on load |
| `CONFIG_HEAP_MEM_POOL_SIZE=…` | one session is allocated on load; size per the table below |

Heap sizing is the one number to get right. Start from this **feature → minimum
heap** guide (one loaded session; round up and leave headroom for your app):

| Configuration | Suggested `CONFIG_HEAP_MEM_POOL_SIZE` |
|---|---|
| Peripheral, NUS or small GATT | `65536` (64 KB) |
| Peripheral, large GATT / many characteristics | `98304` (96 KB) |
| Central / GATT client | `98304` (96 KB) |
| Dual role (server + client) | `131072` (128 KB) |
| + L2CAP CoC | add ~`16384` (16 KB) |

A too-small heap shows up as a failed `runtime_ble_load()` (the session alloc
fails); bump the pool and retry. See the per-example `prj.conf` for working
values, and `CONFIG_RUNTIME_BLE_THREAD_STACK_SIZE` / `_PRIORITY` in
[`Kconfig`](Kconfig) to tune the BLE thread.

## Optimizing RAM

A loaded session is heap-allocated and **fully freed on `runtime_ble_unload()`**,
so it costs ~no RAM until loaded. **The default build is minimum RAM** — a
legacy-advertising peripheral + GATT server with small buffers — and you opt into
more only when you need it. The loaded footprint has three big parts: the **dynamic
thread stack**, the **SoftDevice Controller pool**, and the host/server state.

Measured on nRF54L15 (custom-GATT peripheral, `CONFIG_SYS_HEAP_RUNTIME_STATS`
around load):

| Configuration | Heap used on load |
|---|---|
| **Default (minimum)** — legacy adv, 128-byte values, 20 KB stack | **~30.7 KB** |
| `CONFIG_RUNTIME_BLE_PERF=y` — full features, 244-byte values, 32 KB stack | ~47.5 KB |
| `CONFIG_RUNTIME_BLE_CENTRAL=y` (GATT client) | ~48 KB |
| Dual (server + client), 64 KB stack | ~95 KB |

Opt into performance / capability as needed:
1. **`CONFIG_RUNTIME_BLE_PERF=y`** — the full peripheral lib: extended/periodic
   advertising, Coded PHY, subrating, frame-space, LE extended features, 251-byte
   buffers, 244-byte characteristic values, 64-entry attribute table. Use it for
   throughput / big GATT / advanced advertising. (central/l2cap already include
   the full feature set.)
2. **`CONFIG_RUNTIME_BLE_THREAD_STACK_SIZE`** — the largest single part. Defaults
   to 20480 on the minimum build (measured ~15 KB high-water) and 32768 on the
   full builds. Raise it for heavy on-thread work (pairing, big notifications,
   significant callbacks); each 1 KB is 1 KB of heap.
3. **`config.sdc_disable`** (runtime bitmask, `RUNTIME_BLE_SDC_DISABLE_*`) — on the
   full builds, trims the *active* controller features without switching libs
   (power / interop). The pool is compile-time, so unlike PERF↔default it does not
   resize the heap.

The library logs `"[runtime-ble] sdc mem: need <n> have <pool>"` at load (via your
`on_log`), so you can see the controller's exact memory need and right-size a custom
lib build (`SDC_MEM` in `rust/src/chip/`).

## Features

All of these are configured from C through `runtime_ble_config_t` — **no Rust
rebuild** to define your own services. See **[API.md](API.md)** for the full
call list and config fields.

- **User-defined GATT** — declare services/characteristics/descriptors, choose
  properties and per-attribute security; notify or indicate by characteristic.
- **Advertising / GAP** — automatic builder, or raw AD payload for beacons;
  directed reconnect, channel-map and filter-policy control.
- **Live-link updates** — request PHY, data length, connection params, frame
  spacing, and connection-rate/subrate on an active link.
- **Security** — per-attribute encryption/authentication, persistent bonding
  with app-owned storage, configurable IO capability, and OOB pairing.
- **Central / GATT client** — scan, connect, discover, read/write/subscribe;
  build with `CONFIG_RUNTIME_BLE_CENTRAL=y` and `role = RUNTIME_BLE_ROLE_CENTRAL`.
  See [`examples/gatt_client/`](examples/gatt_client/).
- **Dual role** — server *and* client at once with
  `role = RUNTIME_BLE_ROLE_DUAL`. See [`examples/dual/`](examples/dual/).
- **L2CAP CoC** — connection-oriented channels with `CONFIG_RUNTIME_BLE_L2CAP=y`
  and `config.l2cap_psm`. See [`examples/l2cap_peripheral/`](examples/l2cap_peripheral/).

The role and L2CAP are feature-gated into separate prebuilt libs, so a
peripheral-only app stays on the lean default `libruntime_ble.a` (see
[`examples/README.md`](examples/README.md) for the lib-variant matrix).

## Adding a chip
1. Add a `<chip> = ["_radio", "embassy-nrf/<chip>", "nrf-sdc/<chip>"]` feature
   in `rust/Cargo.toml`.
2. Reuse a family bring-up (`rust/src/chip/nrf54l.rs` or `nrf52.rs`) or add one
   (`Irqs` + `runtime_irq_*` shims + `build_sdc` + `run`).
3. Add the matching IRQ branch in `glue/glue.c` and the SoC case in `CMakeLists.txt`.
4. Build the staticlib ([`rust/README.md`](rust/README.md)); the feature examples build for it once its prebuilt lib exists.
