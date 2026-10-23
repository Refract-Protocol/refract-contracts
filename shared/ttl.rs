pub const MIN_TTL_THRESHOLD: u32 = 17_280;
pub const MAX_TTL_EXTEND: u32 = 535_680;
pub const MAX_BUMP_BATCH: u32 = 16;

pub fn clamp_extend(requested: u32) -> u32 {
    requested.clamp(MIN_TTL_THRESHOLD, MAX_TTL_EXTEND)
}