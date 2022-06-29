//! Sequence numbering.

use std::{
    cmp::Ordering,
    fmt,
    ops::{Add, AddAssign, Sub, SubAssign},
};

/// A 32-bit sequence number that wraps around.
///
/// The difference between the lowest and highest used sequence number
/// must not exceed [Self::USABLE_INTERVAL], otherwise comparision
/// will produce faulty results.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Seq(pub u32);

impl Seq {
    /// Zero.
    pub const ZERO: Self = Self(0);

    /// Minus one.
    pub const MINUS_ONE: Self = Self(0u32.wrapping_sub(1));

    const ONE_QUARTER: u32 = u32::MAX / 4;
    const THREE_QUARTERS: u32 = Self::ONE_QUARTER * 3;

    /// Useable interval.
    pub const USABLE_INTERVAL: i32 = Self::ONE_QUARTER as i32;
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u32> for Seq {
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<Seq> for u32 {
    fn from(v: Seq) -> Self {
        v.0
    }
}

impl Add<u32> for Seq {
    type Output = Self;
    fn add(self, rhs: u32) -> Self::Output {
        Self(self.0.wrapping_add(rhs))
    }
}

impl Add<i32> for Seq {
    type Output = Self;
    fn add(self, rhs: i32) -> Self::Output {
        Self(self.0.wrapping_add(rhs as u32))
    }
}

impl AddAssign<u32> for Seq {
    fn add_assign(&mut self, rhs: u32) {
        *self = *self + rhs;
    }
}

impl Sub<Seq> for Seq {
    type Output = i32;
    fn sub(self, rhs: Seq) -> Self::Output {
        self.0.wrapping_sub(rhs.0) as i32
    }
}

impl Sub<u32> for Seq {
    type Output = Self;
    fn sub(self, rhs: u32) -> Self::Output {
        Self(self.0.wrapping_sub(rhs))
    }
}

impl Sub<i32> for Seq {
    type Output = Self;
    fn sub(self, rhs: i32) -> Self::Output {
        Self(self.0.wrapping_sub(rhs as u32))
    }
}

impl SubAssign<u32> for Seq {
    fn sub_assign(&mut self, rhs: u32) {
        *self = *self - rhs;
    }
}

impl Sub<usize> for Seq {
    type Output = Self;
    fn sub(self, rhs: usize) -> Self::Output {
        let rhs: u32 = rhs.try_into().unwrap();
        self - rhs
    }
}

impl PartialOrd for Seq {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Seq {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.0 < Self::ONE_QUARTER && other.0 >= Self::THREE_QUARTERS {
            Ordering::Greater
        } else if other.0 < Self::ONE_QUARTER && self.0 >= Self::THREE_QUARTERS {
            Ordering::Less
        } else {
            self.0.cmp(&other.0)
        }
    }
}
