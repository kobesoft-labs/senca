# ADR-0008 コンテナでの導入

- 状態: 採用（2026-11-02）
- 関係: REQ-003、REQ-004、ADR-0004

## 決定

- `Dockerfile`（2 段: Rust でビルド → Ubuntu 26.04。ロイヤリティフリーのコーデックの実行時のライブラリ libvpx 1.16 などに合わせる）と `compose.yaml`。`docker compose up -d` で単一ノードが立ち上がる。
- 実行は非 root（uid 10001）、読み取り専用のルート、`no-new-privileges`。データは named volume（`/var/lib/senca`）。
- 初回起動で管理者トークンを作り、`/var/lib/senca/control/bootstrap-admin-token`（0600）に置く。既定のパスワードは持たない。
- ソフトウェアの H.264/H.265 コーデックは入れない。ロイヤリティフリーのコーデック（dav1d・libvpx・SVT-AV1・libopus）はディストリビューションのパッケージを入れ、codec worker が実行時に読み込む。
- GPU: NVIDIA は NVIDIA Container Toolkit と `deploy.resources.reservations.devices`、Intel/AMD は `/dev/dri` を渡す（compose に例を書いた）。ドライバーはホストのもの。
- ヘルスチェックは `senca-node --healthcheck http://127.0.0.1:8080/readyz`（イメージに curl を入れない）。
- イメージはコミット済みのソースから作る（`git archive HEAD | docker build -`）。作業ツリーの途中の状態を混ぜない。
- 署名と SBOM は M14（リリースの手順）で付ける。
