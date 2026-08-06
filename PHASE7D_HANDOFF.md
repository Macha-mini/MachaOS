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

**flaky ハング**: クリーンビルドで sh が exit しない。診断で確認済みの事実:
- fork 子 (echo=8, cat=9) は正常に exit し、cat はファイルへの書込も完了
- sh の wait4(-1) は 8 → 9 → ECHILD の順に正しく戻る (wait4 自体は完成)
- **sh は waitforjob 後の ash コード (リング3) で停止し、以後 syscall を一切しない**
  (デバッグ出力付きの遅いビルドでは通る = タイミング依存の疑い)

次の手:
1. 停止中の sh のユーザー RIP を特定する (scheduler で pid==7 の user rip をダンプ、
   または gdbstub)。0x4567ba 付近 (evaltree) か、その後の waitforjob 後処理と推定
2. `waitpid(8)` / `waitpid(9)` の戻り値が ash の想定と合うか確認 (status の
   WIFEXITED エンコーディング `code << 8` は実装済み)
3. もしかすると ash は `waitpid` を EINTR 扱いする等のエッジがある — SIGCHLD 未実装
   (Phase 8) の影響の可能性。シグナル配送を実装すれば消えるかも

## 運用メモ

- `make test` は target/disk.img を作り直す (手動配置ファイルは消える)
- ホストは macOS: readelf なし、ELF 解析は /usr/bin/objdump + python struct
- 大きな patch はストリームタイムアウトする → 小分割必須
- busybox は ET_EXEC (4MiB ロード) のためカーネル埋め込み不可 → ディスク配置
  (`::/bin/busybox.elf`)、cat.elf は動的 glibc (`/lib/x86_64-linux-gnu/libc.so.6` 依存)
- パイプラインのファイル検証: `/users/macha/Documents/cat-test.txt` に
  `hello from pipeline\n` が書かれていることを fat::read_file で確認
