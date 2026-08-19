idf_component_register(
    SRCS "biometrics_wrapper.cpp"
    INCLUDE_DIRS "include" "."
    REQUIRES 
        cxx 
        app_update
        esp_https_ota
        driver 
        esp_driver_i2c
        esp_eth 
        esp_netif
        esp_lcd
        esp_hw_support 
        esp_pm 
        hal
        lvgl
        esp-dl
        # Managed components with namespace prefixes 
        espressif__esp_video
        espressif__esp_lvgl_port
        espressif__esp_lcd_touch
        espressif__esp_lcd_touch_gt911
        waveshare__esp_lcd_hx8394
)

target_compile_options(${COMPONENT_LIB} PRIVATE -msmall-data-limit=0)