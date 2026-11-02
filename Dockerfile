# SENCA のノード（REQ-003）。docker compose up -d で単一ノードが立ち上がる。
# ソフトウェアの H.264/H.265 コーデックは入れない（ADR-0004）。ロイヤリティフリーのコーデック（dav1d・libvpx・SVT-AV1・libopus）は
# ディストリビューションのパッケージを入れ、codec worker が実行時に読み込む。GPU は compose の設定で渡す。

FROM ubuntu:26.04 AS build
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/* \
 && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# リリース（M14）では --locked を付ける（今は作業中の部品が Cargo.lock に混ざることがある）
RUN cargo build --release -p senca-node -p senca-codec-worker \
 && mkdir -p /out && cp target/release/senca-node target/release/senca-codec-worker /out/

# 実行: libvpx の提供者は libvpx 1.16（libvpx.so.12）の ABI に合わせてあるので、それを持つ Ubuntu 26.04 を使う
FROM ubuntu:26.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata libdav1d7 libvpx12 libopus0 libsvtav1enc2 libva2 libva-drm2 libvpl2 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /var/lib/senca senca \
 && mkdir -p /var/lib/senca /opt/senca/console && chown senca /var/lib/senca
COPY --from=build /out/senca-node /out/senca-codec-worker /opt/senca/bin/
COPY apps/console /opt/senca/console
COPY apps/sender-web /opt/senca/sender
COPY docs/licenses /opt/senca/licenses
USER senca
WORKDIR /var/lib/senca
EXPOSE 8080/tcp 1935/tcp 9000/udp 8443/udp
VOLUME ["/var/lib/senca"]
HEALTHCHECK --interval=10s --timeout=3s CMD ["/opt/senca/bin/senca-node", "--healthcheck", "http://127.0.0.1:8080/readyz"]
ENTRYPOINT ["/opt/senca/bin/senca-node", "--data-dir", "/var/lib/senca", "--console-dir", "/opt/senca/console", "--sender-dir", "/opt/senca/sender", \
            "--codec-lib-dir", "/usr/lib/x86_64-linux-gnu"]
CMD ["--http", "0.0.0.0:8080", "--rtmp", "0.0.0.0:1935", "--srt", "0.0.0.0:9000"]
