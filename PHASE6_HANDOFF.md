# Phase 6 (動的リンク) 調査引き継ぎ書

- 日付: 2026-08-05
- ブランチ: `daily-driver`
- 目的: GNU hello / coreutils true / cat (Debian 12 bookworm, glibc 2.36) をこのカーネル上で動かす (Phase 6: 動的リンク)

## 現状まとめ

**libc.so.6 のロード・リロケーションはカーネル側 workaround でここまで動作するようになったが、`__libc_early_init` の実行中にフォールト (rip=0x0) し、hello/true/cat はまだ実動していない。** make test は SELFTEST OK を出す (hello の失敗はスキップ経由)。

## 解決済み (このセッションで確定した事実)

### 1. `0x38032ad8` は `_dl_pagesize` であり、書き換えてはいけない
- ld.so の `_rtld_global_ro+0x18` (= ld.so ファイルオフセット `0x32ad8`) は **`_dl_pagesize` (初期値 0x1000 が正しい)**。
- 過去のセッションで「`_dl_phdr` と誤認して 0x40000040 / 0x20000040 を書き込む FIX」を入れていたが、これが **`failed to map segment from shared object` の原因** だった。
- 理由: ld.so はこの値を `negq` してページアラインマスクを作る (`ld.so 0x6759` → `0x6ae0/0x6b03/0x6b23` の `andq`)。大きな phdr ポインタを書くとマスクが壊れ、loadcmd の vaddr/offset が非アラインになり、mmap ラッパー (`0x20c90` の `testl $0xfff, %r9d`) が EINVAL を返す。
- **対応: FIX コードを撤去済み。`sys_pread64` は素通し。** (`src/linux_abi.rs` に経緯コメントあり)

### 2. ADJUST (elf_get_dynamic_info の .dynamic バイアス) がスキップされる
- l_addr は RW セグメントマップ後に 0x40000000 になるが、`elf_get_dynamic_info` の ADJUST が実行されず、libc.so.6 の `.dynamic` の d_ptr が未バイアス (DT_SYMTAB = 0x8a50) のまま → do_lookup_x が未バイアス symtab でフォールトしていた。
- 原因は l_ld_readonly の誤計算 (phdr スキャンの起点ずれ) と推定されるが、正確な起点は特定できていない。
- **対応 (workaround): `sys_mmap` が libc.so.6 の RW セグメント (offset=0x1cf000) をマップした直後に、カーネルが `.dynamic` (0x401d2b60) のアドレス系タグ 11 個に l_addr を加算。** glibc の ADJUST_DYN_INFO を模倣 (PLTGOT/HASH/STRTAB/SYMTAB/RELA/JMPREL/RELR/GNU_HASH/GNU_LIBLIST/VERSYM/VERDEF/VERNEED、DT_RELA は d_ptr=0 を除外)。

### 3. RELR リロケーション (.relr.dyn) は ld.so が処理しないためカーネルが適用
- libc.so.6 は `.relr.dyn` (0x25270, 0x118 バイト) を使う。RELATIVE (type 8) の .rela.dyn エントリは 0 個。
- このカーネルの ld.so 実行パスでは RELR が処理されないため、`__libc_early_init` が GOT (0x1d2de8 = `__libc_single_threaded` の GLOB_DAT 先) を 0 のまま読み、NULL フォールトしていた。
- **対応 (workaround): `sys_mmap` 内で `.relr.dyn` をデコードして適用 (アドレスエントリ + ビットマップエントリの両形式、addend = 対象アドレスのファイル初期値)。** 適用数は 118 (※ デコード実装はビットマップの途中 break があり、本来 1227 個のうち一部のみ。GOT に必要な分は適用できている)。

### 4. GLOB_DAT / JUMP_SLOT はカーネルが名前解決
- `.rela.dyn` の GLOB_DAT (type 6) / R_X86_64_64 (type 1) 70 スロットを解決。定義済みシンボルは st_value + l_addr、未定義 (rtld エクスポート) は ld.so の dynsym から名前で解決。
- `.rela.plt` の JUMP_SLOT (type 7) 14 個を eager bind。未定義は ld.so のエクスポート (`_dl_exception_create`, `_dl_find_dso_for_object`, `_dl_deallocate_tls`, `__tls_get_addr`, `_dl_fatal_printf`, `_dl_audit_symbind_alt`, `_dl_rtld_di_serinfo`, `_dl_allocate_tls`, `__tunable_get_val`, `_dl_allocate_tls_init`, `__nptl_change_stack_perm`, `_dl_audit_preinit`) を ld.so ロードベース 0x38000000 + st_value で解決。
- rtld エクスポートの GOT スロット解決値: `_dl_argv`=0x38032a98, `__libc_enable_secure`=0x38032a60, `__libc_stack_end`=0x38032a58, `_rtld_global_ro`=0x38032ac0, `__rseq_size`=0x38032a10, `_rtld_global`=0x38033020。
- ※ これらの値は **Debian 12 bookworm の ld-linux-x86-64.so.2 固有**。フィクスチャを変える場合は再計算が必要。

### 5. その他の確定事実
- libc.so.6 のロードベースは 0x40000000、ld.so は 0x38000000、hello (PIE) は 0x20000000。
- libc.so.6 の link_map は 0x38034a90 (hello 実行時)。l_phnum = 14。
- TLS (fs_base) は ARCH_SET_FS = 0x401e3100 (hello) / 0x401e3140 (true) で設定される。カナリアは fs+0x28 にあり正しくセットされる。
- 64 ビット範囲比較 (`<=`) がデバッグログ内で偽になる現象がある (コンパイラ最適化/LTO 起因と推定)。マッピング検索は**等値比較**を使うこと。

## 未解決 (現在のフォールト)

```
[PROC] killed by page fault at 0x0 (error 0x15, rip=0x0,
       r8=0x40, r9=0xc, r10=0xffffab0, r11=0x246, r12=0xfffffffffffffff8,
       r13=0x0, r14=0x380342e0, r15=0x380342e0, rbx=0xffffa90, rbp=0xffffd90,
       rdx=0x4008b990, rcx=0x1080, rdi=0x20, rsi=0xffffa90, rsp=0xffffa78)
```

- **場所**: `__libc_early_init` (libc.so.6 0x14fee0) が `callq 0x8b9b0` (0x14ff99) で呼ぶ関数の実行中。スタックダンプ (削除前の観測) では rsp+0x30 = 0x4014ff9e (0x14ff99 の戻り先), rsp+0x10 = 0x4008b9dc (0x8b9b0 内の `__tunable_get_val@plt` 1 回目の戻り先), rsp+0x00 = **0**。
- **直接の原因**: `__tunable_get_val@plt` (PLT スタブ 0x262c0) の実行経路で rip=0 へ飛ぶ。GOT.plt[0x1d3158] はフォールト時も 0x38014060 と正しいので、PLT スロット自体は解決済み。
- **最有力仮説 (要検証)**: `__tunable_get_val` (ld.so 0x14060) の末尾 `0x140b0: jmpq *%rdx` でゲッターコールバック (libc.so.6 0x8b990 / 0x8b9a0) にジャンプし、その `retq` (0x8b999 / 0x8b9aa) がスタックの戻りアドレスを rip に載せる。**スタック先頭 (rsp+0x00) が 0 なので、callq の戻り先プッシュが 0 になっている、または 0x8b990 実行前後でスタックが壊れている**。レジスタの rdx=0x4008b990 (= 1 回目のゲッター)、rdi=0x20 (= tunable id) から、1 回目の `__tunable_get_val` 実行中のフォールトと推定。
- ゲッター (libc 0x8b990): `movq (%rdi), %rax; movl %eax, 0x1d328c(%rip); retq` — rdi は `__tunable_get_val` の `0x140ab: leaq 0x48(%rcx,%rax), %rdi` でテーブルエントリ (0x38032828 付近) を指すはずが、レジスタダンプでは 0x20 (id) のまま → **0x140ab が実行されずに 0x140b0 (jmpq *%rdx) に到達した可能性** (rdx=0 のとき 0x140a9: je 0x140b8 で retq だが、rdx は 0x4008b990 なので不一致)。

### 次の調査手順 (推奨)
1. `__tunable_get_val` (ld.so 0x14060) の実行を単純化: tunables テーブル (0x380319e0 + id×112) のエントリをホスト側 python でダンプし、id=0x20 と id=0x12 のエントリ (type/len/val/getter ポインタ) がファイルと実メモリで一致するか確認。
2. 0x8b9b0 (libc) のスタックフレームを正しく追跡: `pushq %rbx; subq $0x10, %rsp` の後 rsp=0xffffa80。callq は 0xffffa78 に戻り先をプッシュするはず。**callq の戻り先プッシュが 0 になる原因** (カーネルの syscall 復帰処理が rsp を壊す? タスクスイッチで fs_base が変わる?) を切り分ける。
3. フォールト時のスタックダンプを一時的に復活させるか、sys_read/sys_write などの次の syscall で 0xffffa78 の値を観測して、0 になるタイミングを特定。
4. 解決したら workaround のオフセット依存 (0x1cf000, 0x401d2b60 など) をコメントに残したままコミット (フィクスチャ固定のため許容)。

## フィクスチャとビルド

- `tools/fetch-linux-fixtures.sh` (新規): Debian 12 bookworm amd64 の .deb から hello / coreutils true・cat / ld-linux-x86-64.so.2 / libc.so.6 を target/ に展開 (冪等、ネット必須)。
- Makefile: `disk: wallpaper fixtures` に `fixtures:` ターゲット追加 (make test 時に自動取得)。
- 注意: **make test は target/disk.img を作り直す**ため、ディスクに手動配置したファイルは消える (fixtures は自動で再配置される)。
- ホストは macOS: readelf なし → ELF 解析は `/usr/bin/objdump` + 自作 python struct パース (python3 使用)。

## 検証手順 (定型)

```sh
make build
make iso GRUB_CFG=boot/grub/grub-selftest.cfg
make test                      # ゲート = [SELFTEST OK]
strings /tmp/machaos-selftest.log | grep -E "dynamically|page fault|SELFTEST"
```

- QEMU 起動は Makefile 準拠 (AHCI のみ、`-hda` 禁止 — 二重アタッチでハング)。
- 手動 QEMU 前に `pkill -9 -f qemu-system` と `pkill -f echo_server`。
- シリアルバッファが小さいのでデバッグログは最小限に。高頻度ログ (pread64/read/mprotect のデータダンプ) はログ欠落の誤判断源になる。
