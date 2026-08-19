idf_component_register(
    SRCS "biometrics_wrapper.cpp"
    INCLUDE_DIRS "include" "."
    REQUIRES 
        cxx 
        app_update
        esp_https_ota
        esp-dl 
        driver 
        esp_driver_i2c
        esp_eth 
        esp_netif
        esp_lcd
        esp_lcd_hx8394
        esp_lcd_touch
        esp_lcd_touch_gt911
        esp_hw_support 
        esp_pm 
        hal
        lvgl
        esp_lvgl_port
        esp_video

)

target_compile_options(${COMPONENT_LIB} PRIVATE -msmall-data-limit=0)