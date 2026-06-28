# runtime-ble on-target tests

[ztest](https://docs.zephyrproject.org/latest/develop/test/ztest.html) suites
that exercise the loadable BLE runtime on real hardware (the radio bring-up
needs the SoC, so this is **not** a `native_sim` test). Validated on the
nRF54L15; the same app builds for any supported board.

## Suites & cases

### `runtime_ble_lifecycle` — the normal paths
| Test | Checks |
|---|---|
| `test_addr_is_static_random` | `runtime_ble_addr()` returns a BLE static-random address (top 2 bits set) |
| `test_load_then_unload` | `load()` → advertise → `unload()` all return OK |
| `test_double_load_is_idempotent` | a second `load()` with no unload is a no-op OK (one session) |
| `test_reload_after_unload` | a session can be torn down and brought back up (load/unload twice) |

### `runtime_ble_edge` — misuse must fail gracefully, never crash
| Test | Checks |
|---|---|
| `test_init_null_is_rejected` | `init(NULL)` → `ERR_INVALID` |
| `test_unload_without_load` | `unload()` with nothing loaded → OK (no-op) |
| `test_double_unload` | second `unload()` → OK (no-op) |
| `test_send_argument_validation` | NULL / zero-length / oversized send → `ERR_INVALID` |
| `test_send_before_load_queues_then_full` | first send queues (OK); second → `ERR_NO_MEM` (slot full) |
| `test_notify_unknown_characteristic` | notify to a non-existent characteristic index → OK, silently dropped, no crash |

> **Precondition:** `load()` reads the config from `init()`. Calling `load()`
> with no prior `init()` faults, so it is a documented precondition (not a
> supported path) and is not exercised here — every suite inits in `setup`.

### `runtime_ble_stress` — no crash, no leak
| Test | Checks |
|---|---|
| `test_repeated_load_unload_no_leak` | 10× load/unload; system-heap free bytes return to baseline (≤128 B slop) |

It prints the measured per-cycle delta, e.g.:
```
[stress] heap free: before=98304 after=98304  leaked=0 over 10 cycles (0 B/cycle)
```

## Run it

```sh
west init -l test
west update                  # clones Zephyr v4.4.1 + zephyr-runtime-ble
west zephyr-export
west build -p always -b xiao_nrf54l15/nrf54l15/cpuapp test
west flash
```
Then read the console (UART/RTT) for the ztest `PASS`/`FAIL` lines and the
`PROJECT EXECUTION SUCCESSFUL` / `FAILED` summary.

> Board per chip: see the root README's board table. With a `gnuarmemb`
> toolchain add `-- -DZEPHYR_TOOLCHAIN_VARIANT=gnuarmemb -DGNUARMEMB_TOOLCHAIN_PATH=<dir>`.
