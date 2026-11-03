# 16. Container images

[日本語](../ja/16-containers.md)

Container images of the SENCA node are published on GitHub Container Registry as `ghcr.io/kobesoft-labs/senca`.
There are three variants, for amd64 (typical servers) and arm64 (Raspberry Pi 4 and 5 and similar boards).
You can add GPU drivers and your own libraries on top.

## 16.1 Variants and tags

| Variant | Tags | Architectures | Contents |
|---|---|---|---|
| standard | `latest`, `<version>` (e.g. `0.0.2`) | amd64, arm64 | slim + royalty-free codecs (dav1d, libvpx, SVT-AV1, libopus) and the VA-API runtime (plus the oneVPL dispatcher on amd64) |
| slim | `slim`, `<version>-slim` | amd64, arm64 | The node only, without codec libraries |
| gpu | `gpu`, `<version>-gpu` | amd64 | standard + GPU drivers (Intel VA-API and the oneVPL GPU runtime, AMD VA-API) |

- The latest `main` branch is published as `edge`, `edge-slim` and `edge-gpu`.
- `docker pull` picks the image that matches your machine's architecture.

Choosing a variant:

- **Delivering, recording and relaying what you receive** works with slim. H.264, H.265 and AAC are passed through without
  transcoding ([4. Ingest and playback](04-ingest-playback.md)). It is the smallest image and suits a Raspberry Pi.
- **AV1, VP8, VP9 and Opus transcoding**, and pipelines ([7. Pipelines](07-pipelines.md)) on the CPU, need standard.
- **H.264 and H.265 transcoding** needs a GPU ([14. Codecs and patents](14-codecs-and-patents.md)). Use gpu for Intel and AMD
  GPUs; NVIDIA GPUs work with any variant ([16.4](#164-using-gpus)).

## 16.2 Running

```sh
docker run -d --name senca \
  -p 8080:8080 -p 1935:1935 -p 9000:9000/udp -p 8443:8443/udp \
  -v senca-data:/var/lib/senca \
  ghcr.io/kobesoft-labs/senca:latest \
  --http 0.0.0.0:8080 --rtmp 0.0.0.0:1935 --srt 0.0.0.0:9000 --webrtc-udp 0.0.0.0:8443 \
  --webrtc-public-ip 203.0.113.10 \
  --public-http-url http://203.0.113.10:8080 --public-rtmp-url rtmp://203.0.113.10:1935
docker exec senca cat /var/lib/senca/control/bootstrap-admin-token
```

- Arguments after the image name are node options ([3. Configuration](03-configuration.md)). Without any, the node starts
  with `--http 0.0.0.0:8080 --rtmp 0.0.0.0:1935 --srt 0.0.0.0:9000 --webrtc-udp 0.0.0.0:8443`. If you pass arguments,
  include all the listeners you need.
- `--data-dir /var/lib/senca`, `--console-dir` and `--sender-dir` are set by the image.
- Replace `203.0.113.10` with the host address that clients see. Without `--webrtc-public-ip`, WebRTC (WHIP, WHEP, rooms)
  cannot be reached from outside the host.

With `compose.yaml` in the repository, choose the variant with `SENCA_IMAGE`:

```sh
SENCA_PUBLIC_IP=203.0.113.10 docker compose up -d
SENCA_IMAGE=ghcr.io/kobesoft-labs/senca:slim docker compose up -d
```

## 16.3 Raspberry Pi

The `linux/arm64` images run on a Raspberry Pi 4 or 5 with a 64-bit OS (64-bit Raspberry Pi OS, or Ubuntu for arm64).
32-bit operating systems are not supported.

```sh
docker run -d --name senca --restart unless-stopped \
  -p 8080:8080 -p 1935:1935 -p 9000:9000/udp \
  -v senca-data:/var/lib/senca \
  --device /dev/video0 --group-add "$(getent group video | cut -d: -f3)" \
  ghcr.io/kobesoft-labs/senca:slim \
  --http 0.0.0.0:8080 --rtmp 0.0.0.0:1935 --srt 0.0.0.0:9000 \
  --public-http-url http://raspberrypi.local:8080 --public-rtmp-url rtmp://raspberrypi.local:1935
```

- The Raspberry Pi's hardware codec (the V4L2 encoder) is not supported, so H.264 and H.265 cannot be transcoded. Delivering,
  recording and relaying what you receive (including joining a grid as a node) works as usual.
- Pass USB cameras and capture devices with `--device /dev/video0` (capture devices in [6. Inputs and outputs](06-inputs-outputs.md)),
  and audio (ALSA) with `--device /dev/snd`. The container runs as uid 10001, so add the device's group (`video` in the
  example) with `--group-add`.
- Continuous recording wears out SD cards; use a USB SSD for the volume, or S3-compatible storage ([5. Recording](05-recording.md)).

## 16.4 Using GPUs

| GPU | Variant | How to pass it |
|---|---|---|
| NVIDIA (NVENC, NVDEC) | any | Install the NVIDIA driver and the NVIDIA Container Toolkit on the host and use `--gpus all`. The toolkit adds the libraries to the container (the image sets `NVIDIA_DRIVER_CAPABILITIES=compute,video,utility`) |
| Intel (VA-API, oneVPL) | gpu | `--device /dev/dri --group-add "$(getent group render \| cut -d: -f3)"` |
| AMD (VA-API) | gpu | Same as Intel |
| AMD (AMF) | gpu | As above, plus mount the AMF runtime from AMD's driver (`libamfrt64.so.1`) at `/opt/senca/lib` ([16.5](#165-adding-drivers-and-libraries)) |

Check what is usable on the console's Nodes and devices page or with `GET /api/v1/devices`. Unusable providers show the
library that could not be loaded and why.

## 16.5 Adding drivers and libraries

There are three ways.

### Mount at run time

Mount a directory of shared libraries (`.so`) at `/opt/senca/lib`. The codec and capture workers look there first and then
in the system's default locations. The directory is also on `LD_LIBRARY_PATH`, so libraries that the added ones depend on
can go in the same place.

```sh
docker run ... -v /opt/amdgpu-pro/lib/x86_64-linux-gnu:/opt/senca/lib:ro ghcr.io/kobesoft-labs/senca:gpu
```

This suits libraries that come with host drivers, such as DeckLink's `libDeckLinkAPI.so` or AMD's AMF runtime.
Only the library names (sonames) known to each provider are loaded ([14. Codecs and patents](14-codecs-and-patents.md)).

### Derive an image

Add Ubuntu 26.04 packages or files on top of a published image:

```dockerfile
FROM ghcr.io/kobesoft-labs/senca:gpu
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends intel-media-va-driver-non-free \
 && rm -rf /var/lib/apt/lists/*
COPY mylibs/ /opt/senca/lib/
USER senca
```

Switch back to `USER senca` at the end; the node is designed not to run as root.

### Build from source

Use the repository's `Dockerfile`, choosing the variant with `--target` and extra packages with `--build-arg`:

```sh
docker build --target slim -t my/senca:slim .
docker build --target standard --build-arg EXTRA_PACKAGES="alsa-utils v4l-utils" -t my/senca .
docker build --target gpu --build-arg INTEL_VA_DRIVER=intel-media-va-driver-non-free -t my/senca:gpu .
docker buildx build --platform linux/arm64 --target slim -t my/senca:slim-arm64 .   # another architecture (needs QEMU)
```

| Argument | Default | Meaning |
|---|---|---|
| `EXTRA_PACKAGES` | none | Ubuntu 26.04 packages added to every variant (space-separated; from main, universe, restricted and multiverse) |
| `INTEL_VA_DRIVER` | `intel-media-va-driver` | Intel VA-API driver in gpu. `intel-media-va-driver-non-free` supports more encoders |
| `UBUNTU` | `ubuntu:26.04` | Base image. The libvpx provider expects `libvpx.so.12`, so VP8 and VP9 are unavailable on releases older than 26.04 |

## 16.6 What is in the image

| Item | Value |
|---|---|
| Base | Ubuntu 26.04 |
| Executables | `/opt/senca/bin/senca-node`, `senca-codec-worker`, `senca-capture-worker` |
| Extra libraries | `/opt/senca/lib` (empty; set in `SENCA_CODEC_LIB_DIR`, `SENCA_CAPTURE_LIB_DIR` and `LD_LIBRARY_PATH`) |
| User | `senca` (uid 10001) |
| Data | volume at `/var/lib/senca` |
| Console and sender page | `/opt/senca/console` (`/console/`), `/opt/senca/sender` (`/sender/`) |
| Health check | `senca-node --healthcheck http://127.0.0.1:8080/readyz` every 10 s |
| Exposed ports | 8080/tcp (HTTP), 1935/tcp (RTMP), 9000/udp (SRT), 8443/udp (WebRTC) |
| Dependency licenses | `/opt/senca/licenses` |

The images are built by GitHub Actions (`.github/workflows/docker.yml`), each architecture on a machine of that
architecture. Pushing a tag that starts with `v` updates the version tags and `latest`; pushing to `main` updates `edge`.
