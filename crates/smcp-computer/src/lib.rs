pub struct SmcpComputer {
    _private: (),
}

impl SmcpComputer {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for SmcpComputer {
    fn default() -> Self {
        Self::new()
    }
}
