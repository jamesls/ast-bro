const std = @import("std");
const helper = @import("support/helper.zig");

pub fn leaf() usize {
    return 1;
}

pub fn calls_leaf() usize {
    return leaf();
}

pub const Widget = struct {
    const Self = @This();

    value: usize,

    pub fn ping(self: Widget) usize {
        return self.value;
    }

    pub fn static_value() usize {
        return 1;
    }

    pub fn receiver_calls(self: Widget) usize {
        return self.ping() + Self.static_value();
    }
};

pub fn build_and_ping() usize {
    const widget = Widget{ .value = 41 };
    return widget.ping();
}

pub fn imported_call() usize {
    return helper.work();
}

pub fn print() void {}

pub fn field_receiver() void {
    std.debug.print("hello\n", .{});
}

test "call stays with test owner" {
    _ = leaf();
}
