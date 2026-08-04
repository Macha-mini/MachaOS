use crate::port;

pub const PIC1_COMMAND: u16 = 0x20;
pub const PIC1_DATA: u16 = 0x21;
pub const PIC2_COMMAND: u16 = 0xA0;
pub const PIC2_DATA: u16 = 0xA1;

pub fn remap() {
    unsafe {
        port::outb(PIC1_COMMAND, 0x11); // ICW1: init
        port::outb(PIC2_COMMAND, 0x11);
        port::outb(PIC1_DATA, 0x20); // ICW2: IRQ0-7 -> vectors 0x20-0x27
        port::outb(PIC2_DATA, 0x28); // IRQ8-15 -> vectors 0x28-0x2F
        port::outb(PIC1_DATA, 0x04); // ICW3: slave on IRQ2
        port::outb(PIC2_DATA, 0x02);
        port::outb(PIC1_DATA, 0x01); // ICW4: 8086 mode
        port::outb(PIC2_DATA, 0x01);
        port::outb(PIC1_DATA, 0xF8); // unmask IRQ0 (timer), IRQ1 (keyboard), IRQ2 (cascade)
        port::outb(PIC2_DATA, 0xFF); // mask all slave IRQs; individual IRQs unmask() themselves
    }
}

pub fn eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            port::outb(PIC2_COMMAND, 0x20);
        }
        port::outb(PIC1_COMMAND, 0x20);
    }
}

/// Unmasks a single IRQ line (0-15) on the appropriate (possibly cascaded) PIC.
pub fn unmask(irq: u8) {
    unsafe {
        if irq < 8 {
            let mask = port::inb(PIC1_DATA) & !(1 << irq);
            port::outb(PIC1_DATA, mask);
        } else {
            let mask = port::inb(PIC2_DATA) & !(1 << (irq - 8));
            port::outb(PIC2_DATA, mask);
        }
    }
}
