esp32-biometric-system/
├── .cargo/                     # Cargo runner & cross-compilation configs
├── .github/
│   └── workflows/              # CI/CD pipelines (Tests, Model Conversion, Firmware Builds)
├── Cargo.toml                  # Root Cargo workspace manifest
│
├── firmware/                   # ESP32-P4 Rust Firmware
│   ├── Cargo.toml              # Firmware crate manifest
│   ├── build.rs                # Compiles C++ esp-dl wrapper & links CMake
│   ├── partitions.csv          # Flash Partition Table (App, Model, NVS)
│   ├── sdkconfig.defaults      # ESP-IDF SDK configuration
│   ├── src/
│   │   ├── main.rs             # Application entry point & FOTA state machine
│   │   ├── audio/              # ES8311 I2C/I2S driver
│   │   ├── camera/             # MIPI-CSI camera capture
│   │   ├── fota/               # ThingsBoard / HTTPS update client
│   │   ├── model/              # PSRAM loader & C++ FFI wrapper bindings
│   │   └── storage/            # NVS enrolled profile database reader
│   ├── cpp/                    # C++ Wrapper for ESP-DL
│   │   ├── CMakeLists.txt
│   │   └── esp_dl_wrapper.cpp  # C++ bridge to call ESP-DL C++ library
│   └── tests/                  # On-target / native Rust integration tests
│
├── ml/                         # Machine Learning Pipeline
│   ├── pyproject.toml          # Poetry / Pip dependencies (numpy, torch, esp-ppq)
│   ├── models/                 # Model definitions
│   │   └── mobilefacenet.py    # PyTorch Architecture
│   ├── export/                 # Export scripts
│   │   ├── export_onnx.py      # PyTorch -> ONNX
│   │   └── quantize_espdl.py   # ONNX -> ESP-DL (.espdl) via esp-ppq
│   ├── data/                   # Calibration sample sets
│   └── tests/                  # PyTest suite for tensor shape and ONNX runtime checks
│
├── tools/                      # System & Operational Utilities
│   ├── enroll_user.py          # Multi-modal (Face + Voice) Enrollment Tool
│   ├── ota_server.py           # Local HTTPS staging server for OTA testing
│   └── simulator.py            # Local serial/network device simulator
│
├── enrolled_users.json         # Local enrolled user profiles database
└── artifacts/                  # Output directory for compiled binaries
    ├── face_net.espdl          # Quantized model binary
    └── app_v1.0.0.bin          # Built firmware binary


Here is the complete, consolidated **ESP32-P4 Rust + C++ FFI Architecture Guide** formatted as a single, clean Markdown document.

You can copy and save this directly into a `README.md` or `ARCHITECTURE.md` file in your repository:

```markdown
# ESP32-P4 Rust + C++ Biometrics Blueprint

A production-ready blueprint for building firmware on the ESP32-P4 using Rust, CMake/ESP-IDF v5.3+, `bindgen`, and native C++ (`esp-dl` v3.x).

---

## 📂 1. Directory Structure

Organize the repository using standard ESP-IDF sub-components to ensure smooth integration between Cargo, CMake, and `bindgen`:

```text
firmware/
├── .cargo/
│   └── config.toml
├── Cargo.toml
├── build.rs
├── src/
│   ├── main.rs
│   └── wifi.rs
└── components/
    └── biometrics_wrapper/
        ├── CMakeLists.txt
        ├── biometrics_wrapper.cpp
        └── include/
            └── bindings.h

```

---

## ⚙️ 2. Project Configuration Files

### `.cargo/config.toml`

Defines the RISC-V target triple, linker flags, and target ESP-IDF version.

```toml
[build]
# Target the ESP32-P4 (32-bit RISC-V IMC architecture)
target = "riscv32imafc-esp-espidf"

[target.riscv32imafc-esp-espidf]
linker = "ld"
rustflags = [
    "-C", "link-arg=-Tlinkall.x",
    "-C", "link-arg=-nostartfiles",
]

[unstable]
# Compiles standard library specifically for the ESP32-P4 target
build-std = ["std", "panic_abort"]

[env]
ESP_IDF_VERSION = "v5.3.2"

```

---

### `Cargo.toml`

Essential dependencies for async networking, serialization, and FFI support.

```toml
[package]
name = "esp32p4-biometric-system"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1.0"
log = "0.4"
esp-idf-svc = { version = "0.49", features = ["alloc", "embassy-time-driver"] }
esp-idf-hal = "0.44"
embedded-svc = "0.28"
serde = { version = "1.0", default-features = false, features = ["derive"] }
serde_json = { version = "1.0", default-features = false, features = ["alloc"] }
tokio = { version = "1", default-features = false, features = ["sync", "time"] }

[build-dependencies]
embuild = "0.32"
bindgen = "0.69"

```

---

### `build.rs`

Generates Rust FFI bindings from `bindings.h` and propagates ESP-IDF linker arguments.

```rust
use embuild::build::LinkArgs;
use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=components/biometrics_wrapper/biometrics_wrapper.cpp");
    println!("cargo:rerun-if-changed=components/biometrics_wrapper/include/bindings.h");

    // Propagate ESP-IDF build flags and linker scripts
    LinkArgs::output_propagated("ESP_IDF").expect("Failed to propagate link args");

    // Generate Rust FFI bindings from clean C header
    let bindings = bindgen::Builder::default()
        .header("components/biometrics_wrapper/include/bindings.h")
        .clang_arg("-Icomponents/biometrics_wrapper/include")
        .derive_default(true)
        .use_core()
        .generate()
        .expect("Unable to generate FFI bindings for biometrics_wrapper");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

```

---

## 🛠️ 3. Component Code & FFI Interface

### `components/biometrics_wrapper/include/bindings.h`

> **Rule:** Use pure C types (`int32_t`, `uint8_t`, `size_t`) only. Never include heavy internal ESP-IDF C headers inside files parsed by `bindgen`.

```c
#ifndef BIOMETRICS_WRAPPER_BINDINGS_H
#define BIOMETRICS_WRAPPER_BINDINGS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    float values[512];
} FaceEmbedding;

// Standard C FFI signatures
int32_t init_p4_ethernet(void);

int run_face_inference(
    const uint8_t* frame_rgb888, 
    size_t frame_len, 
    const uint8_t* model_bytes, 
    FaceEmbedding* embedding_out
);

#ifdef __cplusplus
}
#endif

#endif // BIOMETRICS_WRAPPER_BINDINGS_H

```

---

### `components/biometrics_wrapper/CMakeLists.txt`

Self-contained component declaration for CMake. No `idf_component.yml` needed.

```cmake
idf_component_register(
    SRCS "biometrics_wrapper.cpp"
    INCLUDE_DIRS "include"
    REQUIRES main esp_eth esp_netif driver esp-dl
)

```

---

### `components/biometrics_wrapper/biometrics_wrapper.cpp`

Combines ESP32-P4 Ethernet (EMAC) initialization and `esp-dl` model execution.

```cpp
#include "bindings.h"

// Core ESP-IDF Headers
#include "esp_eth.h"
#include "esp_eth_mac_esp.h"
#include "esp_eth_phy_802_3.h"
#include "esp_event.h"
#include "esp_log.h"
#include "esp_netif.h"
#include "driver/gpio.h"

// Core esp-dl Headers
#include "dl_model_base.hpp"
#include "dl_tensor_base.hpp"

#include <cstring>

static const char *TAG = "p4_ethernet";

static void eth_event_handler(void *arg, esp_event_base_t event_base,
                              int32_t event_id, void *event_data) {
    uint8_t mac_addr[6] = {0};
    esp_eth_handle_t eth_handle = *(esp_eth_handle_t *)event_data;

    switch (event_id) {
    case ETHERNET_EVENT_CONNECTED:
        esp_eth_ioctl(eth_handle, ETH_CMD_G_MAC_ADDR, mac_addr);
        ESP_LOGI(TAG, "Ethernet Link Up. MAC: %02x:%02x:%02x:%02x:%02x:%02x",
                 mac_addr[0], mac_addr[1], mac_addr[2],
                 mac_addr[3], mac_addr[4], mac_addr[5]);
        break;
    case ETHERNET_EVENT_DISCONNECTED:
        ESP_LOGI(TAG, "Ethernet Link Down");
        break;
    case ETHERNET_EVENT_START:
        ESP_LOGI(TAG, "Ethernet Started");
        break;
    case ETHERNET_EVENT_STOP:
        ESP_LOGI(TAG, "Ethernet Stopped");
        break;
    default:
        break;
    }
}

static void got_ip_event_handler(void *arg, esp_event_base_t event_base,
                                 int32_t event_id, void *event_data) {
    ip_event_got_ip_t *event = (ip_event_got_ip_t *)event_data;
    const esp_netif_ip_info_t *ip_info = &event->ip_info;

    ESP_LOGI(TAG, "Ethernet Got IP Address:");
    ESP_LOGI(TAG, "  IP     : " IPSTR, IP2STR(&ip_info->ip));
    ESP_LOGI(TAG, "  Netmask: " IPSTR, IP2STR(&ip_info->netmask));
    ESP_LOGI(TAG, "  GW     : " IPSTR, IP2STR(&ip_info->gw));
}

extern "C" {

int32_t init_p4_ethernet(void) {
    ESP_LOGI(TAG, "Initializing ESP32-P4 Internal EMAC Ethernet...");

    esp_err_t ret = esp_netif_init();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    ret = esp_event_loop_create_default();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    esp_netif_config_t cfg = ESP_NETIF_DEFAULT_ETH();
    esp_netif_t *eth_netif = esp_netif_new(&cfg);

    eth_mac_config_t mac_config = ETH_MAC_DEFAULT_CONFIG();
    eth_esp32_emac_config_t emac_config = ETH_ESP32_EMAC_DEFAULT_CONFIG();
    emac_config.interface = EMAC_DATA_INTERFACE_RMII;

    // ESP-IDF v5.3+ SMI Pin Assignment
    emac_config.smi_gpio.mdc_num = 27;  
    emac_config.smi_gpio.mdio_num = 28; 

    // Clock Config
    emac_config.clock_config.rmii.clock_mode = EMAC_CLK_EXT_IN;

    esp_eth_mac_t *mac = esp_eth_mac_new_esp32(&emac_config, &mac_config);
    if (!mac) return -1;

    eth_phy_config_t phy_config = ETH_PHY_DEFAULT_CONFIG();
    phy_config.phy_addr = 0;
    phy_config.reset_gpio_num = -1;

    esp_eth_phy_t *phy = esp_eth_phy_new_generic(&phy_config);
    if (!phy) return -1;

    esp_eth_config_t eth_config = ETH_DEFAULT_CONFIG(mac, phy);
    esp_eth_handle_t eth_handle = NULL;
    ret = esp_eth_driver_install(&eth_config, &eth_handle);
    if (ret != ESP_OK) return ret;

    ret = esp_netif_attach(eth_netif, esp_eth_new_netif_glue(eth_handle));
    if (ret != ESP_OK) return ret;

    esp_event_handler_instance_register(ETH_EVENT, ESP_EVENT_ANY_ID, &eth_event_handler, eth_handle, NULL);
    esp_event_handler_instance_register(IP_EVENT, IP_EVENT_ETH_GOT_IP, &got_ip_event_handler, NULL, NULL);

    return esp_eth_start(eth_handle);
}

int run_face_inference(
    const uint8_t* frame_rgb888, 
    size_t frame_len, 
    const uint8_t* model_bytes, 
    FaceEmbedding* embedding_out
) {
    if (!frame_rgb888 || !model_bytes || !embedding_out) return -1;

    dl::Model *model = new dl::Model((const char*)model_bytes);
    auto inputs = model->get_inputs();
    if (inputs.empty()) {
        delete model;
        return -2;
    }

    dl::TensorBase *input_tensor = inputs.begin()->second;
    std::memcpy(input_tensor->data, frame_rgb888, frame_len);

    model->run();

    auto outputs = model->get_outputs();
    if (outputs.empty()) {
        delete model;
        return -3;
    }

    dl::TensorBase *output_tensor = outputs.begin()->second;
    float *output_data = (float*)output_tensor->data;

    size_t elem_count = output_tensor->get_size();
    if (elem_count > 512) elem_count = 512;
    std::memcpy(embedding_out->values, output_data, elem_count * sizeof(float));

    delete model;
    return 0;
}

} // extern "C"

```

---

## ⚡ 4. Critical Rules & Technical Lessons

1. **`bindgen` Cleanliness:** Never include ESP-IDF internal system headers (`esp_err.h`, `esp_eth.h`) directly in header files processed by `bindgen`. Use standard C headers (`stdint.h`, `stddef.h`) and standard types (`int32_t`) in `bindings.h`.
2. **ESP-IDF v5.3+ Ethernet API:**
* **MDC/MDIO GPIOs:** Set via `emac_config.smi_gpio.mdc_num` and `emac_config.smi_gpio.mdio_num` (deprecated direct fields `smi_mdc_gpio_num` / `smi_mdio_gpio_num`).
* **Required Header:** `#include "esp_eth_mac_esp.h"` must be included to expose `eth_esp32_emac_config_t` and `esp_eth_mac_new_esp32`.
* **Generic PHY:** Generic 802.3 PHYs (LAN8720, IP101, RTL8201) are instantiated via `esp_eth_phy_new_generic(&phy_config)` from `#include "esp_eth_phy_802_3.h"`.


3. **No Duplicate Declarations:** Define structs (e.g., `FaceEmbedding`) **only once** inside `bindings.h`. Do not re-typedef them inside `.cpp` files.
4. **Cache Management:** When changing component header definitions or macro constants, run `cargo clean` to ensure Ninja/CMake purges cached `.cpp.obj` files inside `target/`.

```

```    