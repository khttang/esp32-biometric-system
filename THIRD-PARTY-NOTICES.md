# Third-Party Notices

This project is licensed under the MIT License (see [LICENSE](LICENSE)). It contains code adapted
from the work below, whose notice is reproduced as its licence requires.

## Espressif ESP-DL face model components

`firmware/components/biometrics_wrapper/face_inference.cpp` adapts the model setup
(pre-processing and post-processing parameters and the two-stage MSR → MNP detection flow) from
Espressif's `human_face_detect` 0.4.2 and `human_face_recognition` 0.3.2 components
(<https://github.com/espressif/esp-dl/tree/master/models>).

```text
MIT License

Copyright (c) 2021 Espressif Systems (Shanghai) Co., Ltd.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Not included in this repository

- **Model weights.** Espressif's pretrained face models are downloaded by
  `tools/face-models.sh` and are not committed.
- **The LFW dataset.** It is downloaded by `tools/face-eval.sh` for the accuracy evaluation and
  is not committed; only aggregate results are published.
- **ESP-IDF, ESP-DL, LVGL and the other build dependencies.** They are fetched at build time
  under their own licences (`firmware/Cargo.lock`, `firmware/components_esp32p4.lock`).
