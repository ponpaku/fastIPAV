# fastIPAV

低遅延 AV-over-IP を想定した `tx` / `rx` 構成の Rust 実装である。  
主軸は `GStreamer` backend、映像 `H.264`、音声 `PCM/L16`、LAN 内 RTP/UDP multicast 配信である。

## 推奨 OS

- Raspberry Pi: `Raspberry Pi OS Bookworm 64bit`
- Linux PC: `Ubuntu 22.04 LTS` 以降
  - CI は Ubuntu 22.04 / 24.04 x86_64、Ubuntu 22.04 arm64、Debian 12 arm64 で確認する

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

既存の `/etc/avoverip/tx.toml` と `/etc/avoverip/rx.toml` は上書きしない。upgrade時は新binaryで既存config/pipelineを事前検証し、既にactiveなsystemd serviceは新binaryへ自動restartする。inactiveなserviceは勝手にenableしない。

## Raspberry Pi のセットアップ

Raspberry Pi は `Raspberry Pi OS Bookworm 64bit` を前提にする。  
受信では KMS/DRM 寄りの表示経路を優先する。
既定のKMS sinkは既存display modeを強制変更しない。特殊なmode設定が必要な場合のみ `video.sink_element` で `kmssink force-modesetting=true` などを明示する。

追加確認:

```bash
gst-inspect-1.0 kmssink
gst-inspect-1.0 avdec_h264
ls -l /dev/video*
v4l2-ctl --device /dev/video0 --list-formats-ext
```

補足:

- `scripts/install.sh` は Raspberry Pi を検出すると `configs/tx.pi.toml` と `configs/rx.pi.toml` を既定として `/etc/avoverip/` に配置する
- Raspberry Pi 5 はH.264 hardware codecを持たないため `avdec_h264` などのsoftware decoderを優先する。旧Piでは利用可能なら `v4l2h264dec` を優先する
- Pi送信の`encoder_element = "auto"`は、Pi 4以前で利用可能なら`v4l2h264enc`、Pi 5系では`x264enc`を選ぶ。V4L2 hardware codecが起動できない、または初回映像を生成できない場合はsoftware codecへfallbackする
- Pi 4以前のV4L2 H.264 encoderでは1080p30向けにH.264 level 4を明示する
- UVC キャプチャを使う場合は `video.device` を必要に応じて変更する。複数video deviceがある環境では `/dev/v4l/by-id/...` の安定したsymlinkを推奨する
- `source_caps` が要求する解像度/fps/formatをcapture deviceが実際に提供するか、`v4l2-ctl --list-formats-ext` で確認する

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

- `renderer = "auto"` はdisplay sessionに応じて `waylandsink` → `sdlvideosink` → `ximagesink` を優先し、明示的なvideo sinkが使えない環境では起動失敗として扱う。`autovideosink` のfake-sink fallbackは使わない。明示 `renderer = "sdl"` は `sdlvideosink` を固定指定する
- UVC 入力が見えているかは `ls -l /dev/video*` で確認する
- 音声入出力は `arecord -l` `aplay -l` で確認する
- TX audioは既定でALSA driver timestampではなくpipeline clockを使い、videoの`do-timestamp=true`と同じclock domainへ寄せる

## 起動例

installerがsystemd unitを配置する場合、実運用configは `root:avoverip` / `0640` にする。一般ユーザーが `/etc/avoverip/*.toml` を直接使って手動起動する場合は、そのユーザーを `avoverip` groupへ追加して再ログインする:

```bash
sudo usermod -aG avoverip "$USER"
```

capture/audio deviceも手動ユーザーから使う場合は、環境に応じて `video` / `audio` group権限も必要。

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

- observability HTTPは認証を持たないため、既定の `127.0.0.1` bindを推奨する。非loopbackへbindする場合はfirewall/VPN等で到達範囲を制限する
- multicast group: `239.255.10.10`
- video port: `5004`
- audio port: `5006`
- interface: `auto`（activeなmulticast対応interfaceが1本だけなら自動選択する。EthernetとWi‑Fiなど複数候補が同時にupの場合は誤送信を避けるため起動せず、明示指定が必要）
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

`/healthz` は media がreadyになるまで HTTP 503 を返し、ready後はHTTP 200と `ok=true` を返す。audio無効時はvideo、audio有効時はvideo/audio双方の実buffer到達がready条件になる。mediaが一定時間停止した場合はpipelineを再起動する。RXはRTP ingressとdecode後bufferを別々に監視するため、送信機がofflineなら待機し、RTPは届いているのにdecode outputが出ない場合は異常として復旧する。

設定ファイルは起動時に検証される。multicast address、RTP port / payload type、映像サイズ・fps、HTTP bind、audio parameter などが不正な場合は pipeline 構築前にエラーで終了する。

install/upgrade時は新しい `tx` / `rx` の `--check-config` で、保持中の実運用TOMLとGStreamer pipelineのparse可否を先に検証してからbinaryを置き換える。

`/stats` の主な項目:

- `estimated_capture_to_display_ms`
- `estimated_av_sync_ms`
- `estimated_audio_offset_ms`
- `pipeline_restarts`
- `audio_underruns`
- `qos_events`
- `dropped_frames`
- `dropped_audio_chunks`

`qos_events` はGStreamer QoS messageの発生数。`dropped_frames` / `dropped_audio_chunks` はwarning文言から分類した診断カウンタであり、厳密なRTP packet loss数やsinkの累積drop統計ではない。

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

## 映像の最小遅延を優先する設定

TXのRaspberry Pi向け構成では、`[video].capture_io_mode = "mmap"` を指定し、V4L2のメモリマップ式キャプチャを優先する。ドライバーが `mmap` をサポートせず起動/入力に失敗した場合は、TXがGStreamerの `io-mode=auto` で再試行する。Linux PCでは引き続き `auto` が既定となる。カスタムの `video.source_element` を指定する場合、この設定は適用されない。その他の選択肢は `rw`、`userptr`、`dmabuf`、`dmabuf-import`（デバイスが対応する場合のみ）である。

**注意**: `mmap` はユーザー空間の `read()` によるコピーを避けられる場合があるが、GStreamerの `auto` も既にmmapを選んでいることがある。選択を固定しただけでCPU使用率や遅延が改善すると断言しない。また `v4l2src` にはドライバー内部のバッファ数を共通に1枚へ固定する標準プロパティはなく、デバイスの必要バッファ数も異なるため、内部プールの枚数は変更しない。TXのキャプチャ直後のキューはすでに1フレーム・旧フレーム破棄方式で、キュー信号の送出も無効化している。

既定のソフトウェアH.264エンコードは`x264enc tune=zerolatency speed-preset=ultrafast`を使用する。`zerolatency`にはBフレーム、フレーム先読み、スレッド先読みを抑える設定が含まれるため、それらを重複指定しても遅延はさらに短縮されない。x264の`byte-stream=true`出力はAU単位で`rtph264pay`に直接接続し、従来の`h264parse`を省略する。V4L2ハードウェアエンコーダーと任意のカスタムエンコーダー経路には、形式変換・SPS/PPS補助のため`h264parse`を維持する。

エンコーダー周辺の待ち時間を調べる場合は、GStreamerの任意指定トレーサーを使用できる（通常運用では無効）。たとえばサービス停止後にTX実行ホストで以下のように起動する。

```bash
GST_TRACERS='latency(flags=pipeline+element+reported)' \
GST_DEBUG='GST_TRACER:7' \
tx --config /etc/avoverip/tx.toml 2>tx-latency-trace.log
```

トレーサーで確認するのはTXホスト内の要素間待ち時間。実機のcapture-to-display遅延、ディスプレイ遅延やネットワークの変動はこれだけでは測れない。また、測定自体が処理負荷に影響する場合がある。同一機材・映像条件で変更前後のフレーム遅延、CPU負荷、フレーム落ちを比較する。
ソフトウェアH.264復号では`avdec_h264 thread-type=slice`を低遅延候補として採用する。libavcodecの`frame`スレッド方式は複数フレームの出力待ちを増やすため、`slice`でフレーム並列化を避ける。H.264ストリームのスライス構成によってはCPU並列性能が下がり、1080p30を維持できない場合もある。旧PiのV4L2ハードウェアデコーダー、明示されたカスタムデコーダーは変更しない。インストーラーは既存の`rx.toml`を保持するため、既存環境で試す場合は`decoder_element = "avdec_h264 thread-type=slice"`と明示する。比較用の従来設定は`decoder_element = "avdec_h264"`。両者を同じ入力条件で遅延・CPU使用率・フレーム落ち・復旧動作まで比較する。

既定の受信映像は `sync=false` で、表示タイムスタンプまで待たず、復号したフレームをすぐ描画へ送る。映像用のRTPジッターバッファも既定で **10ms** に抑えている。RXの表示直前キューは **1フレーム** とし、表示が詰まったときは古いフレームを捨てて最新の映像を優先する。

Raspberry Pi 4系でV4L2 H.264デコーダーとKMS/DRMを組み合わせる場合は、CPUの `videoconvert` を挟まずにデコード出力から直接KMSへ渡す経路を試す。DRM/バッファ形式が合わずGStreamerのネゴシエーションに失敗した場合は、自動デコーダー設定ではソフトウェアデコードへフォールバックする。**ゼロコピーが実際に成立するかはドライバーと実機次第**。Pi 5はH.264のハードウェアデコード/エンコードを前提とせず、通常はソフトウェア経路になる。

`video.sync=false` は低遅延優先であり、画面の表示間隔や音声とのタイミングの安定性を犠牲にする場合がある。表示の滑らかさ・同期を優先するときはRX設定の `video.sync=true` に戻す。ジッターの多いネットワークでは `video.jitter_latency_ms` を10msから増やせるが、バッファ待ち時間も増える。

これらは**遅延を短くするための構成上の変更**であり、capture-to-displayが何msになったかの実測値ではない。実機での入力キャプチャ、H.264符号化、実ネットワーク、DRM表示、ディスプレイ内部処理は別に測定する必要がある。

## 実機受入試験

CI 合格後も、実機での表示・音声・長時間安定性・リンク障害からの復旧を確認する。実施手順と記録テンプレートは [実機受入試験](docs/hardware-validation.md) を参照する。

## 既知の制約

- `estimated_capture_to_display_ms` はend-to-end測定probe未実装のため現状 `null`。RX jitterだけからcapture-to-display値を捏造しない
- `estimated_av_sync_ms` / `estimated_audio_offset_ms` は設定したvideo/audio jitter buffer差に基づく推定値。video/audioは独立RTPストリームで、RTCP/rtpbinによるsender-clock同期はまだ実装していないため、実測A/V同期値としては扱わない
- RXは送信元/SSRCを選別しない。複数TXを同時運用する場合はstreamごとにmulticast groupまたはRTP portを分け、同じgroup+portへ複数送信しない
- RTP/UDP multicastには再送/FEC/暗号化を実装していない。packet loss耐性より低遅延を優先する構成で、信頼できるLANを前提とする
- TXの`/healthz`はローカルのcapture/encode/packetize経路が流れていることを示すが、receiverへの到達確認ではない。end-to-end delivery acknowledgement/RTCPは未実装
- 実機の遅延検証と UVC 入力確認は別途必要
- Raspberry Pi / Linux PC 向けの hardware codec 最適化は今後の調整余地がある
- systemdのRXをLinux desktopで使う場合、display sessionの環境や権限は環境依存。KMS/DRMを使うRaspberry Piとは条件が異なる


## CI

Pull Request と `main` への push では、GitHub Actions で以下を実行する。

- shell script の構文検証
- `cargo fmt --check` / `clippy -D warnings`
- workspace 全体の `cargo check`
- unit test（配布する全TOMLとproduction sink/pipelineのparse検証を含む）
- GStreamer を使った tx/rx loopback smoke test（映像・音声の実buffer通過、RTP ingressがあるのにdecode outputが無い異常、TX停止時のRX stall検出/再起動/復旧を確認）
- TX startup failureがHTTP監視を維持したまま再試行されること、RXが送信機offline中に無駄な再起動をしないことを確認
- Ubuntu 22.04 x86_64 / arm64 と Debian 12 arm64 でrelease package生成・checksum・manifest・local install検証

## License

MIT License。詳細は `LICENSE` を参照。
