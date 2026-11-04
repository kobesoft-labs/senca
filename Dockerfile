# SENCA のノード（REQ-003）。docker compose up -d で単一ノードが立ち上がる。
#
# 種類（--target）。どれも linux/amd64 と linux/arm64（Raspberry Pi 4・5 などの 64 bit OS）で作れる（gpu は amd64 だけ）。
#   slim      コーデックのライブラリを入れない。受け取った映像・音声をそのまま配信・録画・中継する（変換はしない）
#   standard  slim ＋ ロイヤリティフリーのコーデック（dav1d・libvpx・SVT-AV1・libopus）と VA-API・oneVPL の実行時ライブラリ。既定
#   gpu       standard ＋ GPU のドライバー（Intel の VA-API・oneVPL の GPU 実行時、AMD の VA-API）。NVIDIA は --gpus で渡す
#
# ソフトウェアの H.264/H.265/AAC コーデックは入れない（ADR-0004）。ライブラリは codec worker が実行時に読み込む。
#
# 自分でドライバー・ライブラリを足す方法:
#   1. ビルド時: --build-arg EXTRA_PACKAGES="パッケージ ..."（Ubuntu 26.04 のパッケージ。main・universe・multiverse から入る）
#   2. 派生イメージ: FROM ghcr.io/kobesoft-labs/senca:<版> で USER root にして apt-get install、最後に USER senca
#   3. 実行時: 共有ライブラリ（.so）を置いたディレクトリを /opt/senca/lib にマウントする（読み込み先の先頭）

ARG UBUNTU=ubuntu:26.04

FROM ${UBUNTU} AS build
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/* \
 && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p senca-node -p senca-codec-worker -p senca-capture \
 && mkdir -p /out && cp target/release/senca-node target/release/senca-codec-worker target/release/senca-capture-worker /out/ \
 && rm -rf target

# ---------------------------------------------------------------- slim
FROM ${UBUNTU} AS slim
ARG EXTRA_PACKAGES=""
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata ${EXTRA_PACKAGES} \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /var/lib/senca senca \
 && mkdir -p /var/lib/senca /opt/senca/console /opt/senca/lib && chown senca /var/lib/senca
COPY --from=build /out/senca-node /out/senca-codec-worker /out/senca-capture-worker /opt/senca/bin/
COPY apps/console /opt/senca/console
# 管理画面の js/vendor/senca-sdk は ../../../sdk-js/src へのリンク（部屋の試しの参加でブラウザー SDK をそのまま使う）
COPY apps/sdk-js/src /opt/senca/sdk-js/src
COPY apps/sender-web /opt/senca/sender
COPY docs/licenses /opt/senca/licenses
# /opt/senca/lib: 利用者が足すライブラリ（マウント）。コーデック・キャプチャのライブラリはここを先に探し、無ければシステムの既定で探す。
# NVIDIA_*: NVIDIA Container Toolkit（docker run --gpus）に、NVENC/NVDEC のライブラリを入れてもらうための指定
ENV SENCA_CODEC_LIB_DIR=/opt/senca/lib \
    SENCA_CAPTURE_LIB_DIR=/opt/senca/lib \
    LD_LIBRARY_PATH=/opt/senca/lib \
    NVIDIA_DRIVER_CAPABILITIES=compute,video,utility
LABEL org.opencontainers.image.title="SENCA" \
      org.opencontainers.image.description="Media server for live streaming, recording and real-time communication" \
      org.opencontainers.image.source="https://github.com/kobesoft-labs/senca" \
      org.opencontainers.image.licenses="Apache-2.0"
USER senca
WORKDIR /var/lib/senca
EXPOSE 8080/tcp 1935/tcp 9000/udp 8443/udp
VOLUME ["/var/lib/senca"]
HEALTHCHECK --interval=10s --timeout=3s CMD ["/opt/senca/bin/senca-node", "--healthcheck", "http://127.0.0.1:8080/readyz"]
ENTRYPOINT ["/opt/senca/bin/senca-node", "--data-dir", "/var/lib/senca", "--console-dir", "/opt/senca/console", "--sender-dir", "/opt/senca/sender"]
# WebRTC は UDP 8443。ICE の候補には --webrtc-public-ip の IP を載せる（無ければコンテナのインターフェースのアドレス。
# ブリッジのネットワークでは外から届かないので、docker run の引数か compose の SENCA_PUBLIC_IP で渡す）
CMD ["--http", "0.0.0.0:8080", "--rtmp", "0.0.0.0:1935", "--srt", "0.0.0.0:9000", "--webrtc-udp", "0.0.0.0:8443"]

# ---------------------------------------------------------------- standard
# libvpx の提供者は libvpx 1.16（libvpx.so.12）の ABI に合わせてあるので、それを持つ Ubuntu 26.04 を使う。
# libvpl2（oneVPL の dispatcher）は Intel の amd64 だけにある
FROM slim AS standard
ARG TARGETARCH
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends libdav1d7 libvpx12 libopus0 libsvtav1enc2 libva2 libva-drm2 \
      $([ "$TARGETARCH" = amd64 ] && echo libvpl2) \
 && rm -rf /var/lib/apt/lists/*
USER senca

# ---------------------------------------------------------------- gpu（amd64）
# intel-media-va-driver: Intel の VA-API（iHD）。libmfx-gen1.2: oneVPL の Intel GPU の実行時。mesa-libgallium: AMD の VA-API（radeonsi）。
# 対応するエンコードの多い非自由版は --build-arg INTEL_VA_DRIVER=intel-media-va-driver-non-free（multiverse）。
# AMF は AMD のドライバーの実行時ライブラリを /opt/senca/lib にマウントする
FROM standard AS gpu
ARG INTEL_VA_DRIVER=intel-media-va-driver
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends ${INTEL_VA_DRIVER} libmfx-gen1.2 mesa-libgallium \
 && rm -rf /var/lib/apt/lists/*
USER senca

# 既定（--target を付けないとき）は standard
FROM standard
