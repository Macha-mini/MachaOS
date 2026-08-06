TOOLCHAIN_BIN := $(shell ls -d $(HOME)/.rustup/toolchains/nightly*/lib/rustlib/*apple-darwin/bin 2>/dev/null | head -1)
export PATH := $(TOOLCHAIN_BIN):$(HOME)/.cargo/bin:$(PATH)

QEMU := qemu-system-x86_64
KERNEL := target/x86_64-unknown-none/release/machaos
GRUB_MKRESCUE := $(shell command -v i686-elf-grub-mkrescue 2>/dev/null || command -v grub-mkrescue 2>/dev/null)
GRUB_CFG ?= boot/grub/grub.cfg
ISO_DIR := target/machaos-iso
ISO := target/machaos.iso
DISK := target/disk.img
WALLPAPER_SRC := himawari.png
WALLPAPER := target/wallpaper.raw
WALLPAPER_W := 1920
WALLPAPER_H := 1080
TEST_LOG := /tmp/machaos-selftest.log
# A real static-musl x86_64 Linux binary (BusyBox), fetched on demand
# rather than checked in, for testing the Linux ABI layer (linux_abi.rs)
# against something that wasn't hand-built for MachaOS. Not needed for
# `build`/`test` (those cover the native ABI and the Phase 1/2 test
# programs) — only for `runlinux`-style manual runs against a real binary.
BUSYBOX := target/busybox
BUSYBOX_URL := https://www.busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox

.PHONY: all build gen user iso disk disk-linux wallpaper run run-nographic run-linux test clean busybox

all: build

gen:
	python3 tools/gen_isr.py
	python3 tools/gen_jp_font.py

# Builds the freestanding user-program ELFs (user/) and copies them to
# target/ where the kernel embeds them at compile time.
user:
	cd user && cargo build --release --bins
	cp user/target/x86_64-unknown-none/release/prog_exit target/user-exit.elf
	cp user/target/x86_64-unknown-none/release/prog_fault target/user-fault.elf
	cp user/target/x86_64-unknown-none/release/prog_syscall target/user-syscall.elf
	cp user/target/x86_64-unknown-none/release/prog_ipc target/user-ipc.elf
	cp user/target/x86_64-unknown-none/release/prog_stack target/user-stack.elf
	cp user/target/x86_64-unknown-none/release/prog_linux_stack target/user-linux-stack.elf
	cp user/target/x86_64-unknown-none/release/prog_linux_syscall target/user-linux-syscall.elf
	cp user/target/x86_64-unknown-none/release/prog_linux_phase3 target/user-linux-phase3.elf
	cp user/target/x86_64-unknown-none/release/prog_linux_threads target/user-linux-threads.elf
	cp user/target/x86_64-unknown-none/release/prog_linux_fork target/user-linux-fork.elf
	cp user/target/x86_64-unknown-none/release/prog_linux_wayland_client target/user-linux-wayland-client.elf

build: gen user
	cargo build --release

iso: build
	@test -n "$(GRUB_MKRESCUE)" || (echo "x86_64-elf-grub-mkrescue or grub-mkrescue is required"; exit 1)
	rm -rf $(ISO_DIR)
	mkdir -p $(ISO_DIR)/boot/grub
	cp $(KERNEL) $(ISO_DIR)/boot/machaos
	cp $(GRUB_CFG) $(ISO_DIR)/boot/grub/grub.cfg
	$(GRUB_MKRESCUE) -o $(ISO) $(ISO_DIR)

# Converts the source wallpaper image into the raw 0x00RRGGBB pixel dump
# wm.rs loads from disk (see tools/gen_wallpaper.py). Only regenerated
# when the source image or the converter script changes.
$(WALLPAPER): $(WALLPAPER_SRC) tools/gen_wallpaper.py
	python3 tools/gen_wallpaper.py $(WALLPAPER_SRC) $(WALLPAPER) $(WALLPAPER_W) $(WALLPAPER_H)

wallpaper: $(WALLPAPER)

# Phase 6 test fixtures: real dynamically-linked Linux binaries (GNU
# hello, coreutils true/cat, glibc 2.36 ld.so + libc.so.6, Debian 12
# amd64). Idempotent — reuses whatever's already in target/.
fixtures:
	@./tools/fetch-linux-fixtures.sh

disk: wallpaper fixtures $(BUSYBOX)
	@test -n "$$(command -v mkfs.fat)" || (echo "dosfstools (mkfs.fat) is required: brew install dosfstools"; exit 1)
	dd if=/dev/zero of=$(DISK) bs=1m count=256 2>/dev/null
	mkfs.fat -F 32 -s 8 $(DISK)
	printf 'hello from the host\n' > target/fixture.txt
	printf 'welcome to MachaOS\n' > target/fixture2.txt
	mmd -i $(DISK) ::/bin
	mcopy -i $(DISK) target/user-exit.elf ::/bin/prog_exit.elf
	mcopy -i $(DISK) target/user-fault.elf ::/bin/prog_fault.elf
	# Phase 6: real dynamically-linked Linux binaries (Debian 12 amd64)
	# exercised by the selftest — GNU hello, coreutils true/cat, glibc
	# 2.36's ld.so + libc.so.6. See tools/fetch-linux-fixtures.sh.
	mcopy -i $(DISK) target/hello ::/bin/hello.elf
	mcopy -i $(DISK) target/coreutils-true ::/bin/true.elf
	mcopy -i $(DISK) target/coreutils-cat ::/bin/cat.elf
	# Phase 7: real BusyBox (static musl) for the sh-pipeline selftest.
	# Not embedded in the kernel: busybox's ET_EXEC segments load at
	# 4 MiB, which would collide with the kernel image's own data.
	mcopy -i $(DISK) $(BUSYBOX) ::/bin/busybox.elf
	mmd -i $(DISK) ::/lib64
	mmd -i $(DISK) ::/lib
	mmd -i $(DISK) ::/lib/x86_64-linux-gnu
	mcopy -i $(DISK) target/ld-linux-x86-64.so.2 ::/lib64/ld-linux-x86-64.so.2
	mcopy -i $(DISK) target/libc.so.6 ::/lib/x86_64-linux-gnu/libc.so.6
	mmd -i $(DISK) ::/system
	mmd -i $(DISK) ::/users
	mmd -i $(DISK) ::/users/macha
	mmd -i $(DISK) ::/users/macha/Desktop
	mmd -i $(DISK) ::/users/macha/Documents
	mmd -i $(DISK) ::/users/macha/Downloads
	mmd -i $(DISK) ::/users/macha/Pictures
	mmd -i $(DISK) ::/users/macha/Music
	printf 'MachaOS system files\n' > target/system-readme.txt
	printf 'Welcome to MachaOS Desktop\n' > target/desktop-welcome.txt
	mcopy -i $(DISK) target/system-readme.txt ::/system/README.TXT
	mcopy -i $(DISK) target/desktop-welcome.txt ::/users/macha/Desktop/welcome.txt
	mcopy -i $(DISK) target/fixture.txt "::/users/macha/Documents/hello world.txt"
	mcopy -i $(DISK) target/fixture2.txt ::/users/macha/Documents/greetings.txt
	mcopy -i $(DISK) target/fixture.txt ::/users/macha/Documents/readme.txt
	mcopy -i $(DISK) $(WALLPAPER) ::/system/wallpaper.raw
	rm -f target/fixture.txt target/fixture2.txt
	rm -f target/system-readme.txt target/desktop-welcome.txt

$(BUSYBOX):
	curl -sL $(BUSYBOX_URL) -o $(BUSYBOX)
	chmod +x $(BUSYBOX)

busybox: $(BUSYBOX)

# Copies BusyBox onto an already-built disk image, for a manual
# `runlinux /bin/busybox.elf ...` smoke test of the Linux ABI layer
# against a real binary. Kept separate from `disk` so the automated
# `make test` selftest suite never needs network access.
disk-linux: disk $(BUSYBOX)
	mcopy -i $(DISK) $(BUSYBOX) ::/bin/busybox.elf

# QEMU user-mode networking: the e1000 NIC the kernel driver expects,
# NATed through the host (guest 10.0.2.15, gateway 10.0.2.2, DNS 10.0.2.3).
NET_ARGS := -netdev user,id=n0 -device e1000,netdev=n0

# Storage: the disk attaches to an ICH9 AHCI (SATA) controller instead
# of the legacy PIIX IDE bus, exercising the kernel's AHCI driver.
AHCI_ARGS := -device ich9-ahci,id=ahci -drive id=disk0,if=none,file=$(DISK),format=raw \
	-device ide-hd,drive=disk0,bus=ahci.0

run: iso disk
	$(QEMU) -m 4G -cdrom $(ISO) -boot d $(AHCI_ARGS) $(NET_ARGS) -serial stdio

run-nographic: iso disk
	$(QEMU) -m 4G -cdrom $(ISO) -boot d $(AHCI_ARGS) $(NET_ARGS) -display none -serial stdio

test: GRUB_CFG=boot/grub/grub-selftest.cfg
test: iso disk
	@rm -f $(TEST_LOG) $(TEST_LOG).pid
	@echo "== running MachaOS selftest in QEMU =="
	@printf 'MachaOS http fixture 1234567890\n' > target/http-fixture.txt
	@python3 tools/echo_server.py target >/dev/null 2>&1 & echo $$! > $(TEST_LOG).pid
	@$(QEMU) -m 4G -cdrom $(ISO) -boot d -display none -serial file:$(TEST_LOG) \
		$(AHCI_ARGS) $(NET_ARGS) -device isa-debug-exit,iobase=0xf4,iosize=0x04 &
	@for i in $$(seq 1 60); do \
		sleep 1; \
		if grep -q "SELFTEST OK" $(TEST_LOG) 2>/dev/null; then \
			echo "== PASS: selftest completed =="; \
			cat $(TEST_LOG); \
			kill $$(cat $(TEST_LOG).pid) 2>/dev/null || true; \
			rm -f $(TEST_LOG).pid; \
			exit 0; \
		fi; \
		if ! pgrep -q qemu-system-x86_64; then \
			echo "== FAIL: QEMU exited before the selftest finished =="; \
			cat $(TEST_LOG); \
			kill $$(cat $(TEST_LOG).pid) 2>/dev/null || true; \
			rm -f $(TEST_LOG).pid; \
			exit 1; \
		fi; \
	done; \
	echo "== FAIL: selftest timed out =="; \
	cat $(TEST_LOG); \
	kill $$(cat $(TEST_LOG).pid) 2>/dev/null || true; \
	rm -f $(TEST_LOG).pid; \
	exit 1

clean:
	cargo clean
