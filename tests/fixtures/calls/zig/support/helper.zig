pub fn work() usize {
    return 42;
}

pub const Nested = struct {
    pub fn work() usize {
        return 7;
    }
};

// A second print declaration keeps std.debug.print unresolved, allowing the
// caller JSON to expose the receiver text for the grammar-pinning assertion.
pub fn print() void {}
