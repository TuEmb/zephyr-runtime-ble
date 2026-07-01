# runtime-ble — Zephyr module: the trouble (Rust) BLE host + nRF SoftDevice
# Controller, packaged as a per-chip prebuilt staticlib (lib/<chip>/libruntime_ble.a)
# plus the Zephyr glue (glue/glue.c).
#
# The library OWNS the radio, so the application MUST build with CONFIG_BT=n
# (enforced by `depends on !BT` in Kconfig). Register it as a Zephyr module:
#   west build -b <board> <app> -- -DZEPHYR_EXTRA_MODULES=<abs>/runtime-ble

if(CONFIG_RUNTIME_BLE)

set(RTBLE_DIR ${CMAKE_CURRENT_LIST_DIR})

# Select the prebuilt staticlib for the target SoC.
if(CONFIG_SOC_NRF54L15)
  set(RTBLE_CHIP nrf54l15)
elseif(CONFIG_SOC_NRF54L10)
  set(RTBLE_CHIP nrf54l10)
elseif(CONFIG_SOC_NRF54L05)
  set(RTBLE_CHIP nrf54l05)
elseif(CONFIG_SOC_NRF54LM20A OR CONFIG_SOC_NRF54LM20)
  set(RTBLE_CHIP nrf54lm20)
elseif(CONFIG_SOC_NRF52840)
  set(RTBLE_CHIP nrf52840)
elseif(CONFIG_SOC_NRF52833)
  set(RTBLE_CHIP nrf52833)
elseif(CONFIG_SOC_NRF52832)
  set(RTBLE_CHIP nrf52832)
else()
  message(FATAL_ERROR "runtime-ble: no prebuilt staticlib for this SoC. "
                      "Add a chip module in rust/src/chip/ and build lib/<chip>/libruntime_ble.a.")
endif()

# Select the staticlib variant. The DEFAULT (no suffix) is the minimum-RAM
# peripheral + GATT server. CONFIG_RUNTIME_BLE_PERF selects the full peripheral
# (large buffers + all advertising modes); central/l2cap add their roles (and the
# full feature set). perf is peripheral-only, so it is a standalone variant.
set(RTBLE_VARIANT "")
if(CONFIG_RUNTIME_BLE_PERF)
  set(RTBLE_VARIANT "_perf")
else()
  if(CONFIG_RUNTIME_BLE_CENTRAL)
    string(APPEND RTBLE_VARIANT "_central")
  endif()
  if(CONFIG_RUNTIME_BLE_L2CAP)
    string(APPEND RTBLE_VARIANT "_l2cap")
  endif()
endif()

# Resolve the staticlib: prefer a blob fetched by `west blobs fetch zephyr-runtime-ble`
# (zephyr/blobs/<chip>/), else the copy committed under lib/<chip>/ (the fallback
# during development / if blobs are not used). See zephyr/module.yml `blobs:`.
set(RTBLE_NAME libruntime_ble${RTBLE_VARIANT}.a)
set(RTBLE_BLOB ${RTBLE_DIR}/zephyr/blobs/${RTBLE_CHIP}/${RTBLE_NAME})
set(RTBLE_COMMITTED ${RTBLE_DIR}/lib/${RTBLE_CHIP}/${RTBLE_NAME})
if(EXISTS ${RTBLE_BLOB})
  set(RTBLE_LIB ${RTBLE_BLOB})
elseif(EXISTS ${RTBLE_COMMITTED})
  set(RTBLE_LIB ${RTBLE_COMMITTED})
else()
  message(FATAL_ERROR "runtime-ble: ${RTBLE_NAME} for ${RTBLE_CHIP} not found. "
                      "Run `west blobs fetch zephyr-runtime-ble`, or build the Rust "
                      "staticlib for these roles (Cargo features '${RTBLE_VARIANT}') "
                      "— see rust/README.md.")
endif()

# Prebuilt Rust staticlib (owns the radio; build the app with CONFIG_BT=n).
add_library(runtime_ble_imported STATIC IMPORTED GLOBAL)
set_target_properties(runtime_ble_imported PROPERTIES IMPORTED_LOCATION ${RTBLE_LIB})

# Zephyr glue (compiled into the image). glue.c and the .a are mutually
# dependent; both land in the Zephyr link group so the circular refs resolve.
zephyr_library_named(runtime_ble_glue)
zephyr_library_include_directories(${RTBLE_DIR}/include)
zephyr_library_sources(${RTBLE_DIR}/glue/glue.c)

# Public API header, visible to the application.
zephyr_include_directories(${RTBLE_DIR}/include)

# Pull the staticlib into the final link.
zephyr_link_libraries(runtime_ble_imported)

endif() # CONFIG_RUNTIME_BLE
