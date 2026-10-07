# fastIPAV

低遅延 AV-over-IP を想定した `tx` / `rx` 構成の Rust 実装である。  
主軸は `GStreamer` backend、映像 `H.264`、音声 `PCM/L16`、LAN 内 RTP/UDP multicast 配信である。

## 推奨 OS

- Raspberry Pi: `Raspberry Pi OS Bookworm 64bit`
- Linux PC: `Ubuntu 22.04 LTS` 以降
  - CI は Ubuntu 22.04 / 24.04 x86_64 と Ubuntu 22.04 arm64 で確認する

## 配布方針

このリポジトリは、通常運用では「ソースを clone してローカルでビルドする」よりも、`GitHub Releases` に置いたビルド済みアーカイブを `scripts/install.sh` で取得して配置する使い方を想定している。installer は同梱の SHA-256 チェックサムを検証してから展開する。

想定フロー:

1. 依存 package を入れる
2. `git clone`
3. `bash scripts/install.sh`
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
git clone https://github.com/ponpaku/fastIPAV.git
cd fastIPAV
bash scripts/install.sh
```

依存もスクリプト側にやらせる場合:

```bash
bash scripts/install.sh --install-deps
```

`tx` / `rx` の unit を同時に有効化したい場合:

```bash
bash scripts/install.sh --enable-service both
```

Raspberry PiなどKMS/DRMで動かす `rx` だけをsystem serviceとして有効化したい場合:

```bash
bash scripts/install.sh --enable-service rx
```

Linux desktopでWayland/X11へ表示するRXは、system serviceではdisplay session環境が不足しやすい。通常はログイン中のgraphical sessionから `rx` を起動するか、環境に合わせたuser serviceを用意する。

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
- Raspberry Pi 5 はH.264 hardware codecを持たないため `avdec_h264` などのsoftware decoderを優先する。旧Piでは利用可能なら `v4l2h264dec` を優先する
- Pi送信の`encoder_element = "auto"`は、Pi 4以前で利用可能なら`v4l2h264enc`、Pi 5系では`x264enc`を選ぶ
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

- desktop rendererは表示sessionに応じて `waylandsink` → `sdlvideosink` → `ximagesink` を選び、適合する明示sinkが無い場合のみ `autovideosink` を使う
- UVC 入力が見えているかは `ls -l /dev/video*` で確認する
- 音声入出力は `arecord -l` `aplay -l` で確認する
- TX audioは既定でALSA driver timestampではなくpipeline clockを使い、videoの`do-timestamp=true`と同じclock domainへ寄せる

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
bash tools/fetch-stats.sh 127.0.0.1:8082
```

デバイスを使わず、loopback multicast で tx/rx の実パイプラインを確認するスモークテスト:

```bash
bash scripts/smoke-test.sh
```

このテストは MJPEG 入力相当の映像と synthetic audio を生成し、TX の H.264 encode / RTP-L16 packetize 後と RX の H.264 decode / L16 depay 後の双方で実際に media buffer が通過したことを確認する。単に pipeline が `Playing` になっただけでは成功扱いしない。

`/healthz` は media がreadyになるまで HTTP 503 を返し、ready後はHTTP 200と `ok=true` を返す。audio無効時はvideo、audio有効時はvideo/audio双方の実buffer到達がready条件になる。mediaが一定時間停止した場合はpipelineを再起動する。

設定ファイルは起動時に検証される。multicast address、RTP port / payload type、映像サイズ・fps、HTTP bind、audio parameter などが不正な場合は pipeline 構築前にエラーで終了する。

`/stats` の主な項目:

- `estimated_capture_to_display_ms`
- `estimated_av_sync_ms`
- `estimated_audio_offset_ms`
- `pipeline_restarts`
- `audio_underruns`
- `qos_events`
- `dropped_frames`
- `dropped_audio_chunks`

## release 生成

`v*` タグを push すると GitHub Actions が x86_64 / aarch64 のネイティブ runner で release package を生成し、GitHub Release に `.tar.gz` と `.sha256` を公開する。

手元で作る場合は `scripts/package-release.sh` を使う。

ホストと同じ architecture 向け:

```bash
source "$HOME/.cargo/env"
bash scripts/package-release.sh --version v0.1.0
```

Raspberry Pi 向け `aarch64` release は、GStreamerなどのnative libraryへリンクするため、単純なRust target追加だけではクロスビルドしない。aarch64 Linuxホスト上で実行するか、GitHub ActionsのRelease workflowを使う。

aarch64 Linuxホスト上:

```bash
source "$HOME/.cargo/env"
bash scripts/package-release.sh --version v0.1.0
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
- `estimated_av_sync_ms` / `estimated_audio_offset_ms` は設定したvideo/audio jitter buffer差に基づく推定値。video/audioは独立RTPストリームで、RTCP/rtpbinによるsender-clock同期はまだ実装していないため、実測A/V同期値としては扱わない
- 実機の遅延検証と UVC 入力確認は別途必要
- Raspberry Pi / Linux PC 向けの hardware codec 最適化は今後の調整余地がある
- systemdのRXをLinux desktopで使う場合、display sessionの環境や権限は環境依存。KMS/DRMを使うRaspberry Piとは条件が異なる


## CI

Pull Request と `main` への push では、GitHub Actions で以下を実行する。

- shell script の構文検証
- `cargo fmt --check` / `clippy -D warnings`
- workspace 全体の `cargo check`
- unit test（配布する全TOMLとproduction sink/pipelineのparse検証を含む）
- GStreamer を使った tx/rx loopback smoke test（映像・音声の実buffer通過を確認）
- Ubuntu 22.04 x86_64 / arm64 でrelease package生成・checksum・manifest・local install検証

## License

MIT License。詳細は `LICENSE` を参照。
