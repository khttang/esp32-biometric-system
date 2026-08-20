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
        esp_hw_support 
        esp_pm 
        hal
        lvgl
        # Managed components
        espressif__esp-dl 
        espressif__esp_video
        espressif__esp_lvgl_port
        espressif__esp_lcd_touch
        espressif__esp_lcd_touch_gt911
        waveshare__esp_lcd_hx8394

)

target_compile_options(${COMPONENT_LIB} PRIVATE -msmall-data-limit=0)