const std = @import("std");
const helper = @import("./sub/b.zig");

pub fn run() usize {
    std.debug.print("running", .{});
    return helper.answer();
}
