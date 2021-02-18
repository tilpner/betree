use std::{
    cmp::{self, Ordering},
    ops::Index,
};

#[derive(Debug, Clone, Copy)]
pub struct SegmentedSlice<'s, T>(&'s [&'s [T]]);

impl<'s, T: Clone> SegmentedSlice<'s, T> {
    pub fn total_len(&self) -> usize {
        self.0.iter().copied().map(<[T]>::len).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter().copied().flat_map(<[T]>::iter)
    }

    pub fn to_vec(&self) -> Vec<T> {
        let mut v = Vec::with_capacity(self.total_len());

        for slice in self.0.iter() {
            v.extend_from_slice(slice);
        }

        v
    }
}

impl<'s, T> From<&'s [&'s [T]]> for SegmentedSlice<'s, T> {
    fn from(s: &'s [&'s [T]]) -> Self {
        Self(s)
    }
}

impl<'s, T: Clone> Index<usize> for SegmentedSlice<'s, T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.iter().nth(index).unwrap()
    }
}

impl<'s, T: Clone + PartialEq> PartialEq<[T]> for SegmentedSlice<'s, T> {
    fn eq(&self, mut other: &[T]) -> bool {
        for &slice in self.0.iter() {
            let (curr, rest) = other.split_at(slice.len());
            if slice != curr {
                return false;
            }
            other = rest;
        }

        true
    }
}

impl<'s, T: Clone + PartialOrd> PartialOrd<[T]> for SegmentedSlice<'s, T> {
    fn partial_cmp(&self, other: &[T]) -> Option<Ordering> {
        let self_len = self.total_len();
        let len = cmp::min(self_len, other.len());

        for &slice in self.0.iter() {
            let (curr, rest) = other.split_at(slice.len());

            match slice.partial_cmp(curr) {
                Some(Ordering::Equal) => (),
                non_eq => return non_eq,
            }
        }

        self_len.partial_cmp(&other.len())
    }
}

#[cfg(test)]
mod test {
    use super::SegmentedSlice;

    #[test]
    fn test_segmented_slice() {
        let k = b"foo";
        let s = &[&[0][..], &k[..], &[1, 2, 3, 4]][..];
        let s = SegmentedSlice::from(s);

        assert_eq!(s.total_len(), 1 + 3 + 4);
        assert_eq!(
            SegmentedSlice::to_vec(&s),
            vec![0, b'f', b'o', b'o', 1, 2, 3, 4]
        );
    }
}
