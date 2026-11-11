// Fixed 256-bit integers keep dec128 intermediates and aggregates exact.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(32))]
pub struct U256(pub [u64; 4]);
impl U256 {
    pub const ZERO: Self = Self([0; 4]);
    pub fn from_u128(v: u128) -> Self {
        Self([v as u64, (v >> 64) as u64, 0, 0])
    }
    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
    fn bit(self, i: usize) -> bool {
        (self.0[i / 64] >> (i % 64)) & 1 != 0
    }
    fn set_bit(&mut self, i: usize) {
        self.0[i / 64] |= 1u64 << (i % 64);
    }
    fn wrapping_add(self, r: Self) -> Self {
        let mut out = [0; 4];
        let mut carry = 0u128;
        for i in 0..4 {
            let v = self.0[i] as u128 + r.0[i] as u128 + carry;
            out[i] = v as u64;
            carry = v >> 64;
        }
        Self(out)
    }
    pub fn checked_add(self, r: Self) -> Option<Self> {
        let out = self.wrapping_add(r);
        if out < self {
            None
        } else {
            Some(out)
        }
    }
    fn wrapping_sub(self, r: Self) -> Self {
        let mut out = [0; 4];
        let mut borrow = false;
        for i in 0..4 {
            let (a, b) = self.0[i].overflowing_sub(r.0[i]);
            let (v, c) = a.overflowing_sub(borrow as u64);
            out[i] = v;
            borrow = b || c;
        }
        Self(out)
    }
    pub fn checked_sub(self, r: Self) -> Option<Self> {
        if self < r {
            None
        } else {
            Some(self.wrapping_sub(r))
        }
    }
    pub fn checked_mul(self, r: Self) -> Option<Self> {
        let mut out = [0u64; 8];
        for i in 0..4 {
            let mut carry = 0u128;
            for j in 0..4 {
                let k = i + j;
                let v = self.0[i] as u128 * r.0[j] as u128 + out[k] as u128 + carry;
                out[k] = v as u64;
                carry = v >> 64;
            }
            let mut k = i + 4;
            while carry != 0 && k < 8 {
                let v = out[k] as u128 + carry;
                out[k] = v as u64;
                carry = v >> 64;
                k += 1;
            }
        }
        if out[4..].iter().any(|&x| x != 0) {
            None
        } else {
            Some(Self(out[..4].try_into().unwrap()))
        }
    }
    pub fn div_rem(self, r: Self) -> Option<(Self, Self)> {
        if r.is_zero() {
            return None;
        }
        let mut q = Self::ZERO;
        let mut rem = Self::ZERO;
        for i in (0..256).rev() {
            let carry = rem.0[3] >> 63 != 0;
            for j in (0..4).rev() {
                rem.0[j] = (rem.0[j] << 1)
                    | if j > 0 {
                        rem.0[j - 1] >> 63
                    } else {
                        self.bit(i) as u64
                    };
            }
            if carry || rem >= r {
                rem = rem.wrapping_sub(r);
                q.set_bit(i);
            }
        }
        Some((q, rem))
    }
    pub fn checked_div(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.0)
    }
    pub fn checked_rem(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.1)
    }
    pub fn to_u128(self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            None
        } else {
            Some(self.0[0] as u128 | ((self.0[1] as u128) << 64))
        }
    }
    pub fn to_le_bytes(self) -> [u8; 32] {
        let mut b = [0u8; 32];
        for (i, v) in self.0.iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    pub fn from_le_bytes(b: [u8; 32]) -> Self {
        let mut w = [0; 4];
        for i in 0..4 {
            w[i] = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        }
        Self(w)
    }
}
impl Ord for U256 {
    fn cmp(&self, r: &Self) -> std::cmp::Ordering {
        for i in (0..4).rev() {
            let c = self.0[i].cmp(&r.0[i]);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        std::cmp::Ordering::Equal
    }
}
impl PartialOrd for U256 {
    fn partial_cmp(&self, r: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(r))
    }
}
impl std::fmt::Display for U256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut n = *self;
        if n.is_zero() {
            return write!(f, "0");
        }
        let mut digits = Vec::new();
        while !n.is_zero() {
            let (q, r) = n.div_rem(Self::from_u128(10)).unwrap();
            digits.push(b'0' + r.0[0] as u8);
            n = q;
        }
        for d in digits.iter().rev() {
            write!(f, "{}", *d as char)?;
        }
        Ok(())
    }
}
impl std::fmt::Debug for U256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct I256(pub U256);
impl I256 {
    pub const ZERO: Self = Self(U256::ZERO);
    pub fn from_i128(v: i128) -> Self {
        Self(U256([
            v as u64,
            (v >> 64) as u64,
            if v < 0 { u64::MAX } else { 0 },
            if v < 0 { u64::MAX } else { 0 },
        ]))
    }
    pub fn from_u128(v: u128) -> Self {
        Self(U256::from_u128(v))
    }
    pub fn negative(self) -> bool {
        self.0 .0[3] >> 63 != 0
    }
    fn magnitude(self) -> U256 {
        if self.negative() {
            U256::ZERO.wrapping_sub(self.0)
        } else {
            self.0
        }
    }
    fn signed_magnitude(m: U256, negative: bool) -> Option<Self> {
        let high = m.0[3];
        if (!negative && high >> 63 != 0)
            || (negative
                && (high > 0x8000000000000000
                    || (high == 0x8000000000000000 && m.0[..3].iter().any(|&x| x != 0))))
        {
            return None;
        }
        Some(Self(if negative {
            U256::ZERO.wrapping_sub(m)
        } else {
            m
        }))
    }
    pub fn checked_add(self, r: Self) -> Option<Self> {
        let v = Self(self.0.wrapping_add(r.0));
        if self.negative() == r.negative() && v.negative() != self.negative() {
            None
        } else {
            Some(v)
        }
    }
    pub fn checked_sub(self, r: Self) -> Option<Self> {
        let v = Self(self.0.wrapping_sub(r.0));
        if self.negative() != r.negative() && v.negative() != self.negative() {
            None
        } else {
            Some(v)
        }
    }
    pub fn checked_neg(self) -> Option<Self> {
        Self::ZERO.checked_sub(self)
    }
    pub fn checked_mul(self, r: Self) -> Option<Self> {
        Self::signed_magnitude(
            self.magnitude().checked_mul(r.magnitude())?,
            self.negative() != r.negative(),
        )
    }
    pub fn div_rem(self, r: Self) -> Option<(Self, Self)> {
        let (q, m) = self.magnitude().div_rem(r.magnitude())?;
        Some((
            Self::signed_magnitude(q, self.negative() != r.negative())?,
            Self::signed_magnitude(m, self.negative())?,
        ))
    }
    pub fn checked_div(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.0)
    }
    pub fn checked_rem(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.1)
    }
    pub fn to_i128(self) -> Option<i128> {
        let n = self.magnitude().to_u128()?;
        if self.negative() {
            if n == (1u128 << 127) {
                Some(i128::MIN)
            } else if n <= i128::MAX as u128 {
                Some(-(n as i128))
            } else {
                None
            }
        } else if n <= i128::MAX as u128 {
            Some(n as i128)
        } else {
            None
        }
    }
    pub fn to_u128(self) -> Option<u128> {
        if self.negative() {
            None
        } else {
            self.0.to_u128()
        }
    }
    pub fn to_le_bytes(self) -> [u8; 32] {
        self.0.to_le_bytes()
    }
    pub fn from_le_bytes(b: [u8; 32]) -> Self {
        Self(U256::from_le_bytes(b))
    }
}
impl Ord for I256 {
    fn cmp(&self, r: &Self) -> std::cmp::Ordering {
        if self.negative() != r.negative() {
            if self.negative() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        } else {
            self.0.cmp(&r.0)
        }
    }
}
impl PartialOrd for I256 {
    fn partial_cmp(&self, r: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(r))
    }
}
impl std::fmt::Display for I256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.negative() {
            write!(f, "-")?;
        }
        std::fmt::Display::fmt(&self.magnitude(), f)
    }
}
impl std::fmt::Debug for I256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
fn wide_product(a: U256, b: U256) -> [u64; 8] {
    let mut out = [0u64; 8];
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let k = i + j;
            let v = a.0[i] as u128 * b.0[j] as u128 + out[k] as u128 + carry;
            out[k] = v as u64;
            carry = v >> 64;
        }
        let mut k = i + 4;
        while carry != 0 && k < 8 {
            let v = out[k] as u128 + carry;
            out[k] = v as u64;
            carry = v >> 64;
            k += 1;
        }
    }
    out
}
fn wide_quotient(n: [u64; 8], d: U256) -> Option<([u64; 8], U256)> {
    if d.is_zero() {
        return None;
    }
    let mut q = [0u64; 8];
    let mut rem = U256::ZERO;
    for i in (0..512).rev() {
        let carry = rem.0[3] >> 63 != 0;
        for j in (0..4).rev() {
            rem.0[j] = (rem.0[j] << 1)
                | if j > 0 {
                    rem.0[j - 1] >> 63
                } else {
                    (n[i / 64] >> (i % 64)) & 1
                };
        }
        if carry || rem >= d {
            rem = rem.wrapping_sub(d);
            q[i / 64] |= 1u64 << (i % 64);
        }
    }
    Some((q, rem))
}
fn decimal_wide_result(n: [u64; 8], d: U256, negative: bool) -> Result<I256, ErrorCode> {
    let (q, r) = wide_quotient(n, d).ok_or(ErrorCode::ArithOverflow)?;
    if !r.is_zero() || q[4..].iter().any(|&x| x != 0) {
        return Err(ErrorCode::ArithOverflow);
    }
    I256::signed_magnitude(U256(q[..4].try_into().unwrap()), negative)
        .ok_or(ErrorCode::ArithOverflow)
}
fn decimal_wide_mul(a: I256, b: I256, scale: u128) -> Result<I256, ErrorCode> {
    decimal_wide_result(
        wide_product(a.magnitude(), b.magnitude()),
        U256::from_u128(scale),
        a.negative() != b.negative(),
    )
}
fn decimal_wide_div(a: I256, b: I256, scale: u128) -> Result<I256, ErrorCode> {
    decimal_wide_result(
        wide_product(a.magnitude(), U256::from_u128(scale)),
        b.magnitude(),
        a.negative() != b.negative(),
    )
}
fn decimal_mul(a: i128, b: i128, scale: i128) -> Result<i128, ErrorCode> {
    decimal_wide_mul(I256::from_i128(a), I256::from_i128(b), scale as u128)?
        .to_i128()
        .ok_or(ErrorCode::ArithOverflow)
}
fn decimal_div(a: i128, b: i128, scale: i128) -> Result<i128, ErrorCode> {
    decimal_wide_div(I256::from_i128(a), I256::from_i128(b), scale as u128)?
        .to_i128()
        .ok_or(ErrorCode::ArithOverflow)
}
