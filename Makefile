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

.PHONY: all build gen user iso disk wallpaper run run-nographic test clean

all: build

gen:
	python3 tools/gen_isr.py

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

disk: wallpaper
	@test -n "$$(command -v mkfs.fat)" || (echo "dosfstools (mkfs.fat) is required: brew install dosfstools"; exit 1)
	dd if=/dev/zero of=$(DISK) bs=1m count=64 2>/dev/null
	mkfs.fat -F 32 $(DISK)
	printf 'hello from the host\n' > target/fixture.txt
	printf 'welcome to MachaOS\n' > target/fixture2.txt
	mmd -i $(DISK) ::/bin
	mcopy -i $(DISK) target/user-exit.elf ::/bin/prog_exit.elf
	mcopy -i $(DISK) target/user-fault.elf ::/bin/prog_fault.elf
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

run: iso disk
	$(QEMU) -cdrom $(ISO) -boot d -drive file=$(DISK),format=raw -serial stdio

run-nographic: iso disk
	$(QEMU) -cdrom $(ISO) -boot d -display none -serial stdio

test: GRUB_CFG=boot/grub/grub-selftest.cfg
test: iso disk
	@rm -f $(TEST_LOG)
	@echo "== running MachaOS selftest in QEMU =="
	@$(QEMU) -cdrom $(ISO) -boot d -display none -serial file:$(TEST_LOG) -hda $(DISK) \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 &
	@for i in $$(seq 1 60); do \
		sleep 1; \
		if grep -q "SELFTEST OK" $(TEST_LOG) 2>/dev/null; then \
			echo "== PASS: selftest completed =="; \
			cat $(TEST_LOG); \
			exit 0; \
		fi; \
		if ! pgrep -q qemu-system-x86_64; then \
			echo "== FAIL: QEMU exited before the selftest finished =="; \
			cat $(TEST_LOG); \
			exit 1; \
		fi; \
	done; \
	echo "== FAIL: selftest timed out =="; \
	cat $(TEST_LOG); \
	exit 1

clean:
	cargo clean
