const helper = @import(
    // Imports may contain comments between their tokens.
    "helper.zig",
);
const facade = @import("facade.zig");
const Widget = @import("Widget.zig");

// const decoy = @import("missing.zig");
const text =
    \\@import("also_missing.zig")
;

pub const Rule = enum { usingnamespace, async, await };

pub fn normal() void {}
pub fn @"quoted"() void {}
pub fn escapedCalls() void {
    @"normal"();
    quoted();
    helper.quoted();
    facade.escaped();
}

pub const Box = struct {
    pub fn init() Box {
        return .{};
    }
    pub fn ping(_: Box) void {}
};

pub fn inferredInit() Box {
    return .init();
}

pub fn typedInit() void {
    const box: Box = .init();
    box.ping();
}

pub fn fromInit() void {
    const box = Box.init();
    box.ping();
}

pub fn aliases() void {
    const run = helper.work;
    (run)();
    const callback: *const fn () void = normal;
    callback();
    @call(.auto, normal, .{});
    @field(helper, "work")();
    comptime {
        run();
    }
}

pub const Nested = struct {
    const local = @import("alternate.zig");
    const ThisType = @This();
    pub fn init() ThisType {
        const result: ThisType = .{};
        result.ping();
        return result;
    }
    pub fn ping(_: ThisType) void {
        local.work();
    }
};

pub const crate = struct {
    pub fn work() void {}
};
pub const super = struct {
    pub fn work() void {}
};
pub fn ordinaryNamespaces() void {
    crate.work();
    super.work();
}

pub fn throughFacade() u8 {
    const box: facade.Box = facade.Box.init(3);
    return box.read();
}

pub fn doctested() void {
    normal();
}

test doctested {
    doctested();
}

test "inline: test helpers keep their scope" {
    const Local = struct {
        fn run() void {
            normal();
        }
    };
    Local.run();
    typedInit();
    fromInit();
    _ = inferredInit();
    aliases();
    escapedCalls();
    ordinaryNamespaces();
    _ = Widget.init();
    _ = Nested.init();
    _ = throughFacade();
}

fn errorParameter(boundary: struct {
    err: error{ A, B },
}) void {
    _ = boundary;
}

fn fence() void {
    asm volatile ("" ::: .{ .memory = true });
}
