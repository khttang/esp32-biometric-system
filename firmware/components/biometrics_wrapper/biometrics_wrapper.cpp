#include "driver/i2c_master.h"
#include "bindings.h"

#include <stdio.h>
#include <cstring>
#include <memory>
#include <cmath>

#include "esp_log.h"
#include "driver/gpio.h"
#include "driver/i2s_std.h"
#include "driver/isp_core.h"
#include "driver/ledc.h"
#include "esp_err.h"
#include "esp_check.h"
#include "esp_ldo_regulator.h"
#include "esp_cam_ctlr.h"
#include "esp_cam_ctlr_csi.h"
#include "esp_cam_ctlr_types.h"
#include "esp_cam_sensor.h"
#include "esp_lcd_mipi_dsi.h"
#include "esp_lcd_panel_ops.h"
#include "esp_lcd_panel_io.h"
#include "esp_lcd_panel_vendor.h"
#include "esp_heap_caps.h"
#include "esp_eth.h"
#include "esp_eth_mac_esp.h"
#include "esp_eth_phy_802_3.h"
#include "esp_ota_ops.h"
#include "esp_https_ota.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "hal/color_types.h"

// Core esp-dl Headers
#include "dl_model_base.hpp"
#include "dl_tensor_base.hpp"

#include "biometrics_wrapper.h"

// -----------------------------------------------------------------------------
// Centralized Hardware Pinout (Waveshare ESP32-P4-NANO Schematic)
// -----------------------------------------------------------------------------
namespace BoardPins {
    namespace I2C {
        constexpr gpio_num_t SDA = GPIO_NUM_7;
        constexpr gpio_num_t SCL = GPIO_NUM_8;
    }
    namespace Display {
        constexpr gpio_num_t PWM_BL = GPIO_NUM_6;
    }
    namespace Ethernet {
        constexpr gpio_num_t MDC   = GPIO_NUM_31;
        constexpr gpio_num_t MDIO  = GPIO_NUM_52;
        constexpr gpio_num_t CLK   = GPIO_NUM_50;
        constexpr gpio_num_t RESET = GPIO_NUM_53;
        
        namespace RMII {
            constexpr gpio_num_t TX_EN  = GPIO_NUM_49;
            constexpr gpio_num_t TXD0   = GPIO_NUM_34;
            constexpr gpio_num_t TXD1   = GPIO_NUM_35;
            constexpr gpio_num_t CRS_DV = GPIO_NUM_28;
            constexpr gpio_num_t RXD0   = GPIO_NUM_29;
            constexpr gpio_num_t RXD1   = GPIO_NUM_30;
        }
    }
    namespace System {
        constexpr gpio_num_t ADMIN_BTN = GPIO_NUM_0;
    }
}

namespace BoardAddr {
    constexpr uint8_t TCA9554_EXPANDER   = 0x18;
    constexpr uint8_t DISPLAY_BACKLIGHT  = 0x45;
    constexpr uint8_t OV5647_SCCB         = 0x36;
    constexpr uint8_t GT911_TOUCH         = 0x5D;
}

#define TAG_HW      "p4_hardware"
#define TAG_ETH     "p4_ethernet"
#define TAG_CAM     "p4_camera"
#define TAG_SCAN    "i2c_scanner"
#define TAG_OTA     "p4_ota"
#define TAG_FACENET "ESP_DL_FACENET"
#define TAG_I2S     "I2S_WRAPPER"
#define TAG_LCD     "p4_lcd"

// Global Handles
static i2c_master_bus_handle_t s_i2c_bus = NULL;
static i2c_master_dev_handle_t s_tca9554_handle = NULL;
static i2c_master_dev_handle_t s_cam_sccb_handle = NULL;
static i2c_master_dev_handle_t s_backlight_handle = NULL;
static i2c_master_dev_handle_t s_gt911_handle = NULL;

static esp_ldo_channel_handle_t s_ldo_vo3_handle = NULL;
static esp_lcd_dsi_bus_handle_t s_dsi_bus_handle = NULL;
static esp_lcd_panel_handle_t s_lcd_panel = NULL;
static esp_lcd_panel_io_handle_t s_lcd_io = NULL;

static esp_cam_ctlr_handle_t s_csi_cam_handle = NULL;
static uint8_t *s_cam_frame_buffer = NULL;
static size_t s_cam_fb_size = 0;

static i2s_chan_handle_t g_i2s_tx_handle = NULL;
static i2s_chan_handle_t g_i2s_rx_handle = NULL;

static bool s_hardware_initialized = false;
static dl::Model *g_mobilefacenet_model = NULL;

static esp_err_t reset_hx8394_display(void) {
    if (s_tca9554_handle == NULL) {
        ESP_LOGE(TAG_LCD, "Cannot reset HX8394: TCA9554 handle is NULL!");
        return ESP_ERR_INVALID_STATE;
    }

    ESP_LOGI(TAG_LCD, "Asserting HX8394 hardware reset via TCA9554 (P0 LOW)...");
    
    // Drive P0 LOW (0x0E = 0000 1110)
    uint8_t rst_low_cmd[2] = {0x01, 0x0E};
    esp_err_t err = i2c_master_transmit(s_tca9554_handle, rst_low_cmd, sizeof(rst_low_cmd), 1000);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "TCA9554 transmit failed (LOW): 0x%x", err);
        return err;
    }

    vTaskDelay(pdMS_TO_TICKS(20)); // Hold RESET LOW 20ms

    ESP_LOGI(TAG_LCD, "De-asserting HX8394 hardware reset (P0 HIGH)...");
    
    // Drive P0 HIGH (0x0F = 0000 1111)
    uint8_t rst_high_cmd[2] = {0x01, 0x0F};
    err = i2c_master_transmit(s_tca9554_handle, rst_high_cmd, sizeof(rst_high_cmd), 1000);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "TCA9554 transmit failed (HIGH): 0x%x", err);
        return err;
    }

    vTaskDelay(pdMS_TO_TICKS(120)); // Wait 120ms for HX8394 power-on state machine
    return ESP_OK;
}

// -----------------------------------------------------------------------------
// Ethernet Event Handlers
// -----------------------------------------------------------------------------
static void eth_event_handler(void *arg, esp_event_base_t event_base,
                              int32_t event_id, void *event_data) {
    uint8_t mac_addr[6] = {0};
    esp_eth_handle_t eth_handle = *(esp_eth_handle_t *)event_data;

    switch (event_id) {
    case ETHERNET_EVENT_CONNECTED:
        esp_eth_ioctl(eth_handle, ETH_CMD_G_MAC_ADDR, mac_addr);
        ESP_LOGI(TAG_ETH, "Ethernet Link Up. MAC: %02x:%02x:%02x:%02x:%02x:%02x",
                 mac_addr[0], mac_addr[1], mac_addr[2],
                 mac_addr[3], mac_addr[4], mac_addr[5]);
        break;
    case ETHERNET_EVENT_DISCONNECTED:
        ESP_LOGI(TAG_ETH, "Ethernet Link Down");
        break;
    case ETHERNET_EVENT_START:
        ESP_LOGI(TAG_ETH, "Ethernet Driver Started");
        break;
    case ETHERNET_EVENT_STOP:
        ESP_LOGI(TAG_ETH, "Ethernet Driver Stopped");
        break;
    default:
        break;
    }
}

static void got_ip_event_handler(void *arg, esp_event_base_t event_base,
                                 int32_t event_id, void *event_data) {
    ip_event_got_ip_t *event = (ip_event_got_ip_t *)event_data;
    const esp_netif_ip_info_t *ip_info = &event->ip_info;

    ESP_LOGI(TAG_ETH, "Ethernet Got IP Address:");
    ESP_LOGI(TAG_ETH, "  IP     : " IPSTR, IP2STR(&ip_info->ip));
    ESP_LOGI(TAG_ETH, "  Netmask: " IPSTR, IP2STR(&ip_info->netmask));
    ESP_LOGI(TAG_ETH, "  GW     : " IPSTR, IP2STR(&ip_info->gw));
}

int init_i2s_tx_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int dout_gpio) {
    return init_i2s_duplex_c(sample_rate, bclk_gpio, ws_gpio, -1, dout_gpio);
}

// -----------------------------------------------------------------------------
// Audio Peripheral Drivers
// -----------------------------------------------------------------------------
int init_i2s_mic_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio) {
    return init_i2s_duplex_c(sample_rate, bclk_gpio, ws_gpio, din_gpio, -1);
}

int init_i2s_duplex_c(
    uint32_t sample_rate, 
    int bclk_gpio, 
    int ws_gpio, 
    int din_gpio, 
    int dout_gpio
) {
    // 1. Teardown previous handles if re-initializing
    if (g_i2s_tx_handle) {
        i2s_channel_disable(g_i2s_tx_handle);
        i2s_del_channel(g_i2s_tx_handle);
        g_i2s_tx_handle = NULL;
    }
    if (g_i2s_rx_handle) {
        i2s_channel_disable(g_i2s_rx_handle);
        i2s_del_channel(g_i2s_rx_handle);
        g_i2s_rx_handle = NULL;
    }

    // 2. Determine which handles to allocate based on pin passed
    i2s_chan_config_t chan_cfg = I2S_CHANNEL_DEFAULT_CONFIG(I2S_NUM_0, I2S_ROLE_MASTER);
    
    i2s_chan_handle_t *p_tx = (dout_gpio >= 0) ? &g_i2s_tx_handle : NULL;
    i2s_chan_handle_t *p_rx = (din_gpio >= 0)  ? &g_i2s_rx_handle : NULL;

    esp_err_t ret = i2s_new_channel(&chan_cfg, p_tx, p_rx);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_I2S, "i2s_new_channel failed: 0x%x", ret);
        return (int)ret;
    }

    // 3. Configure and Enable TX Channel (if requested)
    if (g_i2s_tx_handle && dout_gpio >= 0) {
        i2s_std_config_t tx_cfg = {
            .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
            .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_MONO),
            .gpio_cfg = {
                .mclk = I2S_GPIO_UNUSED,
                .bclk = (gpio_num_t)bclk_gpio,
                .ws   = (gpio_num_t)ws_gpio,
                .dout = (gpio_num_t)dout_gpio,
                .din  = I2S_GPIO_UNUSED,
            },
        };
        ret = i2s_channel_init_std_mode(g_i2s_tx_handle, &tx_cfg);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_I2S, "TX init failed: 0x%x", ret);
            return (int)ret;
        }
        ret = i2s_channel_enable(g_i2s_tx_handle);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_I2S, "TX enable failed: 0x%x", ret);
            return (int)ret;
        }
        ESP_LOGI(TAG_I2S, "TX Channel enabled successfully");
    }

    // 4. Configure and Enable RX Channel (if requested)
    if (g_i2s_rx_handle && din_gpio >= 0) {
        i2s_std_config_t rx_cfg = {
            .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
            .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_MONO),
            .gpio_cfg = {
                .mclk = I2S_GPIO_UNUSED,
                .bclk = (gpio_num_t)bclk_gpio,
                .ws   = (gpio_num_t)ws_gpio,
                .dout = I2S_GPIO_UNUSED,
                .din  = (gpio_num_t)din_gpio,
            },
        };
        ret = i2s_channel_init_std_mode(g_i2s_rx_handle, &rx_cfg);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_I2S, "RX init failed: 0x%x", ret);
            return (int)ret;
        }
        ret = i2s_channel_enable(g_i2s_rx_handle);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_I2S, "RX enable failed: 0x%x", ret);
            return (int)ret;
        }
        ESP_LOGI(TAG_I2S, "RX Channel enabled successfully");
    }

    return 0;
}

int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms) {
    if (!g_i2s_rx_handle) return -1;
    size_t r_bytes = 0;
    esp_err_t ret = i2s_channel_read(g_i2s_rx_handle, out_buffer, samples_to_read * sizeof(int16_t), &r_bytes, pdMS_TO_TICKS(timeout_ms));
    if (bytes_read) *bytes_read = (uint32_t)r_bytes;
    return (int)ret;
}

int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms) {
    if (!g_i2s_tx_handle) return -1;
    size_t w_bytes = 0;
    esp_err_t ret = i2s_channel_write(g_i2s_tx_handle, buffer, sample_count * sizeof(int16_t), &w_bytes, pdMS_TO_TICKS(timeout_ms));
    return (int)ret;
}

// -----------------------------------------------------------------------------
// Power & Hardware Initialization
// -----------------------------------------------------------------------------
static esp_err_t init_internal_ldo(void) {
    if (s_ldo_vo3_handle != NULL) return ESP_OK;

    ESP_LOGI(TAG_HW, "Acquiring internal LDO Channel 3 (2.5V) for MIPI D-PHY...");
    esp_ldo_channel_config_t ldo_cfg = {};
    ldo_cfg.chan_id = 3;
    ldo_cfg.voltage_mv = 2500;

    esp_err_t ret = esp_ldo_acquire_channel(&ldo_cfg, &s_ldo_vo3_handle);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_HW, "Failed to acquire LDO VO3: 0x%x (%s)", ret, esp_err_to_name(ret));
        return ret;
    }

    ESP_LOGI(TAG_HW, "LDO_VO3 acquired successfully!");
    return ESP_OK;
}

void scan_i2c_bus(i2c_master_bus_handle_t bus_handle) {
    if (bus_handle == NULL) {
        ESP_LOGE(TAG_SCAN, "Cannot scan: I2C master bus handle is NULL!");
        return;
    }

    ESP_LOGI(TAG_SCAN, "==================================================");
    ESP_LOGI(TAG_SCAN, "Starting I2C Bus Scan (0x01 to 0x7F)...");
    ESP_LOGI(TAG_SCAN, "==================================================");

    uint8_t devices_found = 0;

    for (uint8_t addr = 0x01; addr < 0x7F; addr++) {
        esp_err_t ret = i2c_master_probe(bus_handle, addr, 50);

        if (ret == ESP_OK) {
            devices_found++;
            const char *desc = "";
            switch (addr) {
                case 0x20: desc = " (Possible TCA9554 Expander / Touch)"; break;
                case 0x36: desc = " (OV5647 Camera SCCB)"; break;
                case 0x45: desc = " (Waveshare Backlight IC / Panel)"; break;
                case 0x5D: desc = " (GT911 Touch Controller)"; break;
                case 0x14: desc = " (GT911 Alt Touch Address)"; break;
                default:   desc = " (Unknown Device)"; break;
            }
            ESP_LOGI(TAG_SCAN, " -> Found device at 7-bit address: 0x%02X%s", addr, desc);
        }
    }

    ESP_LOGI(TAG_SCAN, "==================================================");
    if (devices_found == 0) {
        ESP_LOGW(TAG_SCAN, "No I2C devices responded! Check SDA/SCL pins or power rails.");
    } else {
        ESP_LOGI(TAG_SCAN, "Scan complete: Total %d device(s) detected.", devices_found);
    }
    ESP_LOGI(TAG_SCAN, "==================================================");
}

static esp_err_t enable_board_power_rails(void) {
    ESP_LOGI(TAG_LCD, "Configuring LEDC PWM on GPIO 6 for Backlight Driver...");

    // 1. Configure LEDC Timer
    ledc_timer_config_t ledc_timer = {};
    ledc_timer.speed_mode       = LEDC_LOW_SPEED_MODE;
    ledc_timer.timer_num        = LEDC_TIMER_0;
    ledc_timer.duty_resolution  = LEDC_TIMER_8_BIT;
    ledc_timer.freq_hz          = 5000;
    ledc_timer.clk_cfg          = LEDC_AUTO_CLK;
    ESP_ERROR_CHECK(ledc_timer_config(&ledc_timer));

    // 2. Configure ONLY GPIO 6 for Backlight PWM (Do NOT touch GPIO 39/40 I2C pins!)
    ledc_channel_config_t ledc_channel = {};
    ledc_channel.speed_mode     = LEDC_LOW_SPEED_MODE;
    ledc_channel.channel        = LEDC_CHANNEL_0;
    ledc_channel.timer_sel      = LEDC_TIMER_0;
    ledc_channel.intr_type      = LEDC_INTR_DISABLE;
    ledc_channel.gpio_num       = BoardPins::Display::PWM_BL; // GPIO 6
    ledc_channel.duty           = 0; // 100% duty cycle    KT:255
    ledc_channel.hpoint         = 0;
    
    ESP_ERROR_CHECK(ledc_channel_config(&ledc_channel));
    ESP_ERROR_CHECK(ledc_update_duty(LEDC_LOW_SPEED_MODE, LEDC_CHANNEL_0));

    ESP_LOGI(TAG_LCD, "Backlight PWM initialized on GPIO 6 at 100%% brightness.");

    vTaskDelay(pdMS_TO_TICKS(50));
    return ESP_OK;
}

// -----------------------------------------------------------------------------
// MIPI-DSI Display Initialization (HX8394)
// -----------------------------------------------------------------------------
static esp_err_t hx8394_write_cmd(esp_lcd_panel_io_handle_t io, uint8_t cmd, const uint8_t *param, size_t param_len) {
    return esp_lcd_panel_io_tx_param(io, cmd, param, param_len);
}

static esp_err_t hx8394_init_vendor_registers(esp_lcd_panel_io_handle_t io) {
    ESP_LOGI(TAG_HW, "Sending Full HX8394-D Power, Gamma & Timing Sequence...");

    const uint8_t extc[] = {0x11, 0x83, 0x94};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB9, extc, sizeof(extc)));

    const uint8_t mipi_cfg[] = {0x63, 0x03, 0x68, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x40};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xBA, mipi_cfg, sizeof(mipi_cfg)));

    const uint8_t pwr_cfg[] = {
        0x48, 0x12, 0x8B, 0x0D, 0x01, 0x00, 0x08, 0x08, 
        0x00, 0x0F, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0x01
    };
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB1, pwr_cfg, sizeof(pwr_cfg)));

    const uint8_t disp_cfg[] = {
        0x00, 0x18, 0xC8, 0x05, 0x70, 0x00, 0x01, 0x00, 
        0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00
    };
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB2, disp_cfg, sizeof(disp_cfg)));

    const uint8_t cyc_cfg[] = {0x80, 0x00, 0x00, 0x00, 0x1A, 0x1A, 0x88, 0x01};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB4, cyc_cfg, sizeof(cyc_cfg)));

    const uint8_t panel_cfg[] = {0x00, 0x00, 0x10, 0x80, 0x00, 0x10, 0x00, 0x00};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xD2, panel_cfg, sizeof(panel_cfg)));

    const uint8_t gamma_cfg[] = {
        0x00, 0x0A, 0x15, 0x1B, 0x1E, 0x21, 0x24, 0x22, 
        0x47, 0x56, 0x65, 0x66, 0x6E, 0x7C, 0x82, 0x88, 
        0x93, 0x9A, 0x9E, 0x00, 0x0A, 0x15, 0x1B, 0x1E, 
        0x21, 0x24, 0x22, 0x47, 0x56, 0x65, 0x66, 0x6E, 
        0x7C, 0x82, 0x88, 0x93, 0x9A, 0x9E
    };
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xE0, gamma_cfg, sizeof(gamma_cfg)));

    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0x11, NULL, 0));
    vTaskDelay(pdMS_TO_TICKS(150));

    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0x29, NULL, 0));
    vTaskDelay(pdMS_TO_TICKS(50));

    ESP_LOGI(TAG_HW, "HX8394-D initialization sequence complete!");
    return ESP_OK;
}

static esp_err_t init_display(uint16_t width, uint16_t height) {
    ESP_LOGI(TAG_LCD, "Initializing HX8394 MIPI-DSI Display (%dx%d)...", width, height);

    ESP_ERROR_CHECK(init_internal_ldo());
    ESP_ERROR_CHECK(init_i2c_bus());
    ESP_ERROR_CHECK(init_tca9554());
    ESP_ERROR_CHECK(enable_board_power_rails());

    // Execute hardware reset pulse unconditionally
    ESP_ERROR_CHECK(reset_hx8394_display());

    esp_lcd_dsi_bus_config_t bus_config = {};
    bus_config.bus_id = 0;
    bus_config.num_data_lanes = 2;
    bus_config.phy_clk_src = MIPI_DSI_PHY_CLK_SRC_DEFAULT;
    bus_config.lane_bit_rate_mbps = 500;

    ESP_ERROR_CHECK(esp_lcd_new_dsi_bus(&bus_config, &s_dsi_bus_handle));

    esp_lcd_dbi_io_config_t dbi_config = {};
    dbi_config.virtual_channel = 0;
    dbi_config.lcd_cmd_bits = 8;
    dbi_config.lcd_param_bits = 8;

    ESP_ERROR_CHECK(esp_lcd_new_panel_io_dbi(s_dsi_bus_handle, &dbi_config, &s_lcd_io));
    
    // HX8394 will now accept vendor DCS commands!
    ESP_ERROR_CHECK(hx8394_init_vendor_registers(s_lcd_io));

    esp_lcd_dpi_panel_config_t dpi_config = {};
    dpi_config.virtual_channel = 0;
    dpi_config.dpi_clk_src = MIPI_DSI_DPI_CLK_SRC_DEFAULT;
    dpi_config.dpi_clock_freq_mhz = 27;
    dpi_config.pixel_format = LCD_COLOR_PIXEL_FORMAT_RGB565;
    dpi_config.in_color_format = LCD_COLOR_FMT_RGB565;
    dpi_config.out_color_format = LCD_COLOR_FMT_RGB565;
    dpi_config.num_fbs = 1;

    dpi_config.video_timing.h_size = width;
    dpi_config.video_timing.v_size = height;
    dpi_config.video_timing.hsync_back_porch = 44;
    dpi_config.video_timing.hsync_front_porch = 46;
    dpi_config.video_timing.hsync_pulse_width = 10;
    dpi_config.video_timing.vsync_back_porch = 16;
    dpi_config.video_timing.vsync_front_porch = 18;
    dpi_config.video_timing.vsync_pulse_width = 4;

    ESP_ERROR_CHECK(esp_lcd_new_panel_dpi(s_dsi_bus_handle, &dpi_config, &s_lcd_panel));
    ESP_ERROR_CHECK(esp_lcd_panel_init(s_lcd_panel));

    ESP_LOGI(TAG_LCD, "HX8394 MIPI-DSI Panel Active & Streaming!");
    return ESP_OK;
}

// -----------------------------------------------------------------------------
// MIPI-CSI Camera Initialization
// -----------------------------------------------------------------------------
static bool IRAM_ATTR csi_on_trans_finished_cb(esp_cam_ctlr_handle_t handle, esp_cam_ctlr_trans_t *trans, void *user_data) {
    return false;
}

static esp_err_t init_camera(uint16_t width, uint16_t height) {
    ESP_LOGI(TAG_HW, "Initializing MIPI-CSI Camera (%dx%d)...", width, height);

    if (s_i2c_bus == NULL) {
        ESP_LOGE(TAG_HW, "I2C bus not initialized!");
        return ESP_ERR_INVALID_STATE;
    }

    if (s_cam_sccb_handle == NULL) {
        i2c_device_config_t cam_sccb_cfg = {};
        cam_sccb_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
        cam_sccb_cfg.device_address = BoardAddr::OV5647_SCCB;
        cam_sccb_cfg.scl_speed_hz = 100000;

        esp_err_t ret = i2c_master_bus_add_device(s_i2c_bus, &cam_sccb_cfg, &s_cam_sccb_handle);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_HW, "Failed to add OV5647 SCCB device: 0x%x", ret);
            return ret;
        }
    }

    s_cam_fb_size = width * height * 2;
    if (s_cam_frame_buffer == NULL) {
        s_cam_frame_buffer = (uint8_t *)heap_caps_malloc(s_cam_fb_size, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
        if (!s_cam_frame_buffer) {
            ESP_LOGE(TAG_CAM, "Failed to allocate camera framebuffer in PSRAM!");
            return ESP_ERR_NO_MEM;
        }
    }

    if (s_csi_cam_handle == NULL) {
        esp_cam_ctlr_csi_config_t csi_config = {};
        csi_config.ctlr_id = 0;
        csi_config.clk_src = MIPI_CSI_PHY_CLK_SRC_DEFAULT;
        csi_config.h_res = width;
        csi_config.v_res = height;
        csi_config.data_lane_num = 2;
        csi_config.lane_bit_rate_mbps = 800;
        csi_config.input_data_color_type = CAM_CTLR_COLOR_RAW10;
        csi_config.output_data_color_type = CAM_CTLR_COLOR_RAW10;
        csi_config.queue_items = 2;
        csi_config.byte_swap_en = false;

        esp_err_t ret = esp_cam_new_csi_ctlr(&csi_config, &s_csi_cam_handle);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_CAM, "esp_cam_new_csi_ctlr failed: 0x%x (%s)", ret, esp_err_to_name(ret));
            return ret;
        }

        esp_cam_ctlr_evt_cbs_t cbs = {
            .on_get_new_trans = NULL,
            .on_trans_finished = csi_on_trans_finished_cb,
        };
        ESP_ERROR_CHECK(esp_cam_ctlr_register_event_callbacks(s_csi_cam_handle, &cbs, NULL));

        ESP_ERROR_CHECK(esp_cam_ctlr_enable(s_csi_cam_handle));
        ESP_ERROR_CHECK(esp_cam_ctlr_start(s_csi_cam_handle));
    }

    ESP_LOGI(TAG_HW, "OV5647 MIPI-CSI Pipeline Active & Frame Buffer Ready!");
    return ESP_OK;
}

// -----------------------------------------------------------------------------
// Public Rust FFI Exports
// -----------------------------------------------------------------------------
extern "C" {

int init_i2c_bus(void) {
    if (s_i2c_bus != NULL) return ESP_OK;

    ESP_LOGI(TAG_HW, "Initializing I2C Master Bus (I2C0)...");

    i2c_master_bus_config_t bus_config = {};
    bus_config.i2c_port = I2C_NUM_0;
    bus_config.sda_io_num = BoardPins::I2C::SDA;
    bus_config.scl_io_num = BoardPins::I2C::SCL;
    bus_config.clk_source = I2C_CLK_SRC_DEFAULT;
    bus_config.glitch_ignore_cnt = 7;
    bus_config.flags.enable_internal_pullup = true;

    ESP_ERROR_CHECK(i2c_new_master_bus(&bus_config, &s_i2c_bus));
    //scan_i2c_bus(s_i2c_bus);

    return ESP_OK;
}

int init_tca9554(void) {
    if (s_i2c_bus == NULL) {
        ESP_LOGE("p4_lcd", "Cannot init TCA9554: I2C bus is NULL!");
        return -1;
    }
    if (s_tca9554_handle != NULL) return ESP_OK;

    ESP_LOGI(TAG_LCD, "Attaching TCA9554 at address 0x%02X to I2C bus...", BoardAddr::TCA9554_EXPANDER);

    i2c_device_config_t dev_cfg = {};
    dev_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    dev_cfg.device_address = BoardAddr::TCA9554_EXPANDER;
    dev_cfg.scl_speed_hz = 100000; // 100 kHz Standard Mode for reliable bring-up

    esp_err_t err = i2c_master_bus_add_device(s_i2c_bus, &dev_cfg, &s_tca9554_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "Failed to add TCA9554 device to bus: 0x%x", err);
        return (int)err;
    }

    // 1. Configure Reg 0x03: P0-P3 as OUTPUT (0), P4-P7 as INPUT (1)
    uint8_t config_cmd[2] = {0x03, 0xF0};
    err = i2c_master_transmit(s_tca9554_handle, config_cmd, sizeof(config_cmd), 1000);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "TCA9554 config reg write failed: 0x%x. Cleaning up device handle.", err);
        i2c_master_bus_rm_device(s_tca9554_handle);
        s_tca9554_handle = NULL;
        return (int)err;
    }

    // 2. Assert Reset: Drive P0 LOW (0x0E = 0000 1110)
    ESP_LOGI("p4_lcd", "Asserting HX8394 hardware reset (P0 LOW)...");
    uint8_t rst_low_cmd[2] = {0x01, 0x0E};
    err = i2c_master_transmit(s_tca9554_handle, rst_low_cmd, sizeof(rst_low_cmd), 1000);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "TCA9554 reset LOW write failed: 0x%x", err);
        i2c_master_bus_rm_device(s_tca9554_handle);
        s_tca9554_handle = NULL;
        return (int)err;
    }

    vTaskDelay(pdMS_TO_TICKS(20)); // Hold RESET LOW for 20ms

    // 3. De-assert Reset: Drive P0 HIGH (0x0F = 0000 1111)
    ESP_LOGI(TAG_LCD, "De-asserting HX8394 hardware reset (P0 HIGH, P1 LOW)...");
    uint8_t rst_high_cmd[2] = {0x01, 0x0D};
    err = i2c_master_transmit(s_tca9554_handle, rst_high_cmd, sizeof(rst_high_cmd), 1000);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "TCA9554 reset HIGH write failed: 0x%x", err);
        i2c_master_bus_rm_device(s_tca9554_handle);
        s_tca9554_handle = NULL;
        return (int)err;
    }

    vTaskDelay(pdMS_TO_TICKS(120)); // Wait 120ms for HX8394 internal power-on reset

    ESP_LOGI(TAG_LCD, "TCA9554 I/O Expander & HX8394 Hardware Reset complete.");
    return (int)ESP_OK;
}

int init_gt911_device(void) {
    if (s_i2c_bus == NULL) return (int)ESP_ERR_INVALID_STATE;
    if (s_gt911_handle != NULL) return (int)ESP_OK;

    ESP_LOGI(TAG_HW, "Attaching GT911 Touch Controller to shared I2C bus...");

    i2c_device_config_t dev_cfg = {};
    dev_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    dev_cfg.device_address = BoardAddr::GT911_TOUCH;
    dev_cfg.scl_speed_hz = 400000;

    esp_err_t err = i2c_master_bus_add_device(s_i2c_bus, &dev_cfg, &s_gt911_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_HW, "Failed to add GT911 device: 0x%x", err);
        return (int)err;
    }

    ESP_LOGI(TAG_HW, "GT911 touch device successfully attached to I2C bus.");
    return (int)ESP_OK;
}

int gt911_i2c_read(uint16_t reg, uint8_t *data, size_t len) {
    if (s_gt911_handle == NULL) return (int)ESP_ERR_INVALID_STATE;
    if (data == NULL || len == 0) return (int)ESP_ERR_INVALID_ARG;

    uint8_t reg_buf[2] = { (uint8_t)(reg >> 8), (uint8_t)(reg & 0xFF) };
    return (int)i2c_master_transmit_receive(s_gt911_handle, reg_buf, sizeof(reg_buf), data, len, 1000);
}

int gt911_i2c_write(uint16_t reg, uint8_t val) {
    if (s_gt911_handle == NULL) return (int)ESP_ERR_INVALID_STATE;

    uint8_t tx_buf[3] = { (uint8_t)(reg >> 8), (uint8_t)(reg & 0xFF), val };
    return (int)i2c_master_transmit(s_gt911_handle, tx_buf, sizeof(tx_buf), 1000);
}

int32_t p4_perform_ota_update(const char *url) {
    if (url == NULL) return ESP_ERR_INVALID_ARG;

    ESP_LOGI(TAG_OTA, "Starting OTA update from: %s", url);

    esp_http_client_config_t http_config = {};
    http_config.url = url;
    http_config.timeout_ms = 15000;
    http_config.keep_alive_enable = true;

    esp_https_ota_config_t ota_config = {};
    ota_config.http_config = &http_config;

    esp_err_t ret = esp_https_ota(&ota_config);
    if (ret == ESP_OK) {
        ESP_LOGI(TAG_OTA, "OTA Update complete! Rebooting in 2 seconds...");
        vTaskDelay(pdMS_TO_TICKS(2000));
        esp_restart();
    } else {
        ESP_LOGE(TAG_OTA, "OTA Update failed: 0x%x (%s)", ret, esp_err_to_name(ret));
    }
    return ret;
}

void p4_mark_app_valid(void) {
    esp_ota_img_states_t ota_state;
    const esp_partition_t *running = esp_ota_get_running_partition();

    if (esp_ota_get_state_partition(running, &ota_state) == ESP_OK) {
        if (ota_state == ESP_OTA_IMG_PENDING_VERIFY) {
            ESP_LOGI(TAG_OTA, "New app booted cleanly! Marking partition as VALID.");
            esp_ota_mark_app_valid_cancel_rollback();
        }
    }
}    

void p4_display_draw_test_pattern(void) {
    if (!s_lcd_panel) return;

    size_t buffer_size = 720 * 1280 * sizeof(uint16_t);
    uint16_t *test_buf = (uint16_t *)heap_caps_malloc(buffer_size, MALLOC_CAP_SPIRAM);

    if (test_buf) {
        for (int i = 0; i < 720 * 1280; i++) {
            test_buf[i] = 0x07FF; // Solid Cyan (RGB565)
        }
        ESP_LOGI(TAG_HW, "Pushing Solid Cyan Test Frame to Display...");
        esp_lcd_panel_draw_bitmap(s_lcd_panel, 0, 0, 720, 1280, test_buf);
        heap_caps_free(test_buf);
    }
}

int32_t p4_hardware_init_all(const p4_hardware_config_t *config) {
    ESP_LOGI(TAG_HW, "Starting Unified Hardware Bring-up...");

    if (s_hardware_initialized) return ESP_OK;
    if (!config) return -1;

    esp_err_t ret = init_display(config->display_width, config->display_height);
    if (ret != ESP_OK) return ret;

    ret = init_camera(config->camera_width, config->camera_height);
    if (ret != ESP_OK) return ret;

    ESP_LOGI(TAG_HW, "All Systems Initialized Successfully!");
    s_hardware_initialized = true;

    p4_display_draw_test_pattern();
    return ESP_OK;
}

int32_t p4_camera_capture_frame(p4_camera_frame_t *frame, uint32_t timeout_ms) {
    if (!s_csi_cam_handle || !s_cam_frame_buffer || !frame) return -1;

    esp_cam_ctlr_trans_t trans = {};
    trans.buffer = s_cam_frame_buffer;
    trans.buflen = s_cam_fb_size;

    esp_err_t ret = esp_cam_ctlr_receive(s_csi_cam_handle, &trans, pdMS_TO_TICKS(timeout_ms));

    if (ret == ESP_OK) {
        frame->data = (uint8_t *)trans.buffer;
        frame->data_len = (trans.received_size > 0) ? trans.received_size : s_cam_fb_size;
        frame->width = 1280;
        frame->height = 720;
        return 0;
    }
    return (int32_t)ret;
}

int run_face_inference(
    const uint8_t* frame_rgb888, 
    size_t frame_len, 
    const uint8_t* model_bytes, 
    FaceEmbedding* embedding_out
) {
    if (!frame_rgb888 || !model_bytes || !embedding_out) return -1;

    try {
        // Use std::unique_ptr for automatic deletion on any exit path
        auto model = std::make_unique<dl::Model>((const char*)model_bytes);
        auto inputs = model->get_inputs();
        if (inputs.empty()) return -2;

        dl::TensorBase *input_tensor = inputs.begin()->second;
        std::memcpy(input_tensor->data, frame_rgb888, frame_len);

        model->run();

        auto outputs = model->get_outputs();
        if (outputs.empty()) return -3;

        dl::TensorBase *output_tensor = outputs.begin()->second;
        float *output_data = (float*)output_tensor->data;

        size_t elem_count = output_tensor->get_size();
        if (elem_count > 512) elem_count = 512;
        std::memcpy(embedding_out->values, output_data, elem_count * sizeof(float));

        return 0;
    } catch (...) {
        return -4;
    }
}

int32_t init_p4_ethernet(void) {
    ESP_LOGI(TAG_ETH, "Initializing Waveshare ESP32-P4-NANO EMAC Ethernet...");

    // 1. Hardware Reset PHY (GPIO 53 LOW -> HIGH)
    gpio_config_t rst_cfg = {};
    rst_cfg.pin_bit_mask = (1ULL << BoardPins::Ethernet::RESET);
    rst_cfg.mode = GPIO_MODE_OUTPUT;
    gpio_config(&rst_cfg);

    gpio_set_level(BoardPins::Ethernet::RESET, 0);
    vTaskDelay(pdMS_TO_TICKS(50));
    gpio_set_level(BoardPins::Ethernet::RESET, 1);
    vTaskDelay(pdMS_TO_TICKS(100));

    // 2. Netif & Event Loop Initialization
    esp_err_t ret = esp_netif_init();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    ret = esp_event_loop_create_default();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    esp_netif_config_t cfg = ESP_NETIF_DEFAULT_ETH();
    esp_netif_t *eth_netif = esp_netif_new(&cfg);

    // 3. Physical Wiring Configuration
    eth_mac_config_t mac_config = ETH_MAC_DEFAULT_CONFIG();
    eth_esp32_emac_config_t emac_config = {};

    emac_config.smi_gpio.mdc_num  = BoardPins::Ethernet::MDC;
    emac_config.smi_gpio.mdio_num = BoardPins::Ethernet::MDIO;
    gpio_set_pull_mode(BoardPins::Ethernet::MDIO, GPIO_PULLUP_ONLY);

    emac_config.interface = EMAC_DATA_INTERFACE_RMII;
    emac_config.clock_config.rmii.clock_mode = EMAC_CLK_EXT_IN;
    emac_config.clock_config.rmii.clock_gpio = (emac_rmii_clock_gpio_t)BoardPins::Ethernet::CLK;

    emac_config.dma_burst_len = ETH_DMA_BURST_LEN_32;
    emac_config.intr_priority = 0;

    emac_config.emac_dataif_gpio.rmii.tx_en_num  = BoardPins::Ethernet::RMII::TX_EN;
    emac_config.emac_dataif_gpio.rmii.txd0_num   = BoardPins::Ethernet::RMII::TXD0;
    emac_config.emac_dataif_gpio.rmii.txd1_num   = BoardPins::Ethernet::RMII::TXD1;
    emac_config.emac_dataif_gpio.rmii.crs_dv_num = BoardPins::Ethernet::RMII::CRS_DV;
    emac_config.emac_dataif_gpio.rmii.rxd0_num   = BoardPins::Ethernet::RMII::RXD0;
    emac_config.emac_dataif_gpio.rmii.rxd1_num   = BoardPins::Ethernet::RMII::RXD1;

    emac_config.clock_config_out_in.rmii.clock_mode = EMAC_CLK_EXT_IN;
    emac_config.clock_config_out_in.rmii.clock_gpio = -1;

    // 4. Create EMAC Instance
    esp_eth_mac_t *mac = esp_eth_mac_new_esp32(&emac_config, &mac_config);
    if (!mac) return -1;

    // 5. Create IP101 PHY Instance
    eth_phy_config_t phy_config = ETH_PHY_DEFAULT_CONFIG();
    phy_config.phy_addr = 1;
    phy_config.reset_gpio_num = -1;

    esp_eth_phy_t *phy = esp_eth_phy_new_ip101(&phy_config);
    if (!phy) {
        mac->del(mac);
        return -1;
    }

    // 6. Install Driver & Attach Netif
    esp_eth_config_t eth_config = ETH_DEFAULT_CONFIG(mac, phy);
    esp_eth_handle_t eth_handle = NULL;
    ret = esp_eth_driver_install(&eth_config, &eth_handle);
    if (ret != ESP_OK) {
        mac->del(mac);
        phy->del(phy);
        return ret;
    }

    ret = esp_netif_attach(eth_netif, esp_eth_new_netif_glue(eth_handle));
    if (ret != ESP_OK) return ret;

    return esp_eth_start(eth_handle);
}

int init_admin_button_gpio() {
    gpio_config_t io_conf = {};
    io_conf.pin_bit_mask = (1ULL << BoardPins::System::ADMIN_BTN);
    io_conf.mode = GPIO_MODE_INPUT;
    io_conf.pull_up_en = GPIO_PULLUP_ENABLE;
    io_conf.pull_down_en = GPIO_PULLDOWN_DISABLE;
    io_conf.intr_type = GPIO_INTR_DISABLE;

    return (int)gpio_config(&io_conf);
}

bool is_admin_button_pressed() {
    return gpio_get_level(BoardPins::System::ADMIN_BTN) == 0;
}

int32_t dl_mobilefacenet_init(const uint8_t *model_buf, size_t model_size) {
    if (!model_buf || model_size == 0) {
        ESP_LOGE(TAG_FACENET, "Invalid model buffer or size");
        return ESP_ERR_INVALID_ARG;
    }

    try {
        // Construct ESP-DL model directly from memory-mapped flash pointer
        g_mobilefacenet_model = new dl::Model(
            (const char *)model_buf,
            model_size
        );

        if (!g_mobilefacenet_model) {
            ESP_LOGE(TAG_FACENET, "Failed to allocate dl::Model");
            return ESP_FAIL;
        }

        ESP_LOGI(TAG_FACENET, "MobileFaceNet loaded successfully! (Flash addr: %p)", model_buf);
        return ESP_OK;

    } catch (const std::exception &e) {
        ESP_LOGE(TAG_FACENET, "Exception during ESP-DL model construction: %s", e.what());
        return ESP_FAIL;
    } catch (...) {
        ESP_LOGE(TAG_FACENET, "Unknown exception during ESP-DL model construction");
        return ESP_FAIL;
    }
}

int32_t dl_mobilefacenet_run(const uint8_t *crop_rgb888, float *out_embedding, size_t embedding_len) {
    if (!g_mobilefacenet_model) {
        ESP_LOGE(TAG_FACENET, "Model not initialized! Call dl_mobilefacenet_init first.");
        return ESP_ERR_INVALID_STATE;
    }

    if (!crop_rgb888 || !out_embedding || embedding_len < 512) {
        return ESP_ERR_INVALID_ARG;
    }

    try {
        // Step A: Get reference to input tensor
        std::map<std::string, dl::TensorBase *> &inputs = g_mobilefacenet_model->get_inputs();
        if (inputs.empty()) {
            ESP_LOGE(TAG_FACENET, "Model has no input tensors!");
            return ESP_FAIL;
        }

        dl::TensorBase *input_tensor = inputs.begin()->second;
        void *tensor_buf = input_tensor->get_element_ptr();
        if (!tensor_buf) {
            ESP_LOGE(TAG_FACENET, "Input tensor memory buffer is null!");
            return ESP_FAIL;
        }

        size_t input_bytes = input_tensor->get_bytes();
        std::memcpy(tensor_buf, crop_rgb888, input_bytes);

        // Step B: Run hardware-accelerated forward pass on ESP32-P4 PIE engine
        g_mobilefacenet_model->run();

        // Step C: Retrieve output tensor
        std::map<std::string, dl::TensorBase *> &outputs = g_mobilefacenet_model->get_outputs();
        if (outputs.empty()) {
            ESP_LOGE(TAG_FACENET, "Model has no output tensors!");
            return ESP_FAIL;
        }

        dl::TensorBase *output_tensor = outputs.begin()->second;
        int8_t *quant_data = (int8_t *)output_tensor->get_element_ptr();

        int exponent = output_tensor->get_exponent();
        float scale = std::pow(2.0f, exponent);

        // Step D: Dequantize INT8 values to float & calculate L2 norm squared
        float sum_squares = 0.0f;
        for (size_t i = 0; i < 512; i++) {
            float dequant_val = (float)quant_data[i] * scale;
            out_embedding[i] = dequant_val;
            sum_squares += dequant_val * dequant_val;
        }

        // Step E: Apply L2 Normalization
        float l2_norm = std::sqrt(sum_squares);
        if (l2_norm > 1e-6f) {
            float inv_norm = 1.0f / l2_norm;
            for (size_t i = 0; i < 512; i++) {
                out_embedding[i] *= inv_norm;
            }
        }

        return ESP_OK;

    } catch (const std::exception &e) {
        ESP_LOGE(TAG_FACENET, "Exception during inference: %s", e.what());
        return ESP_FAIL;
    } catch (...) {
        ESP_LOGE(TAG_FACENET, "Unknown exception during inference");
        return ESP_FAIL;
    }
}

} // extern "C"