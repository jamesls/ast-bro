const a = @import("./a.zig");

pub fn from_b() usize {
    return a.from_a();
}
