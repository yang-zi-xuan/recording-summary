sherpa-onnx 原生库(Windows x64, shared 构建)
================================================

来源:https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.12.9/sherpa-onnx-v1.12.9-win-x64-shared.tar.bz2
版本:v1.12.9

用途:sherpa-rs-sys 的 download-binaries feature 会从 GitHub 下载这个包。
     国内网络下这一步经常超时(ureq 没有重试与断点续传),所以这里把它固定下来。

包含的文件:
  lib/sherpa-onnx-c-api.lib         导入库(链接期)
  lib/sherpa-onnx-cxx-api.lib       导入库(链接期)
  lib/sherpa-onnx-c-api.dll         运行时
  lib/sherpa-onnx-cxx-api.dll       运行时
  lib/onnxruntime.dll               推理运行时
  lib/onnxruntime_providers_shared.dll

构建脚本(scripts/cargo.ps1)会在检测到本目录时设置 SHERPA_LIB_PATH,
从而跳过联网下载。

重新获取的方式(需要能访问 GitHub):
  curl -L --ssl-no-revoke -o sherpa.tar.bz2 ^
    https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.12.9/sherpa-onnx-v1.12.9-win-x64-shared.tar.bz2
  tar -xjf sherpa.tar.bz2
  然后把 sherpa-onnx-v1.12.9-win-x64-shared\{lib,bin} 里的上述文件拷到本目录的 lib/ 下。
