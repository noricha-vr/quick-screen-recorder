# Quick Screen Recorder

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: macOS 15+](https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey.svg)](https://www.apple.com/macos/)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

[English README](README.md)

Apple の **ScreenCaptureKit**・**AVFoundation / VideoToolbox**・**AppKit** を純 Rust から直接駆動する、macOS 向けの高速・軽量ネイティブ画面録画アプリです。

Electron（Chromium + Node.js）ベースの画面録画ツールと異なり、常駐メモリは数十 MB 台、起動と範囲選択は瞬時、録画中は Apple Silicon のハードウェアエンコーダ（`SCRecordingOutput`）へゼロコピー直結し、GIF / APNG 書き出しも外部依存なしのマルチコア並列処理で高速に完了します。

## スクリーンショット

| 初回セットアップ＆チュートリアル | 範囲・ウィンドウ・画面選択オーバーレイ | Trim & Export エディタ |
|---|---|---|
| ![Quick Start Onboarding](docs/images/onboarding.png) | ![Selection Overlay](docs/images/overlay.png) | ![Trim and Export Editor](docs/images/editor.png) |

## 特徴

- **初回起動の権限オンボーディング＆アニメーションチュートリアル** — 初回起動時に画面収録権限のライブステータスを表示し、ワンクリックで「システム設定 > プライバシーとセキュリティ > 画面収録」を開けます。許可・再起動するとステータスがグリーンに変わり、右上メニューバー `◉ Rec` からの試し録画〜書き出し保存までアニメーション付きで 1 回ガイドします（メニューバー **`◉ Rec` → `Quick Start Guide...`** からいつでも再表示可能）。
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
  - **専用保存フォルダ・最適自動命名＆任意ファイル名編集・プログレスバー**: デフォルト保存先は **`~/Movies/Quick Screen Recorder`**（自動作成）で、エディタの **`Save to`** ドロップダウンまたはメニューバー **`◉ Rec` → `Save Folder`** からいつでも任意のフォルダ（`~/Movies`・`~/Desktop`・`~/Downloads`・`~/Documents` や **`Choose Folder...`** での新規フォルダ作成）へ変更・記憶できます。自動ファイル名は時系列順で並び検索しやすい **`YYYY-MM-DD_HH-MM-SS_Quick-Screen-Recorder`**（ウィンドウ録画時は対象アプリ名も付与）を初期表示し、エディタの **`Name`** 欄で任意の名前にそのまま編集してプログレスバー付きで即保存（または **`Copy`** でクリップボードコピー）できます。
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

初回起動時に **Quick Start** ウィンドウが自動で開き、画面収録権限の確認・システム設定のワンクリック起動・許可後のグリーン確認から、右上メニューバーでの試し録画〜書き出し保存まで順番に案内します。

## 使い方

1. 起動後、初回は **Quick Start** ウィンドウの案内に沿って画面収録権限を有効にします（いつでもメニューバー **`◉ Rec` → `Quick Start Guide...`** から開き直せます）。
2. メニューバーの **`◉ Rec` → `New Recording...`** をクリックするか、グローバルショートカット **`Cmd + Shift + 6`** を押します（`Esc` でキャンセル）。
3. **Area / Window / Screen**、比率、解像度、FPS（`30` / `60`）、音声、カーソル・クリック強調を選び、**`● Record`** を押して録画を開始します。
4. 録画中はメニューバーに経過秒数（`● 00:08`）が表示されます。メニューの **`Stop Recording`** または **`Cmd + Shift + 6`** で停止します。
5. 自動で開く **Trim & Export** ウィンドウで **`IN` / `OUT`** 範囲、フォーマット、最大幅、**`Save to`** 保存先フォルダ（既定 `~/Movies/Quick Screen Recorder`、メニューバー **`◉ Rec` → `Save Folder`** からも変更可）、必要に応じて **`Name`** ファイル名欄（自動生成 `YYYY-MM-DD_HH-MM-SS_Quick-Screen-Recorder` 等）を調整し、**`Copy`** または **`Save`** を押します。

## ライセンス

[MIT License](LICENSE)
