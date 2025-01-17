use std::cmp::min;
use std::fmt::{self, Debug};

use super::plumbing::*;
use super::*;
use crate::math::div_round_up;

/// `FoldChunks` is an iterator that groups elements of an underlying iterator and applies a
/// function over them, producing a single value for each group.
///
/// This struct is created by the [`fold_chunks()`] method on [`IndexedParallelIterator`]
///
/// [`fold_chunks()`]: trait.IndexedParallelIterator.html#method.fold_chunks
/// [`IndexedParallelIterator`]: trait.IndexedParallelIterator.html
#[must_use = "iterator adaptors are lazy and do nothing unless consumed"]
#[derive(Clone)]
pub struct FoldChunks<I, ID, F>
where
    I: IndexedParallelIterator,
{
    base: I,
    chunk_size: usize,
    fold_op: F,
    identity: ID,
}

impl<I: IndexedParallelIterator + Debug, ID, F> Debug for FoldChunks<I, ID, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fold")
            .field("base", &self.base)
            .field("chunk_size", &self.chunk_size)
            .finish()
    }
}

impl<I, ID, U, F> FoldChunks<I, ID, F>
where
    I: IndexedParallelIterator,
    ID: Fn() -> U + Send + Sync,
    F: Fn(U, I::Item) -> U + Send + Sync,
    U: Send,
{
    /// Creates a new `FoldChunks` iterator
    pub(super) fn new(base: I, chunk_size: usize, identity: ID, fold_op: F) -> Self {
        FoldChunks {
            base,
            chunk_size,
            identity,
            fold_op,
        }
    }
}

impl<I, ID, U, F> ParallelIterator for FoldChunks<I, ID, F>
where
    I: IndexedParallelIterator,
    ID: Fn() -> U + Send + Sync,
    F: Fn(U, I::Item) -> U + Send + Sync,
    U: Send,
{
    type Item = U;

    fn drive_unindexed<C>(self, consumer: C) -> C::Result
    where
        C: Consumer<U>,
    {
        bridge(self, consumer)
    }

    fn opt_len(&self) -> Option<usize> {
        Some(self.len())
    }
}

impl<I, ID, U, F> IndexedParallelIterator for FoldChunks<I, ID, F>
where
    I: IndexedParallelIterator,
    ID: Fn() -> U + Send + Sync,
    F: Fn(U, I::Item) -> U + Send + Sync,
    U: Send,
{
    fn len(&self) -> usize {
        div_round_up(self.base.len(), self.chunk_size)
    }

    fn drive<C>(self, consumer: C) -> C::Result
    where
        C: Consumer<Self::Item>,
    {
        bridge(self, consumer)
    }

    fn with_producer<CB>(self, callback: CB) -> CB::Output
    where
        CB: ProducerCallback<Self::Item>,
    {
        let len = self.base.len();
        return self.base.with_producer(Callback {
            chunk_size: self.chunk_size,
            len,
            identity: self.identity,
            fold_op: self.fold_op,
            callback,
        });

        struct Callback<CB, ID, F> {
            chunk_size: usize,
            len: usize,
            identity: ID,
            fold_op: F,
            callback: CB,
        }

        impl<T, CB, ID, U, F> ProducerCallback<T> for Callback<CB, ID, F>
        where
            CB: ProducerCallback<U>,
            ID: Fn() -> U + Send + Sync,
            F: Fn(U, T) -> U + Send + Sync,
        {
            type Output = CB::Output;

            fn callback<P>(self, base: P) -> CB::Output
            where
                P: Producer<Item = T>,
            {
                self.callback.callback(FoldChunksProducer {
                    chunk_size: self.chunk_size,
                    len: self.len,
                    identity: &self.identity,
                    fold_op: &self.fold_op,
                    base,
                })
            }
        }
    }
}

struct FoldChunksProducer<'f, P, ID, F>
where
    P: Producer,
{
    chunk_size: usize,
    len: usize,
    identity: &'f ID,
    fold_op: &'f F,
    base: P,
}

impl<'f, P, ID, U, F> Producer for FoldChunksProducer<'f, P, ID, F>
where
    P: Producer,
    ID: Fn() -> U + Send + Sync,
    F: Fn(U, P::Item) -> U + Send + Sync,
{
    type Item = F::Output;
    type IntoIter = FoldChunksSeq<'f, P, ID, F>;

    fn into_iter(self) -> Self::IntoIter {
        FoldChunksSeq {
            chunk_size: self.chunk_size,
            len: self.len,
            identity: self.identity,
            fold_op: self.fold_op,
            inner: if self.len > 0 { Some(self.base) } else { None },
        }
    }

    fn min_len(&self) -> usize {
        div_round_up(self.base.min_len(), self.chunk_size)
    }

    fn max_len(&self) -> usize {
        self.base.max_len() / self.chunk_size
    }

    fn split_at(self, index: usize) -> (Self, Self) {
        let elem_index = min(index * self.chunk_size, self.len);
        let (left, right) = self.base.split_at(elem_index);
        (
            FoldChunksProducer {
                chunk_size: self.chunk_size,
                len: elem_index,
                identity: self.identity,
                fold_op: self.fold_op,
                base: left,
            },
            FoldChunksProducer {
                chunk_size: self.chunk_size,
                len: self.len - elem_index,
                identity: self.identity,
                fold_op: self.fold_op,
                base: right,
            },
        )
    }
}

struct FoldChunksSeq<'f, P, ID, F> {
    chunk_size: usize,
    len: usize,
    identity: &'f ID,
    fold_op: &'f F,
    inner: Option<P>,
}

impl<'f, P, ID, U, F> Iterator for FoldChunksSeq<'f, P, ID, F>
where
    P: Producer,
    ID: Fn() -> U + Send + Sync,
    F: Fn(U, P::Item) -> U + Send + Sync,
{
    type Item = U;

    fn next(&mut self) -> Option<Self::Item> {
        let producer = self.inner.take()?;
        if self.len > self.chunk_size {
            let (left, right) = producer.split_at(self.chunk_size);
            self.inner = Some(right);
            self.len -= self.chunk_size;
            let chunk = left.into_iter();
            Some(chunk.fold((self.identity)(), self.fold_op))
        } else {
            debug_assert!(self.len > 0);
            self.len = 0;
            let chunk = producer.into_iter();
            Some(chunk.fold((self.identity)(), self.fold_op))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl<'f, P, ID, U, F> ExactSizeIterator for FoldChunksSeq<'f, P, ID, F>
where
    P: Producer,
    ID: Fn() -> U + Send + Sync,
    F: Fn(U, P::Item) -> U + Send + Sync,
{
    #[inline]
    fn len(&self) -> usize {
        div_round_up(self.len, self.chunk_size)
    }
}

impl<'f, P, ID, U, F> DoubleEndedIterator for FoldChunksSeq<'f, P, ID, F>
where
    P: Producer,
    ID: Fn() -> U + Send + Sync,
    F: Fn(U, P::Item) -> U + Send + Sync,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        let producer = self.inner.take()?;
        if self.len > self.chunk_size {
            let mut size = self.len % self.chunk_size;
            if size == 0 {
                size = self.chunk_size;
            }
            let (left, right) = producer.split_at(self.len - size);
            self.inner = Some(left);
            self.len -= size;
            let chunk = right.into_iter();
            Some(chunk.fold((self.identity)(), self.fold_op))
        } else {
            debug_assert!(self.len > 0);
            self.len = 0;
            let chunk = producer.into_iter();
            Some(chunk.fold((self.identity)(), self.fold_op))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::ops::Add;

    #[test]
    fn check_fold_chunks() {
        let words = "bishbashbosh!"
            .chars()
            .collect::<Vec<_>>()
            .into_par_iter()
            .fold_chunks(4, String::new, |mut s, c| {
                s.push(c);
                s
            })
            .collect::<Vec<_>>();

        assert_eq!(words, vec!["bish", "bash", "bosh", "!"]);
    }

    // 'closure' values for tests below
    fn id() -> i32 {
        0
    }
    fn sum<T, U>(x: T, y: U) -> T
    where
        T: Add<U, Output = T>,
    {
        x + y
    }

    #[test]
    #[should_panic(expected = "chunk_size must not be zero")]
    fn check_fold_chunks_zero_size() {
        let _: Vec<i32> = vec![1, 2, 3]
            .into_par_iter()
            .fold_chunks(0, id, sum)
            .collect();
    }

    #[test]
    fn check_fold_chunks_even_size() {
        assert_eq!(
            vec![1 + 2 + 3, 4 + 5 + 6, 7 + 8 + 9],
            (1..10)
                .into_par_iter()
                .fold_chunks(3, id, sum)
                .collect::<Vec<i32>>()
        );
    }

    #[test]
    fn check_fold_chunks_empty() {
        let v: Vec<i32> = vec![];
        let expected: Vec<i32> = vec![];
        assert_eq!(
            expected,
            v.into_par_iter()
                .fold_chunks(2, id, sum)
                .collect::<Vec<i32>>()
        );
    }

    #[test]
    fn check_fold_chunks_len() {
        assert_eq!(4, (0..8).into_par_iter().fold_chunks(2, id, sum).len());
        assert_eq!(3, (0..9).into_par_iter().fold_chunks(3, id, sum).len());
        assert_eq!(3, (0..8).into_par_iter().fold_chunks(3, id, sum).len());
        assert_eq!(1, (&[1]).par_iter().fold_chunks(3, id, sum).len());
        assert_eq!(0, (0..0).into_par_iter().fold_chunks(3, id, sum).len());
    }

    #[test]
    fn check_fold_chunks_uneven() {
        let cases: Vec<(Vec<u32>, usize, Vec<u32>)> = vec![
            ((0..5).collect(), 3, vec![0 + 1 + 2, 3 + 4]),
            (vec![1], 5, vec![1]),
            ((0..4).collect(), 3, vec![0 + 1 + 2, 3]),
        ];

        for (i, (v, n, expected)) in cases.into_iter().enumerate() {
            let mut res: Vec<u32> = vec![];
            v.par_iter()
                .fold_chunks(n, || 0, sum)
                .collect_into_vec(&mut res);
            assert_eq!(expected, res, "Case {} failed", i);

            res.truncate(0);
            v.into_par_iter()
                .fold_chunks(n, || 0, sum)
                .rev()
                .collect_into_vec(&mut res);
            assert_eq!(
                expected.into_iter().rev().collect::<Vec<u32>>(),
                res,
                "Case {} reversed failed",
                i
            );
        }
    }
}
