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
        esp_hw_support 
        esp_pm 
        hal
        esp32_p4_nano
        esp_video
)