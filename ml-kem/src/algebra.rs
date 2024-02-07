use const_default::ConstDefault;
use core::ops::{Add, Mul, Sub};
use generic_array::{sequence::GenericSequence, GenericArray};
use sha3::digest::XofReader;
use typenum::consts::U256;

use crate::crypto::{PrfOutput, PRF, XOF};
use crate::encode::Encode;
use crate::param::{ArrayLength, CbdSamplingSize};
use crate::util::{FastClone, FunctionalArray, Truncate, B32};

pub type Integer = u16;

/// An element of GF(q).  Although `q` is only 16 bits wide, we use a wider uint type to so that we
/// can defer modular reductions.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FieldElement(pub Integer);

impl FieldElement {
    pub const Q: Integer = 3329;
    pub const Q32: u32 = Self::Q as u32;
    const Q64: u64 = Self::Q as u64;
    const BARRETT_SHIFT: usize = 24;
    const BARRETT_MULTIPLIER: u64 = (1 << Self::BARRETT_SHIFT) / Self::Q64;

    // A fast modular reduction for small numbers `x < 2*q`
    // TODO(RLB) Replace with constant-time version (~3-5% performance hit)
    fn small_reduce(x: u16) -> u16 {
        if x < Self::Q {
            x
        } else {
            x - Self::Q
        }
    }

    fn barrett_reduce(x: u32) -> u16 {
        let product = u64::from(x) * Self::BARRETT_MULTIPLIER;
        let quotient = (product >> Self::BARRETT_SHIFT).truncate();
        let remainder = x - quotient * Self::Q32;
        Self::small_reduce(remainder.truncate())
    }

    // Algorithm 11. BaseCaseMultiply
    //
    // This is a hot loop.  We promote to u64 so that we can do the absolute minimum number of
    // modular reductions, since these are the expensive operation.
    fn base_case_multiply(a0: Self, a1: Self, b0: Self, b1: Self, i: usize) -> (Self, Self) {
        let a0 = u32::from(a0.0);
        let a1 = u32::from(a1.0);
        let b0 = u32::from(b0.0);
        let b1 = u32::from(b1.0);
        let g = u32::from(GAMMA[i].0);

        let b1g = u32::from(Self::barrett_reduce(b1 * g));

        let c0 = Self::barrett_reduce(a0 * b0 + a1 * b1g);
        let c1 = Self::barrett_reduce(a0 * b1 + a1 * b0);
        (Self(c0), Self(c1))
    }
}

impl ConstDefault for FieldElement {
    const DEFAULT: Self = Self(0);
}

impl Add<FieldElement> for FieldElement {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(Self::small_reduce(self.0 + rhs.0))
    }
}

impl Sub<FieldElement> for FieldElement {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        // Guard against underflow if `rhs` is too large
        Self(Self::small_reduce(self.0 + Self::Q - rhs.0))
    }
}

impl Mul<FieldElement> for FieldElement {
    type Output = FieldElement;

    fn mul(self, rhs: FieldElement) -> FieldElement {
        let x = u32::from(self.0);
        let y = u32::from(rhs.0);
        Self(Self::barrett_reduce(x * y))
    }
}

/// An element of the ring `R_q`, i.e., a polynomial over `Z_q` of degree 255
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Polynomial(pub GenericArray<FieldElement, U256>);

impl ConstDefault for Polynomial {
    const DEFAULT: Self = Self(GenericArray::DEFAULT);
}

impl Add<&Polynomial> for &Polynomial {
    type Output = Polynomial;

    fn add(self, rhs: &Polynomial) -> Polynomial {
        Polynomial(self.0.zip(&rhs.0, |&x, &y| x + y))
    }
}

impl Sub<&Polynomial> for &Polynomial {
    type Output = Polynomial;

    fn sub(self, rhs: &Polynomial) -> Polynomial {
        Polynomial(self.0.zip(&rhs.0, |&x, &y| x - y))
    }
}

impl Mul<&Polynomial> for FieldElement {
    type Output = Polynomial;

    fn mul(self, rhs: &Polynomial) -> Polynomial {
        Polynomial(rhs.0.map(|&x| self * x))
    }
}

impl Polynomial {
    // A lookup table for CBD sampling:
    //
    //   ONES[i][j] = i.count_ones() - j.count_ones() mod Q
    //
    // fn main() {
    //     let q = 3329;
    //     let ones: [[u32; 8]; 8] = array::from_fn(|i| {
    //         array::from_fn(|j| {
    //             let x = i.count_ones();
    //             let y = j.count_ones();
    //             if y <= x {
    //                 x - y
    //             } else {
    //                 x + q - y
    //             }
    //         })
    //     });
    //     println!("ones = {:?}", ones);
    // }
    //
    // XXX(RLB): Empirically, this is not much faster than just doing the calculation inline.  But
    // it avoids having any branching, and pre-computing seems aesthetically nice.
    const ONES: [[u16; 8]; 8] = [
        [0, 3328, 3328, 3327, 3328, 3327, 3327, 3326],
        [1, 0, 0, 3328, 0, 3328, 3328, 3327],
        [1, 0, 0, 3328, 0, 3328, 3328, 3327],
        [2, 1, 1, 0, 1, 0, 0, 3328],
        [1, 0, 0, 3328, 0, 3328, 3328, 3327],
        [2, 1, 1, 0, 1, 0, 0, 3328],
        [2, 1, 1, 0, 1, 0, 0, 3328],
        [3, 2, 2, 1, 2, 1, 1, 0],
    ];

    // Algorithm 7. SamplePolyCBD_eta(B)
    //
    // To avoid all the bitwise manipulation in the algorithm as written, we reuse the logic in
    // ByteDecode.  We decode the PRF output into integers with eta bits, then use
    // `count_ones` to perform the summation described in the algorithm.
    pub fn sample_cbd<Eta>(B: &PrfOutput<Eta>) -> Self
    where
        Eta: CbdSamplingSize,
    {
        let vals: Polynomial = Encode::<Eta::SampleSize>::decode(B);
        Self(vals.0.map(|val| {
            // TODO Flatten ONES table to avoid the need for these operations
            let x = val.0 & ((1 << Eta::USIZE) - 1);
            let y = val.0 >> Eta::USIZE;
            FieldElement(Self::ONES[x as usize][y as usize])
        }))
    }
}

/// A vector of polynomials of length `k`
#[derive(Clone, Default, Debug, PartialEq)]
pub struct PolynomialVector<K: ArrayLength>(pub GenericArray<Polynomial, K>);

impl<K: ArrayLength> Add<PolynomialVector<K>> for PolynomialVector<K> {
    type Output = PolynomialVector<K>;

    fn add(self, rhs: PolynomialVector<K>) -> PolynomialVector<K> {
        PolynomialVector(self.0.zip(&rhs.0, |x, y| x + y))
    }
}

impl<K: ArrayLength> PolynomialVector<K> {
    pub fn sample_cbd<Eta>(sigma: &B32, start_n: u8) -> Self
    where
        Eta: CbdSamplingSize,
    {
        Self(GenericArray::generate(|i| {
            let N = start_n + i.truncate();
            let prf_output = PRF::<Eta>(sigma, N);
            Polynomial::sample_cbd::<Eta>(&prf_output)
        }))
    }
}

/// An element of the ring `T_q`, i.e., a tuple of 128 elements of the direct sum components of `T_q`.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct NttPolynomial(pub GenericArray<FieldElement, U256>);

impl ConstDefault for NttPolynomial {
    const DEFAULT: Self = Self(GenericArray::DEFAULT);
}

impl Add<&NttPolynomial> for &NttPolynomial {
    type Output = NttPolynomial;

    fn add(self, rhs: &NttPolynomial) -> NttPolynomial {
        NttPolynomial(self.0.zip(&rhs.0, |&x, &y| x + y))
    }
}

// Algorithm 6. SampleNTT (lines 4-13)
struct FieldElementReader<'a> {
    xof: &'a mut dyn XofReader,
    data: [u8; 96],
    start: usize,
    next: Option<Integer>,
}

impl<'a> FieldElementReader<'a> {
    fn new(xof: &'a mut impl XofReader) -> Self {
        let mut out = Self {
            xof,
            data: [0u8; 96],
            start: 0,
            next: None,
        };

        // Fill the buffer
        out.xof.read(&mut out.data);

        out
    }

    fn next(&mut self) -> FieldElement {
        if let Some(val) = self.next {
            self.next = None;
            return FieldElement(val);
        }

        loop {
            if self.start == self.data.len() {
                self.xof.read(&mut self.data);
                self.start = 0;
            }

            let end = self.start + 3;
            let b = &self.data[self.start..end];
            self.start = end;

            let d1 = Integer::from(b[0]) + ((Integer::from(b[1]) & 0xf) << 8);
            let d2 = (Integer::from(b[1]) >> 4) + ((Integer::from(b[2]) as Integer) << 4);

            if d1 < FieldElement::Q {
                if d2 < FieldElement::Q {
                    self.next = Some(d2);
                }
                return FieldElement(d1);
            }

            if d2 < FieldElement::Q {
                return FieldElement(d2);
            }
        }
    }
}

impl NttPolynomial {
    // Algorithm 6 SampleNTT(B)
    pub fn sample_uniform(B: &mut impl XofReader) -> Self {
        let mut reader = FieldElementReader::new(B);
        Self(GenericArray::generate(|_| reader.next()))
    }
}

// Since the powers of zeta used in the NTT and MultiplyNTTs are fixed, we use pre-computed tables
// to avoid the need to compute the exponetiations at runtime.
//
// * ZETA_POW_BITREV[i] = zeta^{BitRev_7(i)}
// * GAMMA[i] = zeta^{2 BitRev_7(i) + 1}
//
// The below code was used to generate these tables:
//
// fn bit_reverse(mut x: usize) -> usize {
//     let mut out = 0;
//     for _i in 0..7 {
//         out = (out << 1) + (x % 2);
//         x = x >> 1;
//     }
//     out
// }
//
// fn generate_zeta_gamma() {
//     const ZETA: FieldElement = FieldElement(17);
//
//     let mut pow = [FieldElement(0); 128];
//     pow[0] = FieldElement(1);
//     for i in 1..128 {
//         pow[i] = pow[i - 1] * ZETA;
//     }
//
//     let mut zeta_pow_bitrev = [FieldElement(0); 128];
//     for i in 0..128 {
//         zeta_pow_bitrev[i] = pow[bit_reverse(i)];
//     }
//     println!("ZETA_POW_BITREV: {:?}", zeta_pow_bitrev);
//
//     let mut gamma = [FieldElement(0); 128];
//     for i in 0..128 {
//         gamma[i as usize] = (zeta_pow_bitrev[i] * zeta_pow_bitrev[i]) * ZETA;
//     }
//     println!("GAMMA: {:?}", gamma);
// }
const ZETA_POW_BITREV: [FieldElement; 128] = [
    FieldElement(1),
    FieldElement(1729),
    FieldElement(2580),
    FieldElement(3289),
    FieldElement(2642),
    FieldElement(630),
    FieldElement(1897),
    FieldElement(848),
    FieldElement(1062),
    FieldElement(1919),
    FieldElement(193),
    FieldElement(797),
    FieldElement(2786),
    FieldElement(3260),
    FieldElement(569),
    FieldElement(1746),
    FieldElement(296),
    FieldElement(2447),
    FieldElement(1339),
    FieldElement(1476),
    FieldElement(3046),
    FieldElement(56),
    FieldElement(2240),
    FieldElement(1333),
    FieldElement(1426),
    FieldElement(2094),
    FieldElement(535),
    FieldElement(2882),
    FieldElement(2393),
    FieldElement(2879),
    FieldElement(1974),
    FieldElement(821),
    FieldElement(289),
    FieldElement(331),
    FieldElement(3253),
    FieldElement(1756),
    FieldElement(1197),
    FieldElement(2304),
    FieldElement(2277),
    FieldElement(2055),
    FieldElement(650),
    FieldElement(1977),
    FieldElement(2513),
    FieldElement(632),
    FieldElement(2865),
    FieldElement(33),
    FieldElement(1320),
    FieldElement(1915),
    FieldElement(2319),
    FieldElement(1435),
    FieldElement(807),
    FieldElement(452),
    FieldElement(1438),
    FieldElement(2868),
    FieldElement(1534),
    FieldElement(2402),
    FieldElement(2647),
    FieldElement(2617),
    FieldElement(1481),
    FieldElement(648),
    FieldElement(2474),
    FieldElement(3110),
    FieldElement(1227),
    FieldElement(910),
    FieldElement(17),
    FieldElement(2761),
    FieldElement(583),
    FieldElement(2649),
    FieldElement(1637),
    FieldElement(723),
    FieldElement(2288),
    FieldElement(1100),
    FieldElement(1409),
    FieldElement(2662),
    FieldElement(3281),
    FieldElement(233),
    FieldElement(756),
    FieldElement(2156),
    FieldElement(3015),
    FieldElement(3050),
    FieldElement(1703),
    FieldElement(1651),
    FieldElement(2789),
    FieldElement(1789),
    FieldElement(1847),
    FieldElement(952),
    FieldElement(1461),
    FieldElement(2687),
    FieldElement(939),
    FieldElement(2308),
    FieldElement(2437),
    FieldElement(2388),
    FieldElement(733),
    FieldElement(2337),
    FieldElement(268),
    FieldElement(641),
    FieldElement(1584),
    FieldElement(2298),
    FieldElement(2037),
    FieldElement(3220),
    FieldElement(375),
    FieldElement(2549),
    FieldElement(2090),
    FieldElement(1645),
    FieldElement(1063),
    FieldElement(319),
    FieldElement(2773),
    FieldElement(757),
    FieldElement(2099),
    FieldElement(561),
    FieldElement(2466),
    FieldElement(2594),
    FieldElement(2804),
    FieldElement(1092),
    FieldElement(403),
    FieldElement(1026),
    FieldElement(1143),
    FieldElement(2150),
    FieldElement(2775),
    FieldElement(886),
    FieldElement(1722),
    FieldElement(1212),
    FieldElement(1874),
    FieldElement(1029),
    FieldElement(2110),
    FieldElement(2935),
    FieldElement(885),
    FieldElement(2154),
];
const GAMMA: [FieldElement; 128] = [
    FieldElement(17),
    FieldElement(3312),
    FieldElement(2761),
    FieldElement(568),
    FieldElement(583),
    FieldElement(2746),
    FieldElement(2649),
    FieldElement(680),
    FieldElement(1637),
    FieldElement(1692),
    FieldElement(723),
    FieldElement(2606),
    FieldElement(2288),
    FieldElement(1041),
    FieldElement(1100),
    FieldElement(2229),
    FieldElement(1409),
    FieldElement(1920),
    FieldElement(2662),
    FieldElement(667),
    FieldElement(3281),
    FieldElement(48),
    FieldElement(233),
    FieldElement(3096),
    FieldElement(756),
    FieldElement(2573),
    FieldElement(2156),
    FieldElement(1173),
    FieldElement(3015),
    FieldElement(314),
    FieldElement(3050),
    FieldElement(279),
    FieldElement(1703),
    FieldElement(1626),
    FieldElement(1651),
    FieldElement(1678),
    FieldElement(2789),
    FieldElement(540),
    FieldElement(1789),
    FieldElement(1540),
    FieldElement(1847),
    FieldElement(1482),
    FieldElement(952),
    FieldElement(2377),
    FieldElement(1461),
    FieldElement(1868),
    FieldElement(2687),
    FieldElement(642),
    FieldElement(939),
    FieldElement(2390),
    FieldElement(2308),
    FieldElement(1021),
    FieldElement(2437),
    FieldElement(892),
    FieldElement(2388),
    FieldElement(941),
    FieldElement(733),
    FieldElement(2596),
    FieldElement(2337),
    FieldElement(992),
    FieldElement(268),
    FieldElement(3061),
    FieldElement(641),
    FieldElement(2688),
    FieldElement(1584),
    FieldElement(1745),
    FieldElement(2298),
    FieldElement(1031),
    FieldElement(2037),
    FieldElement(1292),
    FieldElement(3220),
    FieldElement(109),
    FieldElement(375),
    FieldElement(2954),
    FieldElement(2549),
    FieldElement(780),
    FieldElement(2090),
    FieldElement(1239),
    FieldElement(1645),
    FieldElement(1684),
    FieldElement(1063),
    FieldElement(2266),
    FieldElement(319),
    FieldElement(3010),
    FieldElement(2773),
    FieldElement(556),
    FieldElement(757),
    FieldElement(2572),
    FieldElement(2099),
    FieldElement(1230),
    FieldElement(561),
    FieldElement(2768),
    FieldElement(2466),
    FieldElement(863),
    FieldElement(2594),
    FieldElement(735),
    FieldElement(2804),
    FieldElement(525),
    FieldElement(1092),
    FieldElement(2237),
    FieldElement(403),
    FieldElement(2926),
    FieldElement(1026),
    FieldElement(2303),
    FieldElement(1143),
    FieldElement(2186),
    FieldElement(2150),
    FieldElement(1179),
    FieldElement(2775),
    FieldElement(554),
    FieldElement(886),
    FieldElement(2443),
    FieldElement(1722),
    FieldElement(1607),
    FieldElement(1212),
    FieldElement(2117),
    FieldElement(1874),
    FieldElement(1455),
    FieldElement(1029),
    FieldElement(2300),
    FieldElement(2110),
    FieldElement(1219),
    FieldElement(2935),
    FieldElement(394),
    FieldElement(885),
    FieldElement(2444),
    FieldElement(2154),
    FieldElement(1175),
];

// Algorithm 10. MuliplyNTTs
impl Mul<&NttPolynomial> for &NttPolynomial {
    type Output = NttPolynomial;

    fn mul(self, rhs: &NttPolynomial) -> NttPolynomial {
        let mut out = NttPolynomial(GenericArray::const_default());

        for i in 0..128 {
            let (c0, c1) = FieldElement::base_case_multiply(
                self.0[2 * i],
                self.0[2 * i + 1],
                rhs.0[2 * i],
                rhs.0[2 * i + 1],
                i,
            );

            out.0[2 * i] = c0;
            out.0[2 * i + 1] = c1;
        }

        out
    }
}

impl From<GenericArray<FieldElement, U256>> for NttPolynomial {
    fn from(f: GenericArray<FieldElement, U256>) -> NttPolynomial {
        NttPolynomial(f)
    }
}

impl From<NttPolynomial> for GenericArray<FieldElement, U256> {
    fn from(f_hat: NttPolynomial) -> GenericArray<FieldElement, U256> {
        f_hat.0
    }
}

// Algorithm 8. NTT
impl Polynomial {
    pub fn ntt(&self) -> NttPolynomial {
        let mut k = 1;

        let mut f = self.0;
        for len in [128, 64, 32, 16, 8, 4, 2] {
            for start in (0..256).step_by(2 * len) {
                let zeta = ZETA_POW_BITREV[k];
                k += 1;

                for j in start..(start + len) {
                    let t = zeta * f[j + len];
                    f[j + len] = f[j] - t;
                    f[j] = f[j] + t;
                }
            }
        }

        f.into()
    }
}

// Algorithm 9. NTT^{-1}
impl NttPolynomial {
    pub fn ntt_inverse(&self) -> Polynomial {
        let mut f: GenericArray<FieldElement, U256> = self.0.fast_clone();

        let mut k = 127;
        for len in [2, 4, 8, 16, 32, 64, 128] {
            for start in (0..256).step_by(2 * len) {
                let zeta = ZETA_POW_BITREV[k];
                k -= 1;

                for j in start..(start + len) {
                    let t = f[j];
                    f[j] = t + f[j + len];
                    f[j + len] = zeta * (f[j + len] - t);
                }
            }
        }

        FieldElement(3303) * &Polynomial(f)
    }
}

/// A vector of K NTT-domain polynomials
#[derive(Clone, Default, Debug, PartialEq)]
pub struct NttVector<K: ArrayLength>(pub GenericArray<NttPolynomial, K>);

impl<K: ArrayLength> NttVector<K> {
    // Note the transpose here: Apparently the specification is incorrect, and the proper order
    // of indices is reversed.
    //
    // https://github.com/FiloSottile/mlkem768/blob/main/mlkem768.go#L110C4-L112C51
    pub fn sample_uniform(rho: &B32, i: usize, transpose: bool) -> Self {
        Self(GenericArray::generate(|j| {
            let (i, j) = if transpose { (i, j) } else { (j, i) };
            let mut xof = XOF(rho, i.truncate(), j.truncate());
            NttPolynomial::sample_uniform(&mut xof)
        }))
    }
}

impl<K: ArrayLength> Add<&NttVector<K>> for &NttVector<K> {
    type Output = NttVector<K>;

    fn add(self, rhs: &NttVector<K>) -> NttVector<K> {
        NttVector(self.0.zip(&rhs.0, |x, y| x + y))
    }
}

impl<K: ArrayLength> Mul<&NttVector<K>> for &NttVector<K> {
    type Output = NttPolynomial;

    fn mul(self, rhs: &NttVector<K>) -> NttPolynomial {
        self.0.zip(&rhs.0, |x, y| x * y).fold(|x, y| x + y)
    }
}

impl<K: ArrayLength> PolynomialVector<K> {
    pub fn ntt(&self) -> NttVector<K> {
        NttVector(self.0.map(Polynomial::ntt))
    }
}

impl<K: ArrayLength> NttVector<K> {
    pub fn ntt_inverse(&self) -> PolynomialVector<K> {
        PolynomialVector(self.0.map(NttPolynomial::ntt_inverse))
    }
}

/// A K x K matrix of NTT-domain polynomials.  Each vector represents a row of the matrix, so that
/// multiplying on the right just requires iteration.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct NttMatrix<K: ArrayLength>(GenericArray<NttVector<K>, K>);

impl<K: ArrayLength> Mul<&NttVector<K>> for &NttMatrix<K> {
    type Output = NttVector<K>;

    fn mul(self, rhs: &NttVector<K>) -> NttVector<K> {
        NttVector(self.0.map(|x| x * rhs))
    }
}

impl<K: ArrayLength> NttMatrix<K> {
    pub fn sample_uniform(rho: &B32, transpose: bool) -> Self {
        Self(GenericArray::generate(|i| {
            NttVector::sample_uniform(rho, i, transpose)
        }))
    }

    pub fn transpose(&self) -> Self {
        Self(GenericArray::generate(|i| {
            NttVector(GenericArray::generate(|j| self.0[j].0[i].clone()))
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::util::Flatten;
    use generic_array::arr;
    use typenum::consts::{U2, U3, U8};

    // Multiplication in R_q, modulo X^256 + 1
    impl Mul<&Polynomial> for &Polynomial {
        type Output = Polynomial;

        fn mul(self, rhs: &Polynomial) -> Self::Output {
            let mut out = Self::Output::DEFAULT;
            for (i, x) in self.0.iter().enumerate() {
                for (j, y) in rhs.0.iter().enumerate() {
                    let (sign, index) = if i + j < 256 {
                        (FieldElement(1), i + j)
                    } else {
                        (FieldElement(FieldElement::Q - 1), i + j - 256)
                    };

                    out.0[index] = out.0[index] + (sign * *x * *y);
                }
            }
            out
        }
    }

    // A polynomial with only a scalar component, to make simple test cases
    fn const_ntt(x: Integer) -> NttPolynomial {
        let mut p = Polynomial::DEFAULT;
        p.0[0] = FieldElement(x);
        p.ntt()
    }

    #[test]
    fn polynomial_ops() {
        let f = Polynomial(GenericArray::generate(|i| FieldElement(i as Integer)));
        let g = Polynomial(GenericArray::generate(|i| FieldElement(2 * i as Integer)));
        let sum = Polynomial(GenericArray::generate(|i| FieldElement(3 * i as Integer)));
        assert_eq!((&f + &g), sum);
        assert_eq!((&sum - &g), f);
        assert_eq!(FieldElement(3) * &f, sum);
    }

    #[test]
    fn ntt() {
        let f = Polynomial(GenericArray::generate(|i| FieldElement(i as Integer)));
        let g = Polynomial(GenericArray::generate(|i| FieldElement(2 * i as Integer)));
        let f_hat = f.ntt();
        let g_hat = g.ntt();

        // Verify that NTT and NTT^-1 are actually inverses
        let f_unhat = f_hat.ntt_inverse();
        assert_eq!(f, f_unhat);

        // Verify that NTT is a homomorphism with regard to addition
        let fg = &f + &g;
        let f_hat_g_hat = &f_hat + &g_hat;
        let fg_unhat = f_hat_g_hat.ntt_inverse();
        assert_eq!(fg, fg_unhat);

        // Verify that NTT is a homomorphism with regard to multiplication
        let fg = &f * &g;
        let f_hat_g_hat = &f_hat * &g_hat;
        let fg_unhat = f_hat_g_hat.ntt_inverse();
        assert_eq!(fg, fg_unhat);
    }

    #[test]
    fn ntt_vector() {
        // Verify vector addition
        let v1 = NttVector(arr![const_ntt(1), const_ntt(1), const_ntt(1)]);
        let v2 = NttVector(arr![const_ntt(2), const_ntt(2), const_ntt(2)]);
        let v3 = NttVector(arr![const_ntt(3), const_ntt(3), const_ntt(3)]);
        assert_eq!((&v1 + &v2), v3);

        // Verify dot product
        assert_eq!((&v1 * &v2), const_ntt(6));
        assert_eq!((&v1 * &v3), const_ntt(9));
        assert_eq!((&v2 * &v3), const_ntt(18));
    }

    #[test]
    fn ntt_matrix() {
        // Verify matrix multiplication by a vector
        let a = NttMatrix(arr![
            NttVector(arr![const_ntt(1), const_ntt(2), const_ntt(3)]),
            NttVector(arr![const_ntt(4), const_ntt(5), const_ntt(6)]),
            NttVector(arr![const_ntt(7), const_ntt(8), const_ntt(9)]),
        ]);
        let v_in = NttVector(arr![const_ntt(1), const_ntt(2), const_ntt(3)]);
        let v_out = NttVector(arr![const_ntt(14), const_ntt(32), const_ntt(50)]);
        assert_eq!(&a * &v_in, v_out);

        // Verify transpose
        let aT = NttMatrix(arr![
            NttVector(arr![const_ntt(1), const_ntt(4), const_ntt(7)]),
            NttVector(arr![const_ntt(2), const_ntt(5), const_ntt(8)]),
            NttVector(arr![const_ntt(3), const_ntt(6), const_ntt(9)]),
        ]);
        assert_eq!(a.transpose(), aT);
    }

    // To verify the accuracy of sampling, we use a theorem related to the law of large numbers,
    // which bounds the convergence of the Kullback-Liebler distance between the empirical
    // distribution and the hypothesized distribution.
    //
    // Theorem (Cover & Thomas, 1991, Theorem 12.2.1): Let $X_1, \ldots, X_n$ be i.i.d. $~P(x)$.
    // Then:
    //
    //   Pr{ D(P_{x^n} || P) > \epsilon } \leq 2^{ -n ( \epsilon - |X|^{ log(n+1) / n } ) }
    //
    // So if we test by computing D(P_{x^n} || P) and requiring the value to be below a threshold
    // \epsilon, then an unbiased sampling should pass with overwhelming probability 1 - 2^{-k},
    // for some k based on \epsilon, |X|, and n.
    //
    // If we take k = 256 and n = 256, then we can solve for the required threshold \epsilon:
    //
    //   \epsilon = 1 + |X|^{ 0.03125 }
    //
    // For the cases we're interested in here:
    //
    //   CBD(eta = 2) => |X| = 5   => epsilon ~= 2.0516
    //   CBD(eta = 2) => |X| = 7   => epsilon ~= 2.0627
    //   Uniform byte => |X| = 256 => epsilon ~= 2.1892
    //
    // Taking epsilon = 2.05 makes us conservative enough in all cases, without significantly
    // increasing the probability of false negatives.
    const KL_THRESHOLD: f64 = 2.05;

    // The centered binomial distributions are calculated as:
    //
    //   bin_\eta(k) = (2\eta \choose k + \eta) 2^{-2\eta}
    //
    // for k in $-\eta, \ldots, \eta$.  The cases of interest here are \eta = 2, 3.
    type Distribution = [f64; Q_SIZE];
    const Q_SIZE: usize = FieldElement::Q as usize;
    const CBD2: Distribution = {
        let mut dist = [0.0; Q_SIZE];
        dist[Q_SIZE - 2] = 1.0 / 16.0;
        dist[Q_SIZE - 1] = 4.0 / 16.0;
        dist[0] = 6.0 / 16.0;
        dist[1] = 4.0 / 16.0;
        dist[2] = 1.0 / 16.0;
        dist
    };
    const CBD3: Distribution = {
        let mut dist = [0.0; Q_SIZE];
        dist[Q_SIZE - 3] = 1.0 / 64.0;
        dist[Q_SIZE - 2] = 6.0 / 64.0;
        dist[Q_SIZE - 1] = 15.0 / 64.0;
        dist[0] = 20.0 / 64.0;
        dist[1] = 15.0 / 64.0;
        dist[2] = 6.0 / 64.0;
        dist[3] = 1.0 / 64.0;
        dist
    };
    const UNIFORM: Distribution = [1.0 / (FieldElement::Q as f64); Q_SIZE];

    fn kl_divergence(p: &Distribution, q: &Distribution) -> f64 {
        p.iter()
            .zip(q.iter())
            .map(|(p, q)| if *p == 0.0 { 0.0 } else { p * (p / q).log2() })
            .sum()
    }

    fn test_sample(sample: &[FieldElement], ref_dist: &Distribution) {
        // Verify data and compute the empirical distribution
        let mut sample_dist: Distribution = [0.0; Q_SIZE];
        let bump: f64 = 1.0 / (sample.len() as f64);
        for x in sample {
            assert!(x.0 < FieldElement::Q);
            assert!(ref_dist[x.0 as usize] > 0.0);

            sample_dist[x.0 as usize] += bump;
        }

        let d = kl_divergence(&sample_dist, ref_dist);
        assert!(d < KL_THRESHOLD);
    }

    #[test]
    fn sample_uniform() {
        // We require roughly Q/2 samples to verify the uniform distribution.  This is because for
        // M < N, the uniform distribution over a subset of M elements has KL distance:
        //
        //   M sum(p * log(q / p)) = log(q / p) = log(N / M)
        //
        // Since Q ~= 2^11 and 256 == 2^8, we need 2^3 == 8 runs of 256 to get out of the bad
        // regime and get a meaningful measurement.
        let rho = B32::const_default();
        let sample: GenericArray<GenericArray<FieldElement, U256>, U8> =
            GenericArray::generate(|i| {
                let mut xof = XOF(&rho, 0, i as u8);
                NttPolynomial::sample_uniform(&mut xof).into()
            });

        test_sample(&sample.flatten(), &UNIFORM);
    }

    #[test]
    fn sample_cbd() {
        // Eta = 2
        let sigma = B32::const_default();
        let prf_output = PRF::<U2>(&sigma, 0);
        let sample = Polynomial::sample_cbd::<U2>(&prf_output).0;
        test_sample(&sample, &CBD2);

        // Eta = 3
        let sigma = B32::const_default();
        let prf_output = PRF::<U3>(&sigma, 0);
        let sample = Polynomial::sample_cbd::<U3>(&prf_output).0;
        test_sample(&sample, &CBD3);
    }
}
