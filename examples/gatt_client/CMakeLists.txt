cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_gatt_client)

# Central / GATT-client demo. Pair it with the gatt_server example and set
# PEER_* in src/main.c to that board's address. Build for any supported board:
#   west build -b <board> gatt_client
target_sources(app PRIVATE src/main.c)
