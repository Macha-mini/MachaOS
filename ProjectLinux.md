# MachaOS: Linux ABI エミュレーション導入計画

## Context

MachaOSは元々、x86_64ベアメタルで動く自作カーネル(Rust, `no_std`)で、独自の最小ABI(5syscall)しか持たなかった。Phase 1〜5(下記)は**すべて実装・実機テスト・コミット済み**で、現在のMachaOSは:

- Linux x86_64 syscall ABI層(`src/linux_abi.rs`)を持ち、`process::Abi::Linux`でプロセスごとに切り替わる
- ELFローダーは`PT_LOAD`(任意vaddr、ET_EXEC/ET_DYN、セグメント別R/W/X)と`PT_INTERP`(本物の`ld.so`を実エントリポイントとして起動)に対応
- VFS層(`src/vfs.rs`)+ プロセスごとのfdテーブル(`FdEntry`: File/Shm/Socket)
- VMA管理(brk、匿名mmap、ファイルバックドmmap、そして**本物のMAP_SHARED**— `src/shm.rs`の共有メモリオブジェクトを2プロセスの物理フレームとして直接共有)
- AF_UNIX/SOCK_STREAMソケット(`src/socket.rs`)+ SCM_RIGHTSでのfd受け渡し
- カーネルネイティブで動く自作簡易「Wayland風」コンポジタ(`src/wayland.rs`)— 実クライアントプロセスがmemfd+mmap(MAP_SHARED)+socket+sendmsg(SCM_RIGHTS)で実フレームバッファに実際に描画することをselftestで実証済み

**新しい最終目標(ユーザー確定、交渉の余地なし)**: 本物の、無改造の公式Firefox Linuxビルド(`firefox-153.0.3.tar.xz`、実機確認済み — `libxul.so`だけで184MB、`libmozgtk.so`/`libmozwayland.so`に依存 = 本物のGTK3/4スタックと本物のWayland実プロトコル互換が必須)をGUI付きで動かすこと。

ただしこれは現状から見て「次の一手」の距離ではなく、**種類の違う規模**の話である(下記Phase 6以降と「正直な現実評価」セクション参照)。Phase 1〜5の「実バイナリを1個対象に足りないsyscallを数十個直す」という手法は、Phase 6以降でも通用する範囲(〜Phase 11)と、通用しなくなる範囲(Phase 12以降、Linuxデスクトップの主要ライブラリ群の移植)がある。この境界を隠さず明記した上で、段階的ロードマップとして計画する。

**直近の実装対象はPhase 6**(動的リンクの完成)。これが終わらない限りPhase 7以降は何も成立しない。

## 全体方針

- MachaOS独自ABI(`syscall.rs`の5syscall)は**変更しない**。Linux互換は別レイヤーとして追加し、プロセスがどちらのABIで起動されたかをフラグで持つ(既存の`user/src/bin/prog_*.rs`のようなネイティブテストプログラムを壊さない)。
- Linuxカーネル自体が動的リンカのリロケーション処理をしない(`ld-linux-x86-64.so.2`という「ただのLinuxバイナリ」をカーネルがロードしてジャンプするだけ)のと同じ発想を採用する。つまりMachaOS側は独自のELFリロケータを書く必要はなく、`mmap`/`mprotect`/`openat`などのsyscallを本物の`ld.so`が動く程度に正しく実装すれば、動的リンクは「基盤ができた上に自然に乗る」。この設計判断がPhase 1でのVMM刷新の理由。

## Phase 1 — 基盤刷新(全フェーズの前提)【完了】

現在のプロセスモデルはテスト用の小さいベアメタルプログラム専用に最適化されており、Linuxバイナリを載せる前に以下が必須:

1. **フレキシブルなVMA(仮想メモリ領域)管理**
   `src/process.rs`の`Process.mappings: Vec<Mapping>`を、単一の連続ブロック前提から「可変長・複数リージョンのVMAリスト」に拡張。`brk`によるヒープ伸長、匿名`mmap`、複数の`PT_LOAD`セグメントを個別に(結合せず)ロードできるようにする。
   併せて`process::kill_current`(#PFハンドラ)を、正当な遅延マッピング用フォルト(将来のデマンドページング)と真の不正アクセスを区別できるように拡張する必要がある(現状は無条件kill)。

2. **柔軟なELFロード**
   `src/elf.rs`/`src/process.rs::load_segments`を、固定`0x3000000`決め打ちではなく、ELFが指定する`p_vaddr`をそのまま使えるように書き換え。`ET_DYN`(PIE)のロードベースアドレス割り当てにも対応(乱数ASLRは後回しで固定ベースでよい)。セグメントごとに個別の保護属性(R/W/X)を維持する(現状は「どれか1つでもwritableなら全体writable」という粗い実装)。

3. **プロセスごとのファイルディスクリプタ表 + VFS抽象**
   `Process`に`fds: Vec<FileHandle>`を追加。`src/fat.rs`の上に薄いVFS層(新規`src/vfs.rs`)を作り、Linux互換層と既存シェル(`src/shell.rs`)の両方から使えるopen/read/write/close/lseek/statを提供する。

4. **Linux形式の初期スタックレイアウト**
   `argv`/`envp`/`auxv`(`AT_PHDR`/`AT_PHENT`/`AT_PHNUM`/`AT_ENTRY`/`AT_BASE`/`AT_EXECFN`/`AT_RANDOM`等)をユーザースタックに積む処理を追加(現状は戻り先アドレス1個だけ)。glibc/muslどちらの起動処理もこれを読むため必須。

5. **errno方式のsyscall戻り値規約**
   Linux syscallは失敗時に`-errno`を返す。既存の`SYSCALL_ERROR = u64::MAX`センチネル方式(ネイティブABI用)とは独立に、Linux ABI用のディスパッチ経路でこの規約を実装する。

## Phase 2 — Linux syscallテーブル: 静的musl MVP【完了】

新規`src/linux_abi.rs`(名称は仮)にLinux用の`syscall_dispatch`を実装。プロセスの「ABI種別」フラグで、ネイティブ5syscallとLinux syscallテーブルを切り替える(`syscall.rs`のSYSCALL_entryにディスパッチ分岐を追加、もしくは別のLSTAR切り替えでも可 — 実装時に検討)。

対象: **静的リンクmuslバイナリ**(例: busyboxの static musl build、または`musl-gcc -static`でビルドした簡単なCLIツール)。muslはglibcよりも起動時に要求するsyscallが少なく、ABI層の検証に向く。

最小実装すべきsyscall:
- `exit`/`exit_group`
- `read`/`write`/`openat`/`close`/`lseek`/`fstat`/`newfstatat`(Phase 1のVFS層を利用)
- `brk`、匿名`mmap`/`munmap`/`mprotect`(Phase 1のVMA管理を利用)
- `arch_prctl`(`ARCH_SET_FS` — TLSベース。muslも使う)
- `set_tid_address`、`set_robust_list`(no-opで可)
- `rt_sigaction`/`rt_sigprocmask`(状態保持のみ、実配送はしない)
- `getrandom`(スタックプロテクタのcanary用)
- `uname`、`ioctl`(isatty判定用に最小スタブ)、`writev`、`getpid`、`clock_gettime`

検証: `user/src/bin/prog_*.rs`と同じ要領で、実際のmuslツールチェインでビルドした小さいバイナリをFAT32イメージに置き、シェルまたはファイルエクスプローラから起動 → 標準出力と終了コードを確認。新しい起動コマンド(例: `runlinux <path>`)か、ELFヘッダから自動判別してPhase 2経路に流すかは実装時に決める。

## Phase 3 — 静的glibcバイナリ【完了】

muslより要求syscallが多い: `futex`(シングルコア前提での簡易スタブでまず可)、`rseq`(EINVALで拒否してもglibc側がフォールバックするケースが多い)、`prlimit64`、`sched_getaffinity`、`sysinfo`。glibcは`errno`自体をTLS経由で扱うため、`arch_prctl`まわりの正確性がここで特に重要になる。

## Phase 4 — 動的リンク・実ディストリバイナリ【完了(既知の未解決バグあり)】

- ELFの`PT_INTERP`を検出し、バイナリ本体ではなく実物の`/lib64/ld-linux-x86-64.so.2`を実際のエントリポイントとして起動する仕組みは実装・コミット済み。
- **既知の未解決ギャップ**: 実際のGNU Hello(実`ld.so`+実`libc.so.6`、本物のDebianパッケージから抽出)は最後まで動かない。`ld.so`は`libc.so.6`を開き、`pread64`でプログラムヘッダを正しく読み(バイト単位で検証済み)、`fstat`も正しいサイズを返す(実ファイルサイズと一致確認済み)ところまでは到達するが、そのままシンボルバージョン処理に入ってフォールトする — `libc.so.6`のセグメントを実際に`mmap`する呼び出しが一度も発生しないまま。`src/linux_abi.rs`のモジュールdocに調査履歴あり(AT_PHDR/pread64/AT_EMPTY_PATH/スタックバッファ境界/access(21)は全て実バイナリテストで発見・修正済みだが、この最後のブロッカーは未解決)。**Phase 6の最優先事項。**

## Phase 5 — GUIアプリ最小実証【完了】

`AF_UNIX`ソケット(`socket`/`connect`/`sendmsg`/`recvmsg`、`SCM_RIGHTS`でのfd受け渡し)、本物の共有メモリ(`memfd_create`+`mmap(MAP_SHARED)`)、カーネルネイティブの自作簡易「Wayland風」コンポジタ(`src/wayland.rs`)を実装。実クライアントプロセスが実syscallだけで実フレームバッファに描画することをselftestで実証。ただし**本物のWaylandワイヤープロトコルとは非互換**(独自簡略プロトコル、object table無し、xdg-shell無し、入力転送無し)であることを明記済み — Phase 11で本物のプロトコル互換に置き換える。

---

# Firefox実行に向けた拡張ロードマップ(Phase 6〜)

以下はPhase 5完了後、ユーザーが「Firefoxを本気で動かす」ことを最終目標に確定したことを受けて追加した段階的計画。**各フェーズは実バイナリでの検証を伴う独立した意味のある成果**として設計してある — 後述の「正直な現実評価」の通り、Firefox自体への到達を保証するものではないが、途中で止まったとしても各フェーズ単体で「トイOSの域を超えた実績」になるよう意図的に設計している。

## Phase 6 — 動的リンクの完成(最優先・全ての前提)【進行中 — 大きなバグを2件修正・実セグメントmmap達成・次のバグを精密特定済み】

1. **Phase 4の残バグ調査【完了・修正済み】**: GNU Helloが`libc.so.6`のセグメントを`mmap`する前にフォールトしていた原因を特定・修正した。`llvm-objdump`でフォールトRIP(`0x38013c24`)周辺を逆アセンブルし、`frame.r8`(フォールト時の実レジスタ値、`process::kill_current`に恒久的なログとして追加済み)が`0x53053053`(glibcの`RSEQ_SIG`定数)という明らかにおかしい値になっていることを確認。**根本原因**: `syscall_entry`(`src/syscall.rs`)が`syscall_dispatch`呼び出し前後で呼び出し元の`rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`を保存・復元していなかった。本物のLinuxのsyscall ABIでは`rax`(戻り値)と`rcx`/`r11`(`syscall`/`sysretq`命令自体が破壊する)以外は不変であることが保証されており、実際のglibc(`ld.so`のTLS/rseqセットアップ)は`__rseq_offset`をsyscall跨ぎで`r8`に生かしたまま再利用するコードがあり、これに依存していた。修正しコミット済み(`b4b6234`)。フォールトは完全に解消。
2. **st_dev/st_ino常時ゼロバグ【完了・修正済み】**: `fstat`/`newfstatat`が`struct stat`全体をゼロクリアした後`st_mode`/`st_size`等しか埋めておらず、`st_dev`/`st_ino`が全ファイルで常にゼロだった。本物の`ld.so`はロード予定の共有ライブラリを「既にロード済みのマップ」と`(st_dev, st_ino)`で重複排除しており、全ファイルが同じ(ゼロの)ペアを返すため`libc.so.6`がメイン実行ファイル(hello自身)と同一ファイルだと誤認され、`ld.so`は`libc.so.6`の実セグメントを一度もmmapせず、hello自身の`link_map`を無言で再利用していた(これが前回発見した「`hello: hello: no version information available (required by hello)`」の正体 — メッセージ中のライブラリ名は本当にhello自身だった)。修正: 各ファイルに安定的だが用途に足る一意な合成`st_ino`(正規化パスのFNV-1aハッシュ、`vfs::Stat::ino`)と固定の非ゼロ`st_dev`を付与。コミット`8856152`。
3. **修正後の到達点と新たな残バグ【未解決・次の調査対象】**: st_dev/st_ino修正により、`ld.so`は初めて`libc.so.6`の実4セグメントを正しいアドレス・権限で本当に`mmap`するようになった(実際のプログラムヘッダと突き合わせ済み)。その後hello最初の未定義シンボル`__libc_start_main`に対する実`GNU_HASH`ベースのシンボルテーブル探索まで到達 — このプロジェクトで実glibcバイナリが到達した最深部。ただしこの探索の**内部**で新しいフォールトが発生する。`llvm-objdump`で正確な命令まで特定し、glibc本家ソース(`elf/dl-lookup.c`の`do_lookup_x`/`check_match`、`glibc-2.36`タグ)と突き合わせた結果、フォールトするポインタは`libc.so.6`の実`.dynsym`ベースアドレス(`0x8a50`、実ファイルで確認済み)+ もっともらしいシンボルインデックス×24バイトと厳密に一致する ―― **つまり`D_PTR(map, l_info[DT_SYMTAB])`計算(`map->l_addr + リンク時アドレス`)で`map->l_addr`(`libc.so.6`の実ロードバイアス`0x40000000`、mmap時点では正しい値だったことを確認済み)が「0」として扱われている**ように見える。同じmapの`GNU_HASH`バケット/チェイン歩行(こちらは正しくバイアス済みのポインタが必要)は正常に機能しているにも関わらず。`ld.so`自身のデータ/BSS領域を`0x40000000`という値でスキャンしたが、どこにも見つからなかった(「間違った構造体から読んでいる」より「この特定のアクセスに対する代入が一度も実行されていない」説を支持)。3つの独立した実バイナリ(hello, true, cat)で同一の命令・同一の未バイアス値・同一のシンボルで再現し、フィクスチャ固有の問題ではないことを確認済み。本物の(無シンボルテーブル・ストリップ済みの)glibcマシンコードが相手のため、インタラクティブデバッグ(QEMUの`-s`/gdbstub + `lldb`)を実際に試みた: `lldb`からの接続自体は成功したが、(a) ハードウェアブレークポイント(`breakpoint set --address ... --hardware`)は該当命令に到達しても一度もヒットしなかった(QEMU TCGのx86デバッグレジスタ対応の制約の可能性)、(b) ソフトウェアブレークポイントは対象プロセスのページテーブルがアクティブな正確なタイミングで設置する必要があり、非対話的なバッチ実行(1回のBashツール呼び出しごとに新規lldbセッションが必要で、真の対話的セッションを維持できない)ではこのタイミング合わせが実用的でなかった。このアプローチは一旦保留 — 次回、より良いツール(MachaOSカーネル自身へのGDBスタブ組み込み、またはQEMUモニタとの連携による正確なタイミング制御)があれば再挑戦する価値がある。
4. **NXビット対応【完了】**: `EFER.NXE`を有効化(`syscall::init`)し、`paging::PAGE_NX`をスタック/ヒープ/mmap/`mprotect`のPROT_EXEC、および`process::load_segments`が各PT_LOADセグメントの実`PF_X`から導出する形で反映した(コミット`1071103`)。回帰スイート・実バイナリ再テストとも問題なし。
5. **vDSO/AT_SYSINFO_EHDR確認【完了】**: `auxv`テーブルに`AT_SYSINFO_EHDR`(=33)は完全に未搭載であることをコード確認。glibcの標準的なグレースフルフォールバック条件(vDSO無しなら実syscall経由にフォールバック)と一致し、実際musl版busyboxは完全動作、glibc版hello/true/catもTLS/rseq/tunables処理を通過して(vDSO初期化で問題が起きるならここより前に出るはず)既知の別バグまで到達していることから実証的にも問題なしと確認。コード変更不要。
6. GNU Helloに続き、実Debianパッケージから抽出した実coreutils級バイナリ(`true`、`cat`)を動かす試み【完了・上記の各バグ調査で継続的に活用、フィクスチャ固有の問題ではないことの確認に貢献】。

**検証**:
- 主目標: GNU Helloが最後まで実行完了する(未達、ただし大きく前進 — 2つのクラッシュ解消、実セグメントmmap達成、実シンボルテーブル探索到達)。
- 中間チェックポイント: 実際の静的リンクmusl/busyboxバイナリが引き続き動くことを確認済み(`make test`回帰スイート全項目パス)。

## Phase 7 — マルチプロセス・マルチスレッド プロセスモデル

`clone(CLONE_THREAD|CLONE_VM|CLONE_FS|CLONE_FILES)`と`fork`/`execve`/`waitpid`は、どちらも現在の「1 Task = 1 Processが`frames`/`fds`を単独所有」モデル(`process.rs`)から「アドレス空間・fdテーブルを複数Taskで共有・refcount管理」モデルへの同じリファクタが必要 — 別フェーズに分けると同じ設計変更を二度行うことになるため統合する。

1. `Process`の内部を共有可能な形にリファクタ(frames/fdsをrefcounted/共有構造へ)。
2. `clone`: `CLONE_VM`でアドレス空間共有、`CLONE_FILES`でfdテーブル共有、スレッドグループ管理。
3. 本物の`futex` `FUTEX_WAIT`/`FUTEX_WAKE`: 現状は「非ブロッキングsyscallの制約」により常に即座に返すスタブ(`syscall.rs`のSFMASKがsyscall中IF=0にする制約は変わらない)。カーネル内wait-queueを持ち、`futex_wait`したタスクを「ブロック中」としてスケジューラから除外し、`futex_wake`が該当タスクを再度runnableにする、という真のブロッキング機構が必要 — syscallハンドラ自体はノンブロッキングのまま、スケジューラ側の状態遷移で実現する。
4. `fork`/`execve`/`waitpid`/`wait4`: 現状の`process::spawn`は「新規ELFをロードするだけ」で本物のfork+execではない。子プロセスへの状態コピー(またはCoW的な扱い)、`execve`によるアドレス空間の総入れ替え、`waitpid`による親の待ち合わせを実装する。

**検証**:
- 実static pthreadsバイナリ: スレッド生成+join、mutexで保護したカウンタへの競合アクセスが正しい結果になること。
- 実`dash`または`busybox sh`でのパイプライン実行(`ls | grep`など)— fork+exec+wait+pipe2が揃って初めて動く、本物の第三者バイナリでの統合テスト。

## Phase 8 — シグナル配送

`rt_sigaction`の実ハンドラ登録・配送、`rt_sigreturn`、`sigaltstack`。最低限 `SIGSEGV`(現状の`kill_current`無条件killを置き換え)/`SIGBUS`/`SIGCHLD`/`SIGTERM`/`SIGKILL`。

**検証**: SIGSEGVハンドラを登録して回復する実バイナリ、子プロセス終了でSIGCHLDを受け取る親プロセス。

## Phase 9a — イベントループ系syscall

`epoll_create`/`epoll_ctl`/`epoll_wait`、`eventfd`、`timerfd`、`poll`/`ppoll`、`pipe2`、`dup`/`dup2`/`dup3` — glibやモダンなイベントループ実装が前提とする一式。Phase 7のプロセスモデルとは独立に進められる。

**検証**: これらを組み合わせた実バイナリ(後述Phase 12aのGLibメインループテストで本格検証、ここでは個別syscallの単体動作を確認)。

## Phase 9b — 疑似ファイルシステム(/proc, /dev)

最小`/proc`(`self`, `cpuinfo`, `meminfo`)と`/dev`(`null`, `zero`, `urandom`, `shm`)を`vfs.rs`にレイヤーとして追加。`statx`、実`fcntl`サブセット、`unlink`/`rename`/`mkdir`をLinux syscallとして(現状シェル専用のFAT32ラッパーとは別に)実装。

**検証**: `/proc/cpuinfo`/`/proc/meminfo`を読む実バイナリ、`/dev/urandom`からの読み取り。

## Phase 10 — ネットワークスタック

`AF_INET`/`AF_INET6`ソケット、最小TCP/IP実装(またはQEMUのuser-modeネットワーキングと組み合わせ)、DNS解決。TLS自体はFirefoxがNSSを同梱しているためソケット/トランスポート層のみで良い。

**検証**: 静的リンクの実`curl`または`wget`で1回のHTTP GETが成功すること(Phase 6/7の動的リンクリスクと切り離して検証するため、あえて静的リンクバイナリを選ぶ)。

## Phase 11 — 本物のWaylandワイヤープロトコル互換

Phase 5の自作簡略プロトコル(`src/wayland.rs`)を、本物のWaylandワイヤーフォーマットに置き換え・拡張する: `wl_registry.bind`のgeneric new_idエンコーディング、本物のper-connectionオブジェクトテーブル(現状はハードコードされたステップ列)、`xdg-shell`(`xdg_wm_base`/`xdg_surface`/`xdg_toplevel` — 本物のウィンドウ管理に必須)、`wl_seat`+`wl_keyboard`+`wl_pointer`(既存の`keyboard.rs`/`mouse.rs`からの実入力転送)。

**検証**: 本物の、無改造のWaylandクライアント(例: weston付属の`weston-simple-shm`、または実`libwayland-client`を使う最小プログラム)が接続・描画できること — これがプロトコル互換性そのものの証明であり、Firefoxとは独立した達成目標。

## Phase 12 — GTK/ツールキット依存スタック(最大・最高リスクのフェーズ)

**現時点での確認事項**: Firefoxに非GTK・非Waylandの代替バックエンドは存在しない(`dependentlibs.list`で`libmozgtk.so`/`libmozwayland.so`がリンク時依存として確定)。「軽量モード」のような回避策はなく、本物のGTK3/4スタックの移植が必要 — Phase 6+7+8を合計したより大きい、計画全体で最大のフェーズになる見込み。リスクを分散するため独立して検証可能な3段階に分割する:

- **12a: glib/gobject/gio + イベントループ統合**(Phase 9aのepoll等の上に構築)。検証: 実際のGLibメインループを使う最小テストバイナリがGUI無しで動くこと。
- **12b: freetype + fontconfig + cairo + pango**。検証: 実Pangoテストプログラムでテキストをバッファ/PNGにレンダリングできること(ウィンドウ不要)。
- **12c: gdkのWaylandバックエンド**(Phase 11の本物プロトコル + 12aの上に構築)。検証: 実GTKの「hello window」バイナリが表示され、入力に反応すること。

これら(fontconfig, cairo, pango, glib/gobject/gio, GTKのWaylandバックエンド)は移植であって新規実装ではない — 各ライブラリは何年もかけて開発された上流プロジェクトで、実`mmap`/`dlopen`/`pthread`/シグナル/POSIXタイマー/locale・iconvなど、Phase 6〜11で作ってきたものより遥かに深いところでLinuxのPOSIX実装に依存している。「足りないsyscallを1つずつ直す」という手法がここから先は効きにくくなる、計画全体の分水嶺。

## Phase 13 — Firefox起動前提条件の充足

1. **まず環境変数での回避を試す**(実装コストが低い方から): `MOZ_DISABLE_SANDBOX=1`(seccomp-bpf/namespaceセットアップ自体をスキップする既知の正規の回避策)、`MOZ_FORCE_DISABLE_E10S=1`(マルチプロセスを無効化、Phase 7の`fork`要件を緩和できる可能性)。これでプロセスが起動するなら、本物の`seccomp()`/`unshare()`/`prctl(PR_SET_NO_NEW_PRIVS)`実装への投資は後回しにできる。
2. **GPU/レンダリング経路の実地確認(go/no-go判定)**: Firefoxのソフトウェアフォールバック(Software WebRender、Skia経由)自体はDRI/Mesa/GL/Vulkanを一切必要としない(朗報)。ただし起動時のグラフィックス機能検出が`/dev/dri/*`不在やEGL/GLXプローブ失敗を「ハードアボート」として扱うビルド/設定がある。`MOZ_LOG=Layers:5`/`MOZ_LOG=WebRender:5`で実際に何を試して何が起きるかを本物の153.0.3ビルドで確認し、グレースフルにフォールバックしない場合はここで対処法を検討する。
3. オーディオスタック(ALSA/PulseAudioプローブ)のno-opスタブ、D-Bus関連の起動時プローブで判明した必須項目への対応 — 「実際に起動を試して次に何を要求されるか見る」という、これまで全フェーズで通用してきた手法をここでも継続する。

## Phase 14 — 実Firefox起動試行(反復)

Phase 13までの土台の上で、実`firefox-bin`を実際に起動し、「次に何のsyscall/ライブラリを要求するか見て直す」というPhase 1〜6と同じ手法を反復する。段階的な成功基準:

(a) プロセスが即座に落ちずに起動する
(b) 本物のWaylandウィンドウが実際に表示される(Phase 11必須)
(c) UIクローム(ツールバー等)が実際にレンダリングされる(Phase 12必須)
(d) 実際のWebページがネットワーク経由で表示される(Phase 10必須)

**Phase 13完了後もPhase 14の完全成功(=Firefoxが実用的に動く)は保証されない** — これは次セクションで正直に評価する。

---

## 正直な現実評価(ティア分けした成功の物差し)

無改造の本物のFirefoxをGUI付きで完全に動かすことは、**個人の反復的なOS開発努力として現実的な到達点ではない**、と明言しておく。理由は「もっと時間をかければ届く」という量の問題ではなく、**質の違う壁がPhase 12にある**ため: Phase 1〜6の手法(カーネルのsyscall境界を相手に、足りないものを1つずつ直す)は数百個のsyscallという有限の表面積が相手だからこそ機能する。Phase 12以降で相手にするのは、何年もかけて開発された数十万行のCコード(glib/gobject/gio, cairo, pango, harfbuzz, fontconfig, freetype, GTK3/4のWaylandバックエンド)の移植であり、「外から観察してデバッグする」ことしかできず、「自分で書いていないコードの奥深い移植性の前提」に依存する — これは規模ではなく種類の違う難しさである。

そのため、単一の合否ラインではなく**達成ティア**で成功を測る:

- **ティア1(高確率で到達可能)**: Phase 6〜9完了 — 本物の無改造glibc動的リンクバイナリ、本物のPOSIXスレッド、本物のシグナル配送、本物のfork/execが動く。これだけでもホビーOSとしては稀な到達点。
- **ティア2(継続的な努力で十分見込みあり)**: Phase 10〜11完了 — 本物のTCP/IPスタック、本物のWaylandワイヤープロトコル互換(`weston-simple-shm`等の無改造クライアントで証明可能)。
- **ティア3(壁、ここで停滞する可能性が最も高い)**: Phase 12完了 — GTK/cairo/pango等の移植。ここで計画が止まるとしたら、それは「エンジニアリング時間が足りない」からではなく「これらのライブラリの移植性の前提がsyscall単位のデバッグでは追いきれない深さにある」からである可能性が高い。実GTKの「hello window」が表示された時点で、それ自体を独立した立派な成果として扱ってよい。
- **ティア4(Firefox本体)**: 「約束ではなく、挑戦する」という位置づけを明記する。Phase 6〜12はFirefoxが最終的に動くかどうかに関わらずそれ自体で価値がある、という前提で計画されている。

## 検証方法(全フェーズ共通)

- 各フェーズごとに、実物の(musl/glibc/実ディストリパッケージから抽出した)第三者バイナリで検証する — 自作の合成テストではなく、Phase 1〜5で確立した「実バイナリでテストして初めて見つかる本物のバグを直す」手法を継続する。
- 未実装syscallの呼び出しは`[UNKSYSCALL]`ログ(`linux_abi.rs`に実装済み、恒久的な診断機能)に記録し、実バイナリが実際に何を要求してくるかの蓄積を優先順位付けに使う。
- 各フェーズの完了時にドキュメント(モジュールdoc)へ既知のギャップを正直に明記する — Phase 1〜5で確立したパターンを継続する。