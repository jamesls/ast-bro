pub fn before() void {}

pub fn stale_asm() void {
    asm volatile ("" ::: .{ .memory = true });
}

pub fn after() void {}
