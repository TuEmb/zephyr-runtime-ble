cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(runtime_ble_test)

# runtime-ble is discovered as a Zephyr module via west (see west.yml), so no
# -DZEPHYR_EXTRA_MODULES is needed. Build + flash + run, e.g.:
#   west build -b xiao_nrf54l15/nrf54l15/cpuapp test
#   west flash
# then read the ztest output on the console.
target_sources(app PRIVATE
  src/test_support.c
  src/test_lifecycle.c
  src/test_edge.c
  src/test_stress.c
)
