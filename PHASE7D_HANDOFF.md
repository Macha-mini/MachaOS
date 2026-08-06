# Phase 7d 引き継ぎ書 — busybox sh パイプライン統合検証

日付: 2026-08-06
ブランチ: daily-driver (Phase 7b コミット 4c22dae の上に未コミット変更)

## 目的

Phase 7 (プロセスモデル: clone/futex/fork/execve/wait4) の最終検証として、
**本物の busybox sh にパイプラインを実行させる**:
`busybox sh -c "echo hello from pipeline | /bin/cat.elf > /users/macha/Documents/cat-test.txt"`
— fork + pipe + dup2 + execve (実動的 glibc バイナリ) + wait4 が全部同時に動く必要がある。

## 達成状況

- **パイプラインはデバッグ出力付きビルドで 1 回 [SELFTEST OK] を達成した**
  (fork 子2つ生成 → echo が pipe へ書込 → cat (動的 ld.so+libc ロード) が読んでファイルへ書込 → 子2つ exit → sh の wait4 が reap → sh exit → ファイル内容検証 OK)
- **クリーンビルドでは flaky**: sh が `waitforjob` 後の ash コードで止まり SELFTEST FAIL になる。
  詳細は「残課題」参照。

## 今回の変更 (未コミット)

### 追加 syscall (musl/glibc 静的・動的バイナリが使う旧番号)
- `SYS_OPEN(2)` — musl は openat でなく open を使う
- `SYS_STAT(4)` / `SYS_LSTAT(6)` — newfstatat のエイリアス
- `SYS_FCNTL(72)` — F_DUPFD/F_GETFD/F_SETFD/F_GETFL/F_SETFL (CLOEXEC 追跡付き)
- `SYS_FCNTL64(221)` — glibc の fcntl64 (同じコマンド群)
- `SYS_DUP2_64(63)` — **x86_64 の dup2 は 33 でなく 63** (33 は i386 番号)
- `SYS_GETPPID(110)` — 0 返し (親追跡は Process.parent で実装済みだが getppid は未配線)

### バグ修正 (いずれも実機で確認した根本原因)
1. **exit チェーン破壊** (`task.rs::repark_exit_chain` 新設)
   - 旧: enter_usermode が自フレーム深さ (top-0x7c0) に exit チェーンを置く → fork の深い
     syscall フレーム (fork_copy は ~1KiB) が上書き → exit の ret がユーザースタックの
     ゴミへ飛びクラッシュ (sh の "page fault at 0xfffffde, rsp=0x502c7c8" シグネチャ)
   - 新: プロセスタスクは `[kernel_stack_top - RING3_PARK_RESERVE - 4096]` に
     6 ゼロスロット + exit_self を再配置 (RESERVE は 1024→4096 に拡大)
   - **注意**: run_demo (非プロセスの selftest リング3 スモーク) は repark を
     **スキップ**する (`current_is_process()` チェック)。スキップしないとデモの exit が
     exit_self に直行し main タスクが死ぬ (SELFTEST OK が出なくなる)
2. **fork 子の callee-saved が1スロットずれる** (`task.rs::spawn_child`)
   - context_switch は `pop r15 r14 r13 r12 rbp rbx` の順で pop するのに、
     フレームは `[rbx rbp r12 r13 r14 r15]` で書いていた → 子のレジスタが全部ずれ、
     子の r13 が 0 になって ash の evaltree が NULL 参照 (0x21 クラッシュの正体)
   - 修正: `callee_saved.iter().rev()` で書込む
3. **SAVED_USER_RSP グローバルの奪い合い** (`syscall.rs` + `task.rs`)
   - ブロッキング syscall (wait4/pipe read/futex) 中に別タスクの syscall が
     グローバル SAVED_USER_RSP を上書き → 復帰時 rsp が他タスクの値になりクラッシュ
   - 修正: per-task `saved_user_rsp` + `snapshot_saved_user_rsp()` を両 dispatch
     (native + linux) の先頭で呼び、asm の sysretq は `syscall_load_user_rsp()` で
     自タスクの値を読む (rax/r11/rcx はスクラッチ static に退避して helper call を跨ぐ)
4. **execve 後も CPU の CR3 が旧空間のまま** (`process.rs::execve_into_current`)
   - タスクの cr3 フィールドだけ更新しており、exec-restart の iretq まで CPU は旧
     (解放済み) 空間で実行 → 古いスタックのゴミ argv を読んで無限ループ
   - 修正: `paging::write_cr3(new_cr3)` を syscall 中に即実行 (identity map は全空間に
     deep copy 済みなので安全)
5. **write/read が fd テーブルを無視して stdio 直行** (`linux_abi.rs::sys_write/read`)
   - `echo hi > file` のリダイレクトが効かなかった: fd 1/2 を常にシリアルへ出していた
   - 修正: fd テーブルのエントリ優先、無ければ fd 1/2 = シリアル、fd 0 = キーボード
6. **wait4(-1) が「自分の子」でなく任意の exit 済みプロセスを返す** (`sys_wait4`)
   - 過去テストの未 reap ゾンビ (fork テストの子) を返し続け ash の待ちループが無限回転
   - 修正: `Process.parent` (fork 時に設定、execve でも維持) で自分の子に限定
7. **wait4 が子の exit 状態を消費しない** (ゾンビを reap しない)
   - 修正: 返した子の exit_info をクリア (`task::reap_exit_status`)
8. **wait4 が ECHILD を返さない**
   - 全子が reap 済みゾンビなら `-ECHILD` を返す (ash の waitpid(-1) ループの終端条件)

### 検証
- `src/shell.rs` の selftest part 2h: busybox パイプライン (ファイル内容検証付き)
- `src/syscall.rs`: exec-restart の r11/rax/rcx スクラッチ static (SAVED_RFLAGS_SCRATCH
  / SAVED_RETVAL_SCRATCH / SAVED_RIP_SCRATCH — 全て no_mangle 必須)
- `src/syscall.rs`: native dispatch にも snapshot_saved_user_rsp() を追加
  (native プロセスが rsp=0 でクラッシュするのを防ぐ)

## 残課題 (次のセッションで)

**flaky ハング** (調査継続中 — 2026-08-06 追記): クリーンビルドで sh が exit しない
(selftest の `wait(7, 800)` が `None` でタイムアウト → `busybox-sh process gave
unexpected exit: None`)。確認済みの事実:
- fork 子 (echo=8, cat=9) は正常に exit し、cat はファイルへの書込も完了
- sh の wait4(-1) は 8 → 9 → ECHILD の順に正しく戻る (wait4 自体は完成)
- **シリアル診断出力がタイミングを変えて flaky を隠す**: 診断付きビルド
  ([SC7] syscall トレース / [SH] timer ダンプ / [WAITPOLL7]) は 2/2 PASS、
  クリーンビルドは 3/3 FAIL を観測。診断のシリアル書込が syscall dispatch を
  遅くし interleaving を変えている。**シリアル診断ではこのバグを捕まえられない**
- **[SH] トレース分析で sh の「リング3 停止」は否定**: sh は tick 320 で
  カーネルモード (cs=0x8、wait4 ポーリングループ内) で current になっており、
  以後 t%20 境界に現れないのは正常 (QUANTUM=5、タスク ~13 → ターン間隔 ~65 tick
  が t%20 とまれにしか重ならないだけ)。スケジューラ・wait4・spawn (sh は
  parent=None で生成されるため他タスクの wait4(-1) に reap されない) は全て正常
- クリーン失敗時のログ末尾は sh 起動時の未実装 syscall
  ([UNKSYSCALL] 102/104/106/105/79 = getuid/getgid/setgid/setuid/getcwd → ENOSYS)。
  その後の syscall (rt_sigaction 13 / pipe 22 / dup2 63 等) は実装済みでプリント
  されないため、停止箇所はログからは特定不能

**2026-08-06 追記 2 — exit チェーン破壊バグを発見・修正 (Phase 8a 実装中)**:
`Process` 構造体に sigactions 配列 (`[SigAction; 65]` = 1040B) をインライン追加したところ、
fork テストが決定的に ring-0 #UD (rip=0x3) で落ちるようになった。原因は fork パス
(fork_copy が新 Process をカーネルスタック上に構築) のスタック使用が syscall 領域
(top-4096) と parked exit チェーン (top-8192) の 4KB ヘッドルームを超え、チェーンの
ret スロット (exit_self) を破壊 → exit 時に ret がゴミへ飛ぶ、というもの。
- 修正: sigactions を `Box<[SigAction; 65]>` に変更 (構造体 -1032B) +
  `repark_exit_chain` の間隔を 4096 → 8192 に拡大 (防御)
- 教訓: **深い syscall パス (fork/execve) のスタック使用量を増やす構造体変更は
  exit チェーン破壊を招く** — 構造体は小さく保つか、ヘッドルームを先に広げる

**Phase 8a 進捗**: kill(62)/tgkill(234)/rt_sigaction(13)/rt_sigprocmask(14) 実装 +
SIG_DFL デフォルト動作の配送 (終了/無視) + wait4 のシグナル終了ステータス符号化。
selftest の busybox `kill -CHLD/KILL/-0 $$` 3 チェックは一時的順序入れ替えで
[OK] 確認済み。ハンドラ配送 (sigframe + rt_sigreturn + SIGCHLD) は Phase 8b。

次の手:
1. **Phase 8 (シグナル配送) が最有力**: `rt_sigaction`/`rt_sigprocmask` は現在
   スタブ (常に 0 返し)。ash は起動時に SIGCHLD/SIGINT 等のハンドラを登録しており、
   シグナル未配送の影響で ash の待機パスがまれに完了しない可能性がある。
   SIGCHLD 配送 + rt_sigreturn + sigaltstack を実装して再検証する
2. それでも残るなら: gdbstub / QEMU monitor で停止中の sh のユーザー RIP を特定
   (シリアル診断はタイミングを変えるため使えない — 診断なしで停止を捕まえる
   手段が必要)

## 運用メモ

- `make test` は target/disk.img を作り直す (手動配置ファイルは消える)
- ホストは macOS: readelf なし、ELF 解析は /usr/bin/objdump + python struct
- 大きな patch はストリームタイムアウトする → 小分割必須
- busybox は ET_EXEC (4MiB ロード) のためカーネル埋め込み不可 → ディスク配置
  (`::/bin/busybox.elf`)、cat.elf は動的 glibc (`/lib/x86_64-linux-gnu/libc.so.6` 依存)
- パイプラインのファイル検証: `/users/macha/Documents/cat-test.txt` に
  `hello from pipeline\n` が書かれていることを fat::read_file で確認
