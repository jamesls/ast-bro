const Widget = @This();
value: u8 = 0,

pub fn init() Widget {
    var result: Widget = .{};
    result.reset();
    return result;
}

pub fn reset(self: *Widget) void {
    self.value = 0;
}
