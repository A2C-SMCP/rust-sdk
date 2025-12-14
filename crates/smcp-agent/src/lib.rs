pub struct SmcpAgent {
    _private: (),
}

impl SmcpAgent {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for SmcpAgent {
    fn default() -> Self {
        Self::new()
    }
}
