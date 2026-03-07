# fastIPAV

低遅延 AV-over-IP を想定した `tx` / `rx` 構成の Rust 実装である。  
現状の主軸は `GStreamer` backend、映像 `H.264`、音声 `PCM/L16`、LAN 内 multicast 配信である。

## 推奨 OS

- Raspberry Pi: `Raspberry Pi OS Bookworm 64bit`
- Linux PC: `Ubuntu LTS`
  - 開発確認は Ubuntu 24.04 LTS 系を想定

## できること

- `tx` / `rx` を別バイナリで提供
- 設定ファイルは TOML
- 映像は RTP/UDP multicast
- 音声は ALSA / PCM(L16) を別 RTP ストリームで追加可能
- `/healthz` `/stats` を HTTP で提供
- pipeline 異常時の最小限の再起動 supervisor を実装

## リポジトリ構成

- `common`: 設定、監視、メトリクス、ネットワーク補助
- `backends/gst`: GStreamer backend
- `tx`: 送信バイナリ
- `rx`: 受信バイナリ
- `configs`: 設定例
- `systemd`: service 雛形
- `tools`: 補助スクリプト

## セットアップ

### 1. 共通 package

Raspberry Pi / Linux PC のどちらでも、まず以下を導入する。

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  curl \
  git \
  libasound2-dev \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-tools \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav \
  gstreamer1.0-alsa \
  gstreamer1.0-gl \
  gstreamer1.0-x \
  v4l-utils \
  alsa-utils
```

Rust toolchain は `rustup` で入れる。

```bash
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

リポジトリを取得してビルドする。

```bash
git clone git@github.com:ponpaku/fastIPAV.git
cd fastIPAV
source "$HOME/.cargo/env"
cargo build
```

### 2. Linux PC のセットアップ

Linux PC は開発機兼、送信機・受信機のどちらにも使う前提である。

追加確認:

```bash
gst-inspect-1.0 x264enc
gst-inspect-1.0 rtph264pay
gst-inspect-1.0 avdec_h264
gst-inspect-1.0 waylandsink
```

補足:

- `sdl2sink` が無い環境では、実装側で `waylandsink` / `ximagesink` / `autovideosink` に自動フォールバックする
- UVC 入力が見えているかは `ls -l /dev/video*` で確認する
- 音声入出力は `arecord -l` `aplay -l` で確認する

送信の基本起動:

```bash
source "$HOME/.cargo/env"
./target/debug/tx --config configs/tx.default.toml
```

受信の基本起動:

```bash
source "$HOME/.cargo/env"
./target/debug/rx --config configs/rx.default.toml
```

音声込みで有効化する場合:

```bash
./target/debug/tx --config configs/tx.default.toml --enable-audio
./target/debug/rx --config configs/rx.default.toml --enable-audio
```

### 3. Raspberry Pi のセットアップ

Raspberry Pi は `Raspberry Pi OS Bookworm 64bit` を前提にする。  
受信では KMS/DRM 寄りの表示経路を優先する。

追加確認:

```bash
gst-inspect-1.0 kmssink
gst-inspect-1.0 avdec_h264
ls -l /dev/video*
```

補足:

- `configs/rx.pi.toml` は `renderer = "kms_drm"` を既定にしている
- H.264 decoder は backend 側で `v4l2h264dec` などを優先し、無ければ `avdec_h264` へフォールバックする
- UVC キャプチャを使う場合は、必要に応じて `video.device` を `/dev/video0` 以外へ変更する

送信の基本起動:

```bash
source "$HOME/.cargo/env"
./target/debug/tx --config configs/tx.pi.toml
```

受信の基本起動:

```bash
source "$HOME/.cargo/env"
./target/debug/rx --config configs/rx.pi.toml
```

### 4. 設定ファイル

主要な設定例:

- Linux PC 送信: `configs/tx.default.toml`
- Linux PC 受信: `configs/rx.default.toml`
- Raspberry Pi 送信: `configs/tx.pi.toml`
- Raspberry Pi 受信: `configs/rx.pi.toml`
- デバイス無しのスモークテスト: `configs/tx.smoketest.toml` `configs/rx.smoketest.toml`

主な既定値:

- multicast group: `239.255.10.10`
- video port: `5004`
- audio port: `5006`
- interface: `auto`
- TTL: `1`
- HTTP bind: `127.0.0.1`

### 5. 動作確認

ヘルス確認:

```bash
curl -fsS http://127.0.0.1:8081/healthz
curl -fsS http://127.0.0.1:8082/healthz
```

統計確認:

```bash
curl -fsS http://127.0.0.1:8081/stats
curl -fsS http://127.0.0.1:8082/stats
./tools/fetch-stats.sh 127.0.0.1:8082
```

`/stats` の主な項目:

- `estimated_capture_to_display_ms`
- `estimated_av_sync_ms`
- `estimated_audio_offset_ms`
- `pipeline_restarts`
- `audio_underruns`
- `dropped_frames`
- `dropped_audio_chunks`

### 6. systemd

雛形は以下に置いてある。

- `systemd/avoverip-tx.service`
- `systemd/avoverip-rx.service`

例:

```bash
sudo install -D -m 0644 systemd/avoverip-tx.service /etc/systemd/system/avoverip-tx.service
sudo install -D -m 0644 systemd/avoverip-rx.service /etc/systemd/system/avoverip-rx.service
sudo mkdir -p /etc/avoverip
sudo cp configs/tx.default.toml /etc/avoverip/tx.toml
sudo cp configs/rx.default.toml /etc/avoverip/rx.toml
sudo systemctl daemon-reload
sudo systemctl enable --now avoverip-tx
sudo systemctl enable --now avoverip-rx
```

### 7. 実機テスト前の確認項目

- `cargo check` と `cargo build` が通る
- `gst-inspect-1.0 x264enc` が通る
- `gst-inspect-1.0 avdec_h264` が通る
- 送信側で `/dev/video*` が見える
- 受信側で使用する sink が使える
  - Linux PC: `waylandsink` または `ximagesink`
  - Raspberry Pi: `kmssink`

## 既知の制約

- `capture-to-display` は現状、設定値ベースの初期推定を返す
- 実機の遅延検証と UVC 入力確認は別途必要
- Raspberry Pi / Linux PC 向けの hardware codec 最適化は今後の調整余地がある
