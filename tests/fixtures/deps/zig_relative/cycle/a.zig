const b = @import("./b.zig");

pub fn from_a() usize {
    return b.from_b();
}
