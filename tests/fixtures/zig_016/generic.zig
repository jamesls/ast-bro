pub fn Box(comptime T: type) type {
    return struct {
        value: T,
        pub fn init(value: T) @This() {
            return .{ .value = value };
        }
        pub fn read(self: @This()) T {
            return self.value;
        }
    };
}

pub const IntBox = Box(u8);

pub fn useGeneric() u8 {
    const box = IntBox.init(1);
    return box.read();
}
