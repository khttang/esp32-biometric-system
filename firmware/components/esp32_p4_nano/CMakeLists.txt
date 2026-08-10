if("${IDF_VERSION_MAJOR}.${IDF_VERSION_MINOR}" VERSION_GREATER_EQUAL "5.5")
    set(REQ esp_driver_gpio esp_driver_i2s esp_driver_sdmmc esp_driver_sdspi esp_driver_i2c)
    set(PRIV_REQ esp_driver_spi esp_driver_ledc)
else()
    set(REQ driver)
    set(PRIV_REQ "")
endif()

if(${IDF_VERSION_MAJOR} LESS 6)
    list(APPEND PRIV_REQ usb)
endif()

idf_component_register(
    SRCS "esp32_p4_nano.c"
    INCLUDE_DIRS "include"
    PRIV_INCLUDE_DIRS "priv_include"
    REQUIRES ${REQ} fatfs esp_codec_dev
    PRIV_REQUIRES ${PRIV_REQ} esp_lcd esp_lcd_dsi esp_lcd_hx8394 esp_lcd_touch esp_lcd_touch_gt911 spiffs esp_psram hal
)

target_compile_options(${COMPONENT_LIB} PUBLIC -DBSP_CONFIG_NO_GRAPHIC_LIB=1)