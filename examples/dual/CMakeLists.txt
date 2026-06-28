cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_dual)

# Dual GAP role (server + client). Pair the client side with gatt_server; set
# PEER_* in src/main.c. Build for any supported board (a central-capable lib
# must exist for it): west build -b <board> dual
target_sources(app PRIVATE src/main.c)
