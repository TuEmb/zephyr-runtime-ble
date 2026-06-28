cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_nrf54l15_central)

# Central / GATT-client demo. Pair it with the peripheral echo example
# (examples/nrf54l15) and set PEER_* in src/main.c to that board's address.
target_sources(app PRIVATE src/main.c)
