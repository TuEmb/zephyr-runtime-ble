cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_nrf52840)

# The runtime-ble module is registered at build time:
#   west build -b xiao_nrf54l15/nrf54l15/cpuapp <this-dir> \
#       -- -DZEPHYR_EXTRA_MODULES=<abs>/runtime-ble
target_sources(app PRIVATE src/main.c)
