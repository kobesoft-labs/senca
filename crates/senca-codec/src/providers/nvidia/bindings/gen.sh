#!/bin/bash
# NVIDIA の型定義 bindings/nv.rs を生成する（開発機で実行し、結果をコミットする。ビルド時には使わない）。
# 使い方: PATH に bindgen（bindgen-cli 0.73.2）、LIBCLANG_PATH に libclang、RUSTFMT に rustfmt を置き、
#   HDR=<nv-codec-headers>/include/ffnvcodec ./gen.sh out.rs の後、nv.rs の先頭の出典コメントを付け直す。
set -euo pipefail
HDR=${HDR:-/tmp/sencadl/nv-codec-headers/include/ffnvcodec}
OUT=${1:-nv.rs}
bindgen wrapper.h -o "$OUT" \
  --allowlist-type 'tcu(Init|DriverGetVersion|DeviceGetCount|DeviceGet|DeviceGetAttribute|DeviceGetName|DeviceComputeCapability|CtxCreate_v2|CtxPushCurrent_v2|CtxPopCurrent_v2|CtxDestroy_v2|Memcpy2D_v2|GetErrorName)|CUDA_MEMCPY2D.*|CUmemorytype.*|CUdevice_attribute.*|cudaVideo.*|_?CUVID.*|tcuvid.*|CUvideo.*|_?NV_ENC.*|NVENC.*|_?NV_ENCODE.*|PNV.*|_?GUID' \
  --allowlist-var 'NVENCAPI_.*|NV_MAX_SEQ_HDR_LEN|NVENC_INFINITE_GOPLENGTH|CU_CTX_.*' \
  --blocklist-item 'NV_ENC_.*_GUID' \
  --default-enum-style consts --no-prepend-enum-name --no-doc-comments --with-derive-default --use-core --ctypes-prefix core::ffi \
  -- -I"$HDR" -I/usr/lib/gcc/x86_64-linux-gnu/15/include
