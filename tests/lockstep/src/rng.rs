//! 可复现的伪随机数（SplitMix64）。同一个种子总是生成同一串指令和寄存器值。

/// 伪随机数生成器。
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// 用种子初始化。
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// 下一个 64 位随机数。
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// `[0, n)` 内的随机数。
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % n as u64) as u32
    }

    /// 随机的 `n` 位字段。
    pub fn bits(&mut self, n: u32) -> u32 {
        (self.next_u64() & ((1u64 << n) - 1)) as u32
    }

    /// 随机的一位。
    pub fn bit(&mut self) -> u32 {
        self.bits(1)
    }

    /// 从列表里随机取一个。
    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u32) as usize]
    }
}
