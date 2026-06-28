cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_gatt_server)

# runtime-ble is discovered as a Zephyr module via west (see west.yml), so no
# -DZEPHYR_EXTRA_MODULES is needed. Build for any supported board, e.g.:
#   west build -b xiao_nrf54l15/nrf54l15/cpuapp gatt_server
#   west build -b nrf52840dk/nrf52840 gatt_server
target_sources(app PRIVATE src/main.c)
