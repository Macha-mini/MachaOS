# MachaOS: Linux GUI アプリ動作基盤 ロードマップ

> **For Hermes:** この計画は長期的ロードマップ + Phase 6 完了の詳細タスク。Phase 6 はタスク単位で実行可能。その先(ティア1/GUI)はフェーズ単位のマイルストーン。

**Goal:** 任意の Linux GUI アプリ(無改造の実バイナリ)が MachaOS 上で動作する基盤を完成させる。

**Architecture:** カーネルは既に Linux x86_64 syscall ABI を持つ。残りは (a) 本物の glibc 動的リンクを完動させる (Phase 6)、(b) マルチプロセス基盤 (clone/TLS/シグナル/fork-exec = Phase 7-9)、(c) 本物の X11 または Wayland プロトコル実装 (GUI 基盤) の3段階。

**Tech Stack:** Rust no_std カーネル、Linux ABI (`src/linux_abi.rs`)、glibc 2.36 実バイナリ (ld.so / libc.so.6)、QEMU セルフテスト。

---

## 現状 (2026-08-05 時点)

### 完了済み (Phase 1-5)
- Linux x86_64 syscall ABI (`src/linux_abi.rs`)、`process::Abi::Linux`
- ELF ローダー: PT_LOAD / PT_INTERP (実 ld.so をエントリポイントとして起動)、PIE
- VFS + プロセスごと fd テーブル
- VMA: brk / 匿名・ファイル mmap / 本物の MAP_SHARED (`src/shm.rs`)
- AF_UNIX / SOCK_STREAM + SCM_RIGHTS fd 受け渡し
- 自作「Wayland 風」コンポジタ (`src/wayland.rs`、308行) — ただし**本物の Wayland プロトコルではない**

### Phase 6 (動的リンク) — 調査中・バグあり
実 Debian glibc (`ld-linux-x86-64.so.2` + `libc.so.6`) で動的リンク hello を起動 → `do_lookup_x` でフォールト:

```
[PROC] killed by page fault at 0x12f26 (error 0x5, rip=0x38008bc2, r9=0x12f20, ...)
```

**確定事実:**
- libc.so.6 の `.dynamic` が**未調整** (DT_SYMTAB=0x8a50 のまま)。hello / ld.so 自身は調整済み
- → `elf_get_dynamic_info` の ADJUST (`l_addr != 0 && !l_ld_readonly`) が **libc.so.6 のみスキップ**された
- libc.so.6 の `l_addr=0x40000000` は正しい。`l_ld_readonly` ビット = 0 (ADJUST されるべき条件なのにされていない)

**根因の有力仮説 (カーネルのファイル読み込み経路):**
- `l_phdr = 0x40004b48` (本来 `0x40000040` のはず = 異常)
- `l_phnum = 0` (本来 14 のはず = 異常)
- `pread64(fd=3, offset=0x40, count=0x310)` が2回呼ばれ、**1回目が全ゼロを返す**、2回目は正常な phdr
- **ELF ヘッダ読み (`offset=0`) が一度も発生しない**
- `openat` は `/lib/x86_64-linux-gnu/libc.so.6` を正しく開いている (fd 番号は未確認)

**推論:** ld.so が libc.so.6 の ELF ヘッダ/phdr を正しく読めていない (fd 衝突 or 最初の読みがゼロ) → phdr スキャンがゴミ → PT_DYNAMIC の p_flags 誤読 → `l_ld_readonly` 誤判定 → ADJUST スキップ → 未調整 symtab を do_lookup_x が走査 → フォールト。

### ⚠ 作業ツリーの状態 (重要)
- 現在ブランチ: `daily-driver`。**作業ツリーはクリーン**
- **Phase 6 のデバッグコードは `git stash@{1}` (`On NotWM: phase6-linux-abi-tracing-wip`) に退避されている** (src/linux_abi.rs +71行、src/process.rs +189行)
- デバッグコードの中身: `[MMAP]`/`[MPROTECT]`/`[PREAD64]`/`[OPENAT]` ログ (linux_abi.rs)、kill_current の #PF ダンプ拡張 (link_map 実測 / .dynamic ダンプ / bitfield / l_phdr ダンプ) (process.rs)
- **次の一手は「openat が返した fd 番号」の確認** — このログは追加済みのはずだが、stash 内に正しく含まれているか確認してから再実行すること

---

## ティア0: Phase 6 完了 (動的リンクの完成) — 最優先

**完了条件:** `make test` で「real dynamically-linked hello binary: Hello from GNU...」相当の出力 + SELFTEST OK。coreutils true / cat も通る。

### Task 0.1: デバッグコードを復元して調査を再開
- `git stash apply stash@{1}` (または pop) してデバッグコードを復元
- `make build` → `make iso GRUB_CFG=boot/grub/grub-selftest.cfg` → QEMU 実行 (定型コマンド) → `strings /tmp/machaos-selftest.log | grep -E "OPENAT|PREAD64"`
- **確認点:** `[OPENAT] path=/lib/x86_64-linux-gnu/libc.so.6 -> fd=?` の fd 番号。fd=3 が「何のファイル」か (ld.so.cache? libc.so.6? 既に閉じた hello の fd の再利用?)

### Task 0.2: pread64 ゼロ読みの原因特定
fd=3 が libc.so.6 を指しているのにゼロが返る場合、`sys_pread64` の実装 (`src/linux_abi.rs:498`) を精査:
- `h.seek(Start(offset))` → `h.read()` → `h.seek(Start(saved))` のカーソル保存/復元が正しいか
- VFS (`src/vfs.rs`) の `FileHandle::read` / FAT 層 (`src/fat.rs`) の読みが offset を正しく扱うか
- 1回目のゼロ読みは「同一 fd での2回目の open?」など、fd テーブル (`alloc_fd`) の衝突が原因の可能性も — openat の fd 番号ログで判別

### Task 0.3: 修正 + 検証
- 原因修正 → `make build` → `make test` で **SELFTEST OK** を確認 (これがゲート)
- 動的リンク hello が最後まで実行される (exit コード正常、`[INFO]` に実行成功ログ)

### Task 0.4: true / cat の selftest 追加
- `src/shell.rs` の selftest (1418-1482行付近) に coreutils-true (exit 0) / coreutils-cat (stdin エコー) の実行を追加
- `make test` = SELFTEST OK

### Task 0.5: デバッグコード除去 + コミット
- `[MMAP]`/`[MPROTECT]`/`[PREAD64]`/`[OPENAT]` ログと kill_current の拡張ダンプを除去 (or 最小限に)
- `git commit -m "feat: dynamically-linked binaries run under real glibc (hello/true/cat)"` (make test: SELFTEST OK を末尾に)

**見積もり: 数日〜1週間** (ゼロ読みが VFS の根本バグなら +1週間)

---

## ティア1: Phase 7-9 (マルチプロセス基盤) — 1〜2ヶ月

GUI アプリはマルチスレッド/シグナル/fork を使うため必須。各 Phase で「実バイナリが selftest を通る」を完了条件にする。

### Phase 7: Pthreads 基盤
- **clone** syscall (SYS_CLONE=56): スレッド生成。カーネルのタスク構造をスレッド対応に
- **TLS**: `arch_prctl(ARCH_SET_FS)` + スレッドローカルストレージ領域 (glibc が使う)
- **futex** (SYS_FUTEX=202): 最低限の WAKE/WAIT (mutex/condvar 用)
- 検証: 実バイナリの pthread テスト (例: `pthread_create` を呼ぶ小さい ELF) が動く

### Phase 8: シグナル
- `rt_sigaction` / `rt_sigprocmask` / `rt_sigreturn` (SYS 13/14/15)
- ページフォールト → SIGSEGV の変換 (kill_current をシグナル配送に置換)
- 検証: SIGSEGV を catch する実バイナリ

### Phase 9: fork / exec
- **fork** (SYS_FORK=57): COW ページ共有
- **execve** 完全実装 (現在のローダーを execve として再利用)
- 検証: `fork()` → `execve()` チェーンの実バイナリ (coreutils の sh 的な動作)

---

## ティア2: GUI 基盤 (本物のプロトコル) — 1〜3ヶ月

### 方針の決定 (要ユーザー判断)
- **A. 本物の X11 サーバ実装**: TCP 6000 で X11 Core プロトコル (基本描画: 矩形/テキスト/ピクセルマップ)。最小クライアントとして **xterm** を目標
- **B. 本物の Wayland 実装**: 既存の `src/wayland.rs` (自作プロトコル) を本物の wayland.xml プロトコルに置換。weston クライアントが動くことを目標
- **C. 静的リンクの X11 クライアントを自作**: カーネル側に最小 X サーバ (X11 プロトコルのサブセット) を実装し、自前コンパイルの静的クライアントで描画

**推奨: A (X11 Core)** — xterm は広く配布されており「任意の GUI アプリ」の代表例になりやすい。X11 Core は Wayland よりプロトコルが単純 (双方向バイトストリーム、イベント/リクエスト固定長)。

### マイルストーン
1. X11 サーバ: 接続受付 (TCP 6000)、Setup 応答、CreateWindow / MapWindow / イベント配送 (Expose / KeyPress)
2. 描画: PolyFillRectangle / PolyText8 / PutImage (フレームバッファへの合成)
3. 入力: PS/2 キーボード/マウス → X11 イベント変換
4. 最小クライアント動作: xterm 起動 → シェルが動く (これは Phase 9 の fork/exec + 疑似端末 (pty) が必要)
   - **注: pty 実装 (SYS_OPENPTY / TIOCSPTLCK 等) が xterm には必須** — ティア1 に含めるか、ティア2 冒頭で追加
5. 既存 WM (`src/wm.rs`) との統合: X11 ウィンドウをデスクトップのアプリウィンドウとして描画

### 検証
- GUI 目視検証は**ユーザー自身が `make run` で実施** (エージェントは make test のみ)
- 自動ゲート: X11 サーバのプロトコル selftest (クライアント接続 → 描画リクエスト → バッファ検証) をカーネル内 selftest に追加

---

## 最終目標 (ユーザー許容範囲)

- 「Firefox でなくとも**任意の Linux GUI アプリが動けば完了**」が確定済みの最終目標
- 本物の Firefox (無改造) は「種類の違う規模」— GTK3/4 スタック + 本物 Wayland 必須。ロードマップ上は**別プロジェクト扱い**とし、この計画の完了条件にしない

---

## 総合見積もり (正直なレンジ)

| 段階 | 内容 | 見積もり |
|---|---|---|
| ティア0 | Phase 6 完了 (動的リンク) | 数日〜2週間 |
| ティア1 | Phase 7-9 (Pthreads/シグナル/fork-exec + pty) | 1〜2ヶ月 |
| ティア2 | X11 サーバ + 最小クライアント (xterm) | 1〜3ヶ月 |
| **合計** | **任意の Linux GUI アプリ 1 個が動く基盤** | **3〜6ヶ月 (フルタイム換算)** |

- 「任意の GUI アプリ」を広く (GTK/Qt アプリ多数) 動かすには、さらにライブラリ移植が続く → 年単位の可能性
- 最大のリスク: pread64 ゼロ読みが VFS/FAT の根本バグの場合 (波及が広い)、X11 の描画モデルと既存 WM の統合

## リスク・開放質問

1. **pread64 ゼロ読みの根**: ファイル読みのカーソル管理 (`seek`/`read`/`pread` 相互) にバグがあれば、VFS 全体の信頼性に関わる。Phase 6 で徹底的に潰すこと
2. **fd 番号の衝突**: `alloc_fd` が最小空き番号を返す実装か、ld.so の fd が期待とずれていないか
3. **X11 or Wayland?** (ユーザー判断が必要。推奨は X11 Core)
4. **xterm の依存**: pty + termios + locale が xterm には必須。より軽いターゲット (例: 自作静的 X11 クライアント) を先に動かす方が確実
5. **GUI 検証**: 自動化は不可 (ユーザーが `make run` で目視)。X11 サーバのプロトコル部分のみ selftest 化

## 直近の次の一手 (この計画を実行する場合の最初のアクション)

1. `git stash apply stash@{1}` でデバッグコード復元
2. `make build` + `make iso GRUB_CFG=boot/grub/grub-selftest.cfg` + QEMU
3. `strings /tmp/machaos-selftest.log | grep OPENAT` で **fd 番号**を確認 → Task 0.2 へ
