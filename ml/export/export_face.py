import torch
import torch.nn as nn

class ConvBlock(nn.Module):
    def __init__(self, in_c, out_c, kernel, stride, padding, groups=1):
        super().__init__()
        self.conv = nn.Conv2d(in_c, out_c, kernel, stride, padding, groups=groups, bias=False)
        self.bn = nn.BatchNorm2d(out_c)
        self.prelu = nn.PReLU(out_c)

    def forward(self, x):
        return self.prelu(self.bn(self.conv(x)))

class MobileFaceNet(nn.Module):
    def __init__(self, embedding_size=128):
        super().__init__()
        self.conv1 = ConvBlock(3, 64, 3, 2, 1)
        self.dw_conv1 = ConvBlock(64, 64, 3, 1, 1, groups=64)
        
        # Bottleneck blocks
        self.conv_two = ConvBlock(64, 64, 3, 2, 1, groups=64)
        self.conv_three = ConvBlock(64, 128, 1, 1, 0)
        
        # Global Depthwise Conv (GDConv) reduces spatial 28x28 -> 1x1 cleanly
        self.conv_sep = ConvBlock(128, 128, 3, 1, 1, groups=128)
        self.gdc = nn.Conv2d(128, 128, kernel_size=(28, 28), stride=1, padding=0, groups=128, bias=False)
        self.bn_gdc = nn.BatchNorm2d(128)
        self.linear = nn.Conv2d(128, embedding_size, 1, 1, 0, bias=False)
        self.bn_linear = nn.BatchNorm2d(embedding_size)

    def forward(self, x):
        x = self.conv1(x)       # -> [1, 64, 56, 56]
        x = self.dw_conv1(x)    # -> [1, 64, 56, 56]
        x = self.conv_two(x)    # -> [1, 64, 28, 28]
        x = self.conv_three(x)  # -> [1, 128, 28, 28]
        x = self.conv_sep(x)    # -> [1, 128, 28, 28]
        x = self.gdc(x)         # -> [1, 128, 1, 1]
        x = self.bn_gdc(x)
        x = self.linear(x)      # -> [1, 128, 1, 1]
        x = self.bn_linear(x)
        return x  # Outputs 4D [1, 128, 1, 1] tensor directly to avoid ONNX Reshape node

def main():
    print("Building MobileFaceNet without Reshape node for ESP-DL...")
    model = MobileFaceNet(embedding_size=128).eval()

    dummy_input = torch.randn(1, 3, 112, 112)
    onnx_file = "face_net.onnx"

    print(f"Exporting to {onnx_file}...")
    torch.onnx.export(
        model,
        dummy_input,
        onnx_file,
        export_params=True,
        opset_version=13,
        do_constant_folding=True,
        input_names=['input'],
        output_names=['embedding']
    )

    print("Successfully exported face_net.onnx!")

if __name__ == "__main__":
    main()
