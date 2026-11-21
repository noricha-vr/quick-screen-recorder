# Quick Screen Recorder

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: macOS 15+](https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey.svg)](https://www.apple.com/macos/)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

[English README](README.md)

Apple の **ScreenCaptureKit**・**AVFoundation / VideoToolbox**・**AppKit** を純 Rust から直接駆動する、macOS 向けの高速・軽量ネイティブ画面録画アプリです。

Electron（Chromium + Node.js）ベースの画面録画ツールと異なり、常駐メモリは数十 MB 台、起動と範囲選択は瞬時、録画中は Apple Silicon のハードウェアエンコーダ（`SCRecordingOutput`）へゼロコピー直結し、GIF / APNG 書き出しも外部依存なしのマルチコア並列処理で高速に完了します。

## スクリーンショット

| 範囲・ウィンドウ・画面選択オーバーレイ | Trim & Export エディタ |
|---|---|
| ![Selection Overlay](docs/images/overlay.png) | ![Trim and Export Editor](docs/images/editor.png) |

## 特徴

- **Electron / WebView 不使用の純ネイティブ設計** — AppKit のメニューバー常駐とボーダレス透明オーバーレイにより、軽快に動作します。
- **ハードウェア直結録画 (`SCRecordingOutput`)** — 範囲・単一ウィンドウ・画面全体を H.264 / HEVC でリアルタイムにハードウェアエンコードし、停止と同時に MP4 が完成します。
- **直感的なキャプチャ選択オーバーレイ**
  - **Area（範囲選択）**: ドラッグ作成、選択範囲内のドラッグ移動、8 方向リサイズ、アスペクト比ロック（`Free` / `16:9` / `16:10` / `4:3` / `3:2` / `1:1` / `9:16`）、`W × H` ピクセル数値入力に対応。
  - **Window（ウィンドウ単体）**: 画面上の任意のウィンドウをクリックするだけで録画対象を切り替え可能（手前に重なった別ウィンドウは映り込みません）。
  - **Screen（画面全体）**: マルチディスプレイ環境で任意のディスプレイを選択可能。
  - **カーソル表示**・**クリック強調表示**・**システム音声 / マイク録音**をフローティングバーからワンクリックで切り替え。
- **録画後すぐ使える Trim & Export エディタ**
  - **`IN` / `OUT` ブラケット型タイムライン**: 開始地点・終了地点のブラケットと再生ヘッドを直感的にドラッグし、リアルタイムでフレームをプレビュー。
  - **外部依存ゼロのネイティブ書き出し**: **MP4 (H.264)**、**MP4 (HEVC)**、**GIF**（並列 NeuQuant パレット量子化＋フレーム差分最適化）、**APNG**。
  - **FFmpeg 自動連携（任意）**: システムに `ffmpeg` がある場合は **WebM (VP9)** と **MP4 (AV1)** も自動で選択可能になります。
  - **保存先フォルダ選択＆即座保存・プログレスバー**: デフォルトは `~/Desktop` ですが、エディタの **`Save to`** ドロップダウンまたはメニューバー **`◉ Rec` → `Save Folder`** からいつでも任意のフォルダ（`~/Movies`・`~/Downloads`・`~/Documents` や **`Choose Folder...`** での新規フォルダ作成・指定）へ変更・記憶できます。**`Save`** を押した瞬間にダイアログなしでプログレスバー付きで書き出され、**`Copy`** でクリップボードへの即コピーにも対応します。
- **プライバシー重視のローカル完結設計**
  - ネットワーク通信・外部テレメトリ送信は一切ありません。
  - 書き出し時に動画コンテナのメタデータを自動除去します。
  - ローカルの速度統計（`stats` サブコマンド）にもファイルパス・ウィンドウ名・個人情報は一切記録しません。

## 動作要件

- **macOS 15.0 (Sequoia) 以降**
- ソースからビルドする場合: **Rust 1.85 以降**
- 任意: WebM / AV1 書き出しを使う場合のみ `ffmpeg`（`brew install ffmpeg`）

## ビルドと起動

```bash
git clone https://github.com/noricha-vr/quick-screen-recorder.git
cd quick-screen-recorder

# Release ビルドと dist/QuickScreenRecorder.app の作成
make bundle

# 起動（メニューバーに「◉ Rec」が表示されます）
open dist/QuickScreenRecorder.app

# アプリケーションフォルダへ配置する場合
cp -R dist/QuickScreenRecorder.app /Applications/
```

初回録画時に macOS の「システム設定 > プライバシーとセキュリティ > 画面収録とシステムオーディオ録音」で許可を有効にしてください。

## 使い方

1. 起動後、メニューバーの **`◉ Rec` → `New Recording...`** をクリックするか、グローバルショートカット **`Cmd + Shift + 6`** を押します（`Esc` でキャンセル）。
2. **Area / Window / Screen**、比率、解像度、FPS（`30` / `60`）、音声、カーソル・クリック強調を選び、**`● Record`** を押して録画を開始します。
3. 録画中はメニューバーに経過秒数（`● 00:08`）が表示されます。メニューの **`Stop Recording`** または **`Cmd + Shift + 6`** で停止します。
4. 自動で開く **Trim & Export** ウィンドウで **`IN` / `OUT`** 範囲、フォーマット、最大幅、**`Save to`** 保存先フォルダ（既定 `~/Desktop`、メニューバー **`◉ Rec` → `Save Folder`** からも変更可）を選び、**`Copy`**（クリップボードへコピー）または **`Save`**（指定フォルダへ即保存）を押します。

## ライセンス

[MIT License](LICENSE)
