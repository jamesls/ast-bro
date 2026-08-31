const std = @import("std");

pub extern var shared_state: u32 align(16) addrspace(.generic) linksection(".shared");
pub const Callback = *const fn (*anyopaque) callconv(.c) void;

pub const PackedHeader = packed struct {
    bits: u8 align(1),
};

pub const ExternHeader = extern struct {
    code: c_int,
};

pub usingnamespace @import("mixin.zig");
usingnamespace Helpers;

pub comptime {
    registerTypes();

    const Generated = struct {
        fn generatedHook() void {
            generatedLeaf();
        }
    };
}

pub extern "c" fn qualified(
    destination: [*]u8,
    source: [*]const u8,
) align(16) addrspace(.generic) linksection(".text.qualified") callconv(.c) void;

pub fn owner(destination: [*]u8, source: [*]const u8) void {
    beforeLocal();
    @memcpy(destination, source);
    _ = @import("local.zig");

    const Local = struct {
        fn callback() void {
            callbackOnly();
            @branchHint(.cold);
        }
    };

    consume(struct {
        fn lessThan(_: void, left: u8, right: u8) bool {
            comparatorOnly();
            return left < right;
        }
    }.lessThan);

    comptime {
        compileOnly();
    }
    afterLocal();
}

export fn abiEntry(mapping: []align(32) u8) void {
    _ = mapping;
}

pub const PointerFields = struct {
    ptr: ?[*]align(4096) u8,
    probe_fn: *const fn (mapping: []align(32) u8) void,
};

pub const typed_value: struct {
    typed_one: u8,
    typed_two: u16,
} = .{ .typed_one = 1, .typed_two = 2 };

pub const Payload = union(enum) {
    fulfill: struct {
        pending: u8,
        buffer: []u8,
    },
};

pub fn anonymousShapes() type {
    const cases = [_]struct {
        size: usize,
        expected: []const u8,
    }{};
    _ = cases;
    return struct {
        value: u8 align(8),
        pad: [7]u8,
    };
}

pub const top_level_cases = [_]struct {
    name: []const u8,
    expected: usize,
}{};

pub const top_level_nested = .{
    struct {
        value: u8,
    }{ .value = 1 },
};

pub const CombinedError = error{
    Missing,
    Invalid,
} || error{
    Other,
};

fn beforeLocal() void {}
fn callbackOnly() void {}
fn comparatorOnly() void {}
fn compileOnly() void {}
fn afterLocal() void {}
fn generatedLeaf() void {}
fn registerTypes() void {}
fn consume(callback: anytype) void {
    _ = callback;
}

pub const Documented = struct {
    //! Documentation for the enclosing container.
    value: u8,
};

/// Outer documentation wins when the IR cannot preserve both positions.
pub const BothDocs = struct {
    //! Inner documentation cannot share the outer-doc placement bit.
    value: u8,
};
