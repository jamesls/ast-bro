const std = @import("std");

/// A public widget.
pub const Widget = struct {
    /// Number of completed operations.
    count: usize = 0,
    enabled: bool = true,
    const Self = @This();

    /// Builds a widget.
    pub inline fn init(name: []const u8) !Widget {
        _ = name;
        return Widget{ .count = 0, .enabled = true };
    }

    fn reset(self: *Self) void {
        self.count = 0;
    }

    test "widget local" {
        _ = try init("local");
    }
};

/// Operating modes.
pub const Mode = enum {
    idle,
    running = 2,
};

pub const Value = union(enum) {
    number: i64,
    text: []const u8,
};

pub const Handle = opaque {
    const marker = 0;
};

pub const Failure = error{
    BadInput,
    Timeout,
};

const Hidden = struct {
    marker: u8,
};

threadlocal var active: bool = false;

/// Starts the application.
pub export fn launch() void {}

// This ordinary comment is not documentation.
pub fn undocumented() void {}

/// Detached documentation.

pub fn after_gap() void {}

test "launch works" {
    launch();
    std.debug.print("ok", .{});
}
