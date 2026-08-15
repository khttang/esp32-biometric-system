idf_component_register(
    SRCS "biometrics_wrapper.cpp"
    INCLUDE_DIRS "include"
    REQUIRES 
        main
        cxx 
        esp-dl 
        driver 
        esp_driver_i2c
        esp_eth 
        esp_netif
        esp_lcd
        esp_lcd_dsi
        esp_lcd_hx8394
        esp_lcd_touch
        esp_lcd_touch_gt911
        esp_hw_support 
        esp_pm 
        hal
        lvgl
        esp32_p4_nano
        esp_video

)