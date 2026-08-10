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
#include "esp_err.h"
#include "esp_check.h"
#include "esp_cam_ctlr.h"
#include "esp_cam_ctlr_csi.h"
#include "esp_cam_ctlr_types.h"
#include "esp_cam_sensor.h"
#include "esp_lcd_mipi_dsi.h"
#include "esp_lcd_panel_ops.h"
#include "esp_lcd_panel_io.h"
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
#include "esp_ldo_regulator.h"

// Official Waveshare ESP32-P4-NANO BSP Headers
#include "esp_lcd_touch.h"
#include "bsp/esp32_p4_nano.h"
#include "bsp/display.h"
#include "bsp/touch.h"

// Core esp-dl Headers
#include "dl_model_base.hpp"
#include "dl_tensor_base.hpp"

#include "biometrics_wrapper.h"

// -----------------------------------------------------------------------------
// Hardware Pinout Definitions
// -----------------------------------------------------------------------------
namespace BoardPins {
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
    constexpr uint8_t OV5647_SCCB = 0x36;
}

#define TAG_HW      "p4_hardware"
#define TAG_ETH     "p4_ethernet"
#define TAG_CAM     "p4_camera"
#define TAG_OTA     "p4_ota"
#define TAG_FACENET "ESP_DL_FACENET"
#define TAG_I2S     "I2S_WRAPPER"
#define TAG_LCD     "p4_lcd"

// Global Hardware Handles
static esp_lcd_panel_handle_t s_lcd_panel = NULL;
static esp_lcd_panel_io_handle_t s_lcd_io = NULL;
static esp_lcd_touch_handle_t s_touch_handle = NULL;
static esp_cam_ctlr_handle_t s_csi_cam_handle = NULL;

static uint8_t *s_cam_frame_buffer = NULL;
static size_t s_cam_fb_size = 0;

static i2s_chan_handle_t g_i2s_tx_handle = NULL;
static i2s_chan_handle_t g_i2s_rx_handle = NULL;

static bool s_hardware_initialized = false;
static dl::Model *g_mobilefacenet_model = NULL;

// -----------------------------------------------------------------------------
// Display Initialization & Helper Functions
// -----------------------------------------------------------------------------
static esp_err_t power_on_display_bridge_0x45(void) {
    i2c_master_bus_handle_t bus_handle = bsp_i2c_get_handle();
    if (!bus_handle) {
        ESP_LOGE(TAG_LCD, "BSP I2C bus handle is NULL");
        return ESP_ERR_INVALID_STATE;
    }

    i2c_master_dev_handle_t dev_handle = NULL;
    i2c_device_config_t dev_cfg = {};
    dev_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    dev_cfg.device_address = 0x45;
    dev_cfg.scl_speed_hz = 100000;

    esp_err_t err = i2c_master_bus_add_device(bus_handle, &dev_cfg, &dev_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "Failed to register 0x45 I2C device: 0x%x", err);
        return err;
    }

    ESP_LOGI(TAG_LCD, "Sending power-on sequence to display bridge IC @ 0x45...");
    uint8_t cmd1[2] = {0x95, 0x11};
    i2c_master_transmit(dev_handle, cmd1, 2, 1000);
    uint8_t cmd2[2] = {0x95, 0x17};
    i2c_master_transmit(dev_handle, cmd2, 2, 1000);
    uint8_t cmd3[2] = {0x96, 0x00};
    i2c_master_transmit(dev_handle, cmd3, 2, 1000);
    vTaskDelay(pdMS_TO_TICKS(100));

    // Assert power and wait for PMIC voltage rails to stabilize
    uint8_t cmd4[2] = {0x96, 0xFF};
    i2c_master_transmit(dev_handle, cmd4, 2, 1000);
    vTaskDelay(pdMS_TO_TICKS(1000));

    i2c_master_bus_rm_device(dev_handle);
    ESP_LOGI(TAG_LCD, "0x45 Power-on sequence complete & rails stabilized.");
    return ESP_OK;
}

int32_t init_display_with_bsp(void) {
    if (s_lcd_panel != NULL) return ESP_OK;

    ESP_LOGI(TAG_LCD, "Initializing 5-inch MIPI-DSI Display via Waveshare BSP...");

    // 1. Initialize BSP primary I2C bus (I2C0, GPIO7/8)
    esp_err_t err = bsp_i2c_init();
    if (err != ESP_OK && err != ESP_ERR_INVALID_STATE) {
        ESP_LOGE(TAG_LCD, "Failed to initialize BSP I2C bus: 0x%x", err);
        return err;
    }

    // 2. Power on display bridge IC @ 0x45
    err = power_on_display_bridge_0x45();
    if (err != ESP_OK) {
        ESP_LOGW(TAG_LCD, "0x45 power sequence returned error: 0x%x (proceeding anyway)", err);
    }

    // 3. Allocate Display & Power Handles via BSP
    bsp_lcd_handles_t handles = {};
    bsp_display_config_t disp_cfg = {};
    
    err = bsp_display_new_with_handles(&disp_cfg, &handles);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "Failed to create BSP display handles: 0x%x (%s)", err, esp_err_to_name(err));
        return err;
    }

    s_lcd_panel = handles.panel;
    s_lcd_io = handles.io;

    // 4. Turn on Backlight
    ESP_ERROR_CHECK(bsp_display_backlight_on());

    ESP_LOGI(TAG_LCD, "5-inch Display & Backlight Initialized Successfully!");
    return ESP_OK;
}

void p4_display_draw_test_pattern(void) {
    if (!s_lcd_panel) {
        ESP_LOGE(TAG_LCD, "Cannot draw test pattern: Display panel handle is NULL!");
        return;
    }

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

// -----------------------------------------------------------------------------
// MIPI-CSI Camera Initialization
// -----------------------------------------------------------------------------
static bool IRAM_ATTR csi_on_trans_finished_cb(esp_cam_ctlr_handle_t handle, esp_cam_ctlr_trans_t *trans, void *user_data) {
    return false;
}

static esp_err_t init_camera(uint16_t width, uint16_t height) {
    ESP_LOGI(TAG_HW, "Initializing MIPI-CSI Camera (%dx%d)...", width, height);

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

// -----------------------------------------------------------------------------
// Audio Peripheral Drivers
// -----------------------------------------------------------------------------
int init_i2s_tx_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int dout_gpio) {
    return init_i2s_duplex_c(sample_rate, bclk_gpio, ws_gpio, -1, dout_gpio);
}

int init_i2s_mic_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio) {
    return init_i2s_duplex_c(sample_rate, bclk_gpio, ws_gpio, din_gpio, -1);
}

int init_i2s_duplex_c(uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio, int dout_gpio) {
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

    i2s_chan_config_t chan_cfg = I2S_CHANNEL_DEFAULT_CONFIG(I2S_NUM_0, I2S_ROLE_MASTER);
    
    i2s_chan_handle_t *p_tx = (dout_gpio >= 0) ? &g_i2s_tx_handle : NULL;
    i2s_chan_handle_t *p_rx = (din_gpio >= 0)  ? &g_i2s_rx_handle : NULL;

    esp_err_t ret = i2s_new_channel(&chan_cfg, p_tx, p_rx);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_I2S, "i2s_new_channel failed: 0x%x", ret);
        return (int)ret;
    }

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
        if (ret != ESP_OK) return (int)ret;
        ret = i2s_channel_enable(g_i2s_tx_handle);
        if (ret != ESP_OK) return (int)ret;
    }

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
        if (ret != ESP_OK) return (int)ret;
        ret = i2s_channel_enable(g_i2s_rx_handle);
        if (ret != ESP_OK) return (int)ret;
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
// Public Rust FFI Exports
// -----------------------------------------------------------------------------
extern "C" {

int32_t p4_hardware_init_all(const p4_hardware_config_t *config) {
    ESP_LOGI(TAG_HW, "Starting Unified Hardware Bring-up...");

    if (s_hardware_initialized) return ESP_OK;
    if (!config) return -1;

    esp_err_t ret = init_display_with_bsp();
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

int32_t init_p4_ethernet(void) {
    ESP_LOGI(TAG_ETH, "Initializing Waveshare ESP32-P4-NANO EMAC Ethernet...");

    gpio_config_t rst_cfg = {};
    rst_cfg.pin_bit_mask = (1ULL << BoardPins::Ethernet::RESET);
    rst_cfg.mode = GPIO_MODE_OUTPUT;
    gpio_config(&rst_cfg);

    gpio_set_level(BoardPins::Ethernet::RESET, 0);
    vTaskDelay(pdMS_TO_TICKS(50));
    gpio_set_level(BoardPins::Ethernet::RESET, 1);
    vTaskDelay(pdMS_TO_TICKS(100));

    esp_err_t ret = esp_netif_init();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    ret = esp_event_loop_create_default();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    esp_netif_config_t cfg = ESP_NETIF_DEFAULT_ETH();
    esp_netif_t *eth_netif = esp_netif_new(&cfg);

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

    esp_eth_mac_t *mac = esp_eth_mac_new_esp32(&emac_config, &mac_config);
    if (!mac) return -1;

    eth_phy_config_t phy_config = ETH_PHY_DEFAULT_CONFIG();
    phy_config.phy_addr = 1;
    phy_config.reset_gpio_num = -1;

    esp_eth_phy_t *phy = esp_eth_phy_new_ip101(&phy_config);
    if (!phy) {
        mac->del(mac);
        return -1;
    }

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

// -----------------------------------------------------------------------------
// ESP-DL Model Inference Entry Points
// -----------------------------------------------------------------------------
int32_t dl_mobilefacenet_init(const uint8_t *model_buf, size_t model_size) {
    if (!model_buf || model_size == 0) {
        ESP_LOGE(TAG_FACENET, "Invalid model buffer or size");
        return ESP_ERR_INVALID_ARG;
    }

    try {
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
        std::map<std::string, dl::TensorBase *> &inputs = g_mobilefacenet_model->get_inputs();
        if (inputs.empty()) return ESP_FAIL;

        dl::TensorBase *input_tensor = inputs.begin()->second;
        void *tensor_buf = input_tensor->get_element_ptr();
        if (!tensor_buf) return ESP_FAIL;

        size_t input_bytes = input_tensor->get_bytes();
        std::memcpy(tensor_buf, crop_rgb888, input_bytes);

        g_mobilefacenet_model->run();

        std::map<std::string, dl::TensorBase *> &outputs = g_mobilefacenet_model->get_outputs();
        if (outputs.empty()) return ESP_FAIL;

        dl::TensorBase *output_tensor = outputs.begin()->second;
        int8_t *quant_data = (int8_t *)output_tensor->get_element_ptr();

        int exponent = output_tensor->get_exponent();
        float scale = std::pow(2.0f, exponent);

        float sum_squares = 0.0f;
        for (size_t i = 0; i < 512; i++) {
            float dequant_val = (float)quant_data[i] * scale;
            out_embedding[i] = dequant_val;
            sum_squares += dequant_val * dequant_val;
        }

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

int32_t init_touch_with_bsp(void) {
    if (s_touch_handle != NULL) return ESP_OK;

    ESP_LOGI(TAG_LCD, "Initializing GT911 Touch Controller via Waveshare BSP...");

    esp_err_t err = bsp_i2c_init();
    if (err != ESP_OK && err != ESP_ERR_INVALID_STATE) {
        ESP_LOGE(TAG_LCD, "Failed to initialize I2C for touch: 0x%x", err);
        return err;
    }

    err = bsp_touch_new(NULL, &s_touch_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "Failed to initialize touch handle: 0x%x", err);
        return err;
    }

    ESP_LOGI(TAG_LCD, "Touch Controller Initialized Successfully!");
    return ESP_OK;
}

bool p4_touch_read(p4_touch_data_t *touch_data) {
    if (!s_touch_handle || !touch_data) return false;

    esp_lcd_touch_read_data(s_touch_handle);

    uint16_t x[1] = {0};
    uint16_t y[1] = {0};
    uint16_t strength[1] = {0};
    uint8_t point_num = 0;

    bool touched = esp_lcd_touch_get_coordinates(s_touch_handle, x, y, strength, &point_num, 1);

    touch_data->touched = touched && (point_num > 0);
    touch_data->x = x[0];
    touch_data->y = y[0];
    touch_data->strength = strength[0];
    touch_data->points = point_num;

    return touch_data->touched;
}

} // extern "C"