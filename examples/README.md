# runtime-ble examples

Organized **by feature**, not by board — each example is a standalone west app
that builds for **any supported board**. Pick the board on the command line; the
module's `CMakeLists.txt` selects the matching prebuilt staticlib for that SoC,
and `boards/<board>.conf` overlays apply any board-specific tweaks.

| Example | Feature | Lib variant needed |
|---|---|---|
| [`gatt_server/`](gatt_server/) | Peripheral + user-defined GATT (echo) | `libruntime_ble.a` (default) |
| [`gatt_client/`](gatt_client/) | Central: scan/connect + discover/read/write/subscribe | `libruntime_ble_central.a` (`CONFIG_RUNTIME_BLE_CENTRAL=y`) |
| [`l2cap_peripheral/`](l2cap_peripheral/) | L2CAP CoC echo server | `libruntime_ble_l2cap.a` (`CONFIG_RUNTIME_BLE_L2CAP=y`) |
| [`l2cap_central/`](l2cap_central/) | Central that opens an L2CAP channel | `libruntime_ble_central_l2cap.a` (both) |

The default lib (peripheral + GATT server) is committed for all chips, so
`gatt_server` builds for every board out of the box. The other examples need a
role-specific lib variant — committed for nRF54L15, build it for other chips
with the matching Cargo feature (see [`../rust/README.md`](../rust/README.md)).

## Build (any board)

```sh
cd gatt_server            # or gatt_client / l2cap_peripheral / l2cap_central
west init -l .
west update               # clones Zephyr + zephyr-runtime-ble
west zephyr-export
west build -b <board> .
west flash
```

Boards (qualifier → `boards/<board>.conf` overlay):

| Chip | `-b <board>` | Overlay |
|---|---|---|
| nRF54L15 | `xiao_nrf54l15/nrf54l15/cpuapp`, `nrf54l15dk/nrf54l15/cpuapp` | — (soft-float) |
| nRF54L10 / L05 | `nrf54l15dk/nrf54l10/cpuapp`, `nrf54l15dk/nrf54l05/cpuapp` | — |
| nRF54LM20 | `nrf54lm20dk/nrf54lm20/cpuapp` | — |
| nRF52840 | `nrf52840dk/nrf52840` | `boards/nrf52840dk_nrf52840.conf` (FPU) |
| nRF52833 | `nrf52833dk/nrf52833` | `boards/nrf52833dk_nrf52833.conf` (FPU) |
| nRF52832 | `nrf52dk/nrf52832` | `boards/nrf52dk_nrf52832.conf` (FPU + smaller heap) |

## Two-board demos (HW-verified on nRF54L15)

- **GATT**: flash `gatt_server` on one board, `gatt_client` on another (set
  `PEER_*` in `gatt_client/src/main.c` to the server's address) — the client
  discovers, subscribes, writes, and receives the echo notification.
- **L2CAP**: flash `l2cap_peripheral` on one board, `l2cap_central` on another
  (set `PEER_*`) — the central opens a channel, sends, and receives the echo.
