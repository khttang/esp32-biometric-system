use anyhow::{Context, Result};
use esp_idf_svc::mqtt::client::{EspMqttClient, EspMqttEvent, EventPayload, MqttClientConfiguration, QoS};
use esp_idf_sys::EspError;
use log::{info, warn};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Default, Debug, Clone)]
pub struct PendingAssets {
    pub target_fw_ver: Option<String>,
    pub target_model_ver: Option<String>,
    pub model_url: Option<String>,
}

pub struct ThingsBoardClient {
    client: EspMqttClient<'static>,
    pub pending: Arc<Mutex<PendingAssets>>,
}

impl ThingsBoardClient {
    pub fn new(broker_url: &str, device_token: &str) -> Result<Self> {
        let pending = Arc::new(Mutex::new(PendingAssets::default()));
        let pending_clone = Arc::clone(&pending);

        let cfg = MqttClientConfiguration {
            client_id: Some("esp32p4-biometric-gateway"),
            username: Some(device_token),
            ..Default::default()
        };

// Explicitly type the closure argument and unwrap the `Ok` Result
        let mut client = EspMqttClient::new_cb(
            broker_url,
            &cfg,
            move |event: EspMqttEvent<'_> | {
                if let EventPayload::Received { topic, data, .. } = event.payload() {
                    // Safely unwrap `Option<&str>` before checking substring
                    if let Some(topic_str) = topic {
                        if topic_str.contains("v1/devices/me/attributes") {
                            if let Ok(json_str) = std::str::from_utf8(data) {
                                Self::parse_attribute_response(json_str, &pending_clone);
                            }
                        }
                    }
                }
            },
        )
        .context("Failed to create EspMqttClient instance")?;

        client
            .subscribe("v1/devices/me/attributes/response/+", QoS::AtLeastOnce)
            .context("Failed subscribing to ThingsBoard attributes response topic")?;

        Ok(Self { client, pending })
    }

    pub fn pull_target_versions(&mut self) -> Result<()> {
        let req_topic = "v1/devices/me/attributes/request/1";
        let payload = r#"{"sharedKeys":"fw_version,model_version,model_url"}"#;

        self.client
            .publish(req_topic, QoS::AtLeastOnce, false, payload.as_bytes())
            .context("Failed publishing attribute request to ThingsBoard")?;
        
        info!("[ThingsBoard] Requested asset attributes from server");
        Ok(())
    }

    fn parse_attribute_response(json_str: &str, state: &Arc<Mutex<PendingAssets>>) {
        if let Ok(v) = serde_json::from_str::<Value>(json_str) {
            let shared = v.get("shared").unwrap_or(&v);
            let mut pending = state.lock().unwrap();

            if let Some(fw) = shared.get("fw_version").and_then(|x| x.as_str()) {
                pending.target_fw_ver = Some(fw.to_string());
            }
            if let Some(model) = shared.get("model_version").and_then(|x| x.as_str()) {
                pending.target_model_ver = Some(model.to_string());
            }
            if let Some(url) = shared.get("model_url").and_then(|x| x.as_str()) {
                pending.model_url = Some(url.to_string());
            }

            warn!("[ThingsBoard] Pending asset targets updated: {:?}", *pending);
        }
    }
}