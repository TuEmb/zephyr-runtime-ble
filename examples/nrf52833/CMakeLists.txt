cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_nrf52833)

# runtime-ble is discovered as a Zephyr module via west (see ../../west.yml),
# so no -DZEPHYR_EXTRA_MODULES is needed. Build, e.g.:
#   west build -b <board> zephyr-runtime-ble/examples/nrf52833
target_sources(app PRIVATE src/main.c)
