import os
os.environ["PROTOCOL_BUFFERS_PYTHON_IMPLEMENTATION"] = "python"

import torch
from esp_ppq.api import espdl_quantize_onnx

# collate_fn receives individual sample dictionaries or tensors directly from esp-ppq
def collate_fn(sample):
    if isinstance(sample, dict):
        return sample["input"]
    elif isinstance(sample, (list, tuple)):
        return sample[0]
    return sample

def main():
    onnx_file = "face_net.onnx"
    output_prefix = "mobilefacenet_quantized.espdl"

    print(f"Loading {onnx_file} and quantizing with esp-ppq...")

    # Calibration dataset: 32 raw random image tensors (1x3x112x112)
    calibration_dataset = [torch.randn(1, 3, 112, 112) for _ in range(32)]

    # Quantize directly to .espdl for ESP32-P4
    espdl_quantize_onnx(
        onnx_import_file=onnx_file,
        espdl_export_file=output_prefix,
        calib_dataloader=calibration_dataset,
        calib_steps=32,
        collate_fn=collate_fn,
        input_shape=[1, 3, 112, 112],
        target="esp32p4",
        device="cpu"
    )

    print(f"\nSuccess! {output_prefix}.espdl generated successfully.")

if __name__ == "__main__":
    main()
