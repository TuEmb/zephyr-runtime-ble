cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_l2cap_central)

# Pair with the l2cap_peripheral example; set PEER_* in src/main.c to its
# address. Build for any supported board: west build -b <board> l2cap_central
target_sources(app PRIVATE src/main.c)
