use std::ops::Range;

pub trait SliceChecked {
    fn slice_checked(&self, range: Range<usize>) -> Option<&Self>;
}

impl<T> SliceChecked for [T] {
    fn slice_checked(&self, range: Range<usize>) -> Option<&Self> {
        if range.end <= self.len() {
            Some(&self[range])
        } else {
            None
        }
    }
}

pub trait SubsliceOffset {
    fn subslice_offset(&self, inner: &Self) -> Option<usize>;
}

pub trait SubsliceRange {
    fn subslice_in_range(&self, inner: &Self) -> Option<Range<usize>>;
}

impl<T> SubsliceOffset for [T] {
    fn subslice_offset(&self, inner: &Self) -> Option<usize> {
        let start = self.as_ptr() as usize;
        let inner_start = inner.as_ptr() as usize;
        if inner_start < start || inner_start > start.wrapping_add(self.len()) {
            None
        } else {
            inner_start.checked_sub(start)
        }
    }
}

impl<T> SubsliceRange for [T]
where
    [T]: SubsliceOffset,
{
    fn subslice_in_range(&self, inner: &Self) -> Option<Range<usize>> {
        let offset = self.subslice_offset(inner)?;
        let end = offset.checked_add(inner.len())?;
        let start = self.as_ptr() as usize;
        if end > start + self.len() {
            None
        } else {
            Some(Range { start: offset, end })
        }
    }
}
