pub struct Alpha {
    pub value: i32,
}

pub enum Mode {
    Fast,
    Slow,
}

pub trait Runnable {
    fn run(&self);
}

impl Alpha {
    pub fn new(value: i32) -> Self {
        Self { value }
    }
}

impl Runnable for Alpha {
    fn run(&self) {}
}

pub fn alpha_needle() -> &'static str {
    "needle alpha"
}
