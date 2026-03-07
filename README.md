# fastIPAV

低遅延 AV-over-IP を想定した `tx` / `rx` 構成の Rust 実装である。  
主軸は `GStreamer` backend、映像 `H.264`、音声 `PCM/L16`、LAN 内 RTP/UDP multicast 配信である。

## 推奨 OS

- Raspberry Pi: `Raspberry Pi OS Bookworm 64bit`
- Linux PC: `Ubuntu LTS`
  - 現時点の開発確認は Ubuntu 24.04 LTS 系を基準にしている

## 配布方針

このリポジトリは、通常運用では「ソースを clone してローカルでビルドする」よりも、`GitHub Releases` に置いたビルド済みアーカイブを `scripts/install.sh` で取得して配置する使い方を想定している。

想定フロー:

1. 依存 package を入れる
2. `git clone`
3. `./scripts/install.sh`
4. 必要なら `systemctl enable --now ...`

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
- `scripts`: release 生成と install スクリプト

## クイックスタート

### Linux PC / Raspberry Pi 共通

依存 package を入れる。

```bash
sudo apt-get update
sudo apt-get install -y \
  curl \
  ca-certificates \
  git \
  tar \
  libasound2 \
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

リポジトリを取得して install スクリプトを実行する。

```bash
git clone git@github.com:ponpaku/fastIPAV.git
cd fastIPAV
./scripts/install.sh
```

依存もスクリプト側にやらせる場合:

```bash
./scripts/install.sh --install-deps
```

`tx` / `rx` の unit を同時に有効化したい場合:

```bash
./scripts/install.sh --enable-service both
```

`rx` だけ有効化したい場合:

```bash
./scripts/install.sh --enable-service rx
```

### install 後の配置先

- バイナリ: `/usr/local/bin/tx` `/usr/local/bin/rx`
- 共有設定例: `/usr/local/share/fastipav/configs/`
- 実運用設定: `/etc/avoverip/tx.toml` `/etc/avoverip/rx.toml`
- systemd unit: `/etc/systemd/system/avoverip-tx.service` `/etc/systemd/system/avoverip-rx.service`

既存の `/etc/avoverip/tx.toml` と `/etc/avoverip/rx.toml` は上書きしない。

## Raspberry Pi のセットアップ

Raspberry Pi は `Raspberry Pi OS Bookworm 64bit` を前提にする。  
受信では KMS/DRM 寄りの表示経路を優先する。

追加確認:

```bash
gst-inspect-1.0 kmssink
gst-inspect-1.0 avdec_h264
ls -l /dev/video*
```

補足:

- `scripts/install.sh` は Raspberry Pi を検出すると `configs/tx.pi.toml` と `configs/rx.pi.toml` を既定として `/etc/avoverip/` に配置する
- H.264 decoder は backend 側で `v4l2h264dec` などを優先し、無ければ `avdec_h264` へフォールバックする
- UVC キャプチャを使う場合は `video.device` を必要に応じて変更する

## Linux PC のセットアップ

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

## 起動例

送信の基本起動:

```bash
/usr/local/bin/tx --config /etc/avoverip/tx.toml
```

受信の基本起動:

```bash
/usr/local/bin/rx --config /etc/avoverip/rx.toml
```

音声込みで有効化する場合:

```bash
/usr/local/bin/tx --config /etc/avoverip/tx.toml --enable-audio
/usr/local/bin/rx --config /etc/avoverip/rx.toml --enable-audio
```

## systemd

unit 雛形は `systemd/` にある。`scripts/install.sh` は install 時に `/etc/systemd/system/` へ配置する。

手動で有効化する場合:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now avoverip-tx
sudo systemctl enable --now avoverip-rx
```

## 設定ファイル

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

## 動作確認

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

## release 生成

release アーカイブは `scripts/package-release.sh` で作る。

ホストと同じ architecture 向け:

```bash
source "$HOME/.cargo/env"
./scripts/package-release.sh --version v0.1.0
```

Raspberry Pi 向け `aarch64` release:

```bash
source "$HOME/.cargo/env"
rustup target add aarch64-unknown-linux-gnu
./scripts/package-release.sh --version v0.1.0 --target aarch64-unknown-linux-gnu
```

生成物:

- `dist/fastipav-v0.1.0-linux-x86_64.tar.gz`
- `dist/fastipav-v0.1.0-linux-aarch64.tar.gz`
- `dist/*.sha256`

## ソースからビルドしたい場合

開発用途では従来どおり `cargo build` も使える。

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  libasound2-dev \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev

curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cargo build
```

## 既知の制約

- `capture-to-display` は現状、設定値ベースの初期推定を返す
- 実機の遅延検証と UVC 入力確認は別途必要
- Raspberry Pi / Linux PC 向けの hardware codec 最適化は今後の調整余地がある
