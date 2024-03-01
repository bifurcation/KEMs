use core::mem::ManuallyDrop;
use core::ops::{Div, Mul, Rem};
use core::ptr;
use hybrid_array::{
    typenum::{
        operator_aliases::{Prod, Quot},
        Unsigned, U0, U32,
    },
    Array, ArraySize,
};

/// A 32-byte array, defined here for brevity because it is used several times
pub type B32 = Array<u8, U32>;

/// Benchmarking shows that `Array::clone` does not optimize as well as this alternative
/// implementation.  (Obviously, we can't re-implement Clone, so we have a new name.)
pub trait FastClone {
    fn fast_clone(&self) -> Self;
}

impl<T, N> FastClone for Array<T, N>
where
    T: Copy + Default,
    N: ArraySize,
{
    fn fast_clone(&self) -> Self {
        self.map(Clone::clone)
    }
}

/// Benchmarking shows that the `FunctionalSequence` versions of `zip`, `fold`, and `map` do not
/// optimize as well as these alternative implementations.
pub trait FunctionalArray<T, N>
where
    N: ArraySize,
{
    fn map<U, F>(&self, f: F) -> Array<U, N>
    where
        U: Default,
        F: Fn(&T) -> U;

    fn zip<U, F>(&self, b: &Self, f: F) -> Array<U, N>
    where
        U: Default,
        F: Fn(&T, &T) -> U;

    fn fold<F>(&self, f: F) -> T
    where
        T: Clone,
        F: Fn(&T, &T) -> T;
}

impl<T, N> FunctionalArray<T, N> for Array<T, N>
where
    N: ArraySize,
{
    fn map<U, F>(&self, f: F) -> Array<U, N>
    where
        U: Default,
        F: Fn(&T) -> U,
    {
        Array::from_fn(|i| f(&self[i]))
    }

    fn zip<U, F>(&self, other: &Self, f: F) -> Array<U, N>
    where
        U: Default,
        F: Fn(&T, &T) -> U,
    {
        Array::from_fn(|i| f(&self[i], &other[i]))
    }

    fn fold<F>(&self, f: F) -> T
    where
        T: Clone,
        F: Fn(&T, &T) -> T,
    {
        let mut out = self[0].clone();
        for i in 1..N::USIZE {
            out = f(&out, &self[i]);
        }
        out
    }
}

/// Safely truncate an unsigned integer value to shorter representation
pub trait Truncate<T> {
    fn truncate(self) -> T;
}

macro_rules! define_truncate {
    ($from:ident, $to:ident) => {
        impl Truncate<$to> for $from {
            fn truncate(self) -> $to {
                // This line is marked unsafe because the `unwrap_unchecked` call is UB when its
                // `self` argument is `Err`.  It never will be, because we explicitly zeroize the
                // high-order bits before converting.  We could have used `unwrap()`, but chose to
                // avoid the possibility of panic.
                unsafe { (self & $from::from($to::MAX)).try_into().unwrap_unchecked() }
            }
        }
    };
}

define_truncate!(u32, u16);
define_truncate!(u64, u32);
define_truncate!(usize, u8);
define_truncate!(u128, u16);
define_truncate!(u128, u8);

/// Defines a sequence of sequences that can be merged into a bigger overall seequence
pub trait Flatten<T, M: ArraySize> {
    type OutputSize: ArraySize;

    fn flatten(self) -> Array<T, Self::OutputSize>;
}

impl<T, N, M> Flatten<T, Prod<M, N>> for Array<Array<T, M>, N>
where
    N: ArraySize,
    M: ArraySize + Mul<N>,
    Prod<M, N>: ArraySize,
{
    type OutputSize = Prod<M, N>;

    // This is the reverse transmute between [T; K*N] and [[T; K], M], which is guaranteed to be
    // safe by the Rust memory layout of these types.
    fn flatten(self) -> Array<T, Self::OutputSize> {
        let whole = ManuallyDrop::new(self);
        unsafe { ptr::read(whole.as_ptr().cast()) }
    }
}

/// Defines a sequence that can be split into a sequence of smaller sequences of uniform size
pub trait Unflatten<M>
where
    M: ArraySize,
{
    type Part;

    fn unflatten(self) -> Array<Self::Part, M>;
}

impl<T, N, M> Unflatten<M> for Array<T, N>
where
    T: Default,
    N: ArraySize + Div<M> + Rem<M, Output = U0>,
    M: ArraySize,
    Quot<N, M>: ArraySize,
{
    type Part = Array<T, Quot<N, M>>;

    // This requires some unsafeness, but it is the same as what is done in Array::split.
    // Basically, this is doing transmute between [T; K*N] and [[T; K], M], which is guaranteed to
    // be safe by the Rust memory layout of these types.
    fn unflatten(self) -> Array<Self::Part, M> {
        let part_size = Quot::<N, M>::USIZE;
        let whole = ManuallyDrop::new(self);
        Array::from_fn(|i| unsafe { ptr::read(whole.as_ptr().add(i * part_size).cast()) })
    }
}

impl<'a, T, N, M> Unflatten<M> for &'a Array<T, N>
where
    T: Default,
    N: ArraySize + Div<M> + Rem<M, Output = U0>,
    M: ArraySize,
    Quot<N, M>: ArraySize,
{
    type Part = &'a Array<T, Quot<N, M>>;

    // This requires some unsafeness, but it is the same as what is done in Array::split.
    // Basically, this is doing transmute between [T; K*N] and [[T; K], M], which is guaranteed to
    // be safe by the Rust memory layout of these types.
    fn unflatten(self) -> Array<Self::Part, M> {
        let part_size = Quot::<N, M>::USIZE;
        let mut ptr: *const T = self.as_ptr();
        Array::from_fn(|_i| unsafe {
            let part = &*(ptr.cast());
            ptr = ptr.add(part_size);
            part
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use hybrid_array::typenum::consts::*;

    #[test]
    fn flatten() {
        let flat: Array<u8, _> = Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let unflat2: Array<Array<u8, _>, _> = Array([
            Array([1, 2]),
            Array([3, 4]),
            Array([5, 6]),
            Array([7, 8]),
            Array([9, 10]),
        ]);
        let unflat5: Array<Array<u8, _>, _> =
            Array([Array([1, 2, 3, 4, 5]), Array([6, 7, 8, 9, 10])]);

        // Flatten
        let actual = unflat2.flatten();
        assert_eq!(flat, actual);

        let actual = unflat5.flatten();
        assert_eq!(flat, actual);

        // Unflatten
        let actual: Array<Array<u8, U2>, U5> = flat.unflatten();
        assert_eq!(unflat2, actual);

        let actual: Array<Array<u8, U5>, U2> = flat.unflatten();
        assert_eq!(unflat5, actual);

        // Unflatten on references
        let actual: Array<&Array<u8, U2>, U5> = (&flat).unflatten();
        for (i, part) in actual.iter().enumerate() {
            assert_eq!(&unflat2[i], *part);
        }

        let actual: Array<&Array<u8, U5>, U2> = (&flat).unflatten();
        for (i, part) in actual.iter().enumerate() {
            assert_eq!(&unflat5[i], *part);
        }
    }
}
