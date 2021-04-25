use core::cmp;

use crate::{
    cow::CowBytes,
    memmem::{
        prefilter::{Freqy, Prefilter, PrefilterState},
        rabinkarp, util,
    },
};

/// Two-Way search in the forward direction.
#[derive(Clone, Debug)]
pub(crate) struct Forward<'n>(TwoWay<'n>);

/// Two-Way search in the reverse direction.
#[derive(Clone, Debug)]
pub(crate) struct Reverse<'n>(TwoWay<'n>);

/// The configuration to use for Two-Way. A configuration alters the behavior
/// of Two-Way in a way that callers can control. Currently, "altering the
/// behavior" only refers to changing its performance characteristics. (But
/// never its time/space complexity.)
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Config {
    /// This permits changing the behavior of the prefilter, since it can have
    /// a variable impact on performance.
    pub(crate) prefilter: Prefilter,
}

/// An implementation of the TwoWay substring search algorithm, with heuristics
/// for accelerating search based on frequency analysis.
///
/// This searcher supports forward and reverse search, although not
/// simultaneously. It runs in O(n + m) time and O(1) space, where
/// `n ~ len(needle)` and `m ~ len(haystack)`.
///
/// The implementation here roughly matches that which was developed by
/// Crochemore and Perrin in their 1991 paper "Two-way string-matching." The
/// changes in this implementation are 1) the use of zero-based indices, 2) a
/// heuristic skip table based on the last byte (borrowed from Rust's standard
/// library) and 3) the addition of heuristics for a fast skip loop. That is,
/// (3) this will detect bytes that are believed to be rare in the needle and
/// use fast vectorized instructions to find their occurrences quickly. The
/// Two-Way algorithm is then used to confirm whether a match at that location
/// occurred.
///
/// The heuristic for fast skipping is automatically shut off if it's
/// detected to be ineffective at search time. Generally, this only occurs in
/// pathological cases. But this is generally necessary in order to preserve
/// a `O(n + m)` time bound.
///
/// The code below is fairly complex and not obviously correct at all. It's
/// likely necessary to read the Two-Way paper cited above in order to fully
/// grok this code. The essence of it is:
///
/// 1) Do something to detect a "critical" position in the needle.
/// 2) For the current position in the haystack, look if needle[critical..]
///    matches at that position.
/// 3) If so, look if needle[..critical] matches.
/// 4) If a mismatch occurs, shift the search by some amount based on the
///    critical position and a pre-computed shift.
///
/// This type is wrapped in Forward and Reverse types that expose consistent
/// forward or reverse APIs.
#[derive(Clone, Debug)]
struct TwoWay<'n> {
    /// The needle that we're looking for.
    needle: CowBytes<'n>,
    /// An implementation of a fast skip loop based on hard-coded frequency
    /// data. This is only used when conditions are deemed favorable. That is,
    /// the number of skipped bytes is tracked, and if it's too low after a
    /// certain number of skips, the prefilter is disabled.
    ///
    /// While it's a heuristic, the fact that we disable it is important for
    /// holding up our time complexity guarantee. Since without a heuristic
    /// to disable the prefilter, it's possible for pathological inputs to
    /// cause worst case multiplicative behavior (in the size of the needle and
    /// haystack).
    pre: Freqy,
    /// A small bitset used as a quick prefilter (in addition to the faster
    /// SIMD based prefilter). Namely, a bit 'i' is set if and only if b%64==i
    /// for any b in the needle.
    ///
    /// When used as a prefilter, if the last byte at the current candidate
    /// position is NOT in this set, then we can skip that entire candidate
    /// position (the length of the needle). This is essentially the shift
    /// trick found in Boyer-Moore, but only applied to bytes that don't appear
    /// in the needle.
    ///
    /// N.B. This trick was inspired by something similar in std's
    /// implementation of Two-Way.
    byteset: ApproximateByteSet,
    /// A critical position in needle. Specifically, this position corresponds
    /// to beginning of either the minimal or maximal suffix in needle. (N.B.
    /// See SuffixType below for why "minimal" isn't quite the correct word
    /// here.)
    ///
    /// This is the position at which every search begins. Namely, search
    /// starts by scanning text to the right of this position, and only if
    /// there's a match does the text to the left of this position get scanned.
    critical_pos: usize,
    /// The amount we shift by in the Two-Way search algorithm. This
    /// corresponds to the "small period" and "large period" cases.
    shift: Shift,
}

impl<'n> Forward<'n> {
    /// Create a searcher that uses the Two-Way algorithm by searching forwards
    /// through any haystack.
    pub(crate) fn new(config: Config, needle: &'n [u8]) -> Forward<'n> {
        if needle.is_empty() {
            return Forward(TwoWay::empty());
        }

        let pre = Freqy::forward(&config.prefilter, needle);
        let byteset = ApproximateByteSet::new(needle);
        let min_suffix = Suffix::forward(needle, SuffixKind::Minimal);
        let max_suffix = Suffix::forward(needle, SuffixKind::Maximal);
        let (period_lower_bound, critical_pos) =
            if min_suffix.pos > max_suffix.pos {
                (min_suffix.period, min_suffix.pos)
            } else {
                (max_suffix.period, max_suffix.pos)
            };
        let shift = Shift::forward(needle, period_lower_bound, critical_pos);
        let needle = CowBytes::new(needle);
        Forward(TwoWay { needle, pre, byteset, critical_pos, shift })
    }

    /// Return a fresh prefilter state that can be used with this searcher.
    /// A prefilter state is used to track the effectiveness of a searcher's
    /// prefilter for speeding up searches. Therefore, the prefilter state
    /// should generally be reused on subsequent searches (such as in an
    /// iterator). For searches on a different haystack, then a new prefilter
    /// state should be used.
    ///
    /// This always initializes a valid prefilter state even if this searcher
    /// does not have a prefilter enabled.
    #[inline(always)]
    pub(crate) fn prefilter_state(&self) -> PrefilterState {
        self.0.pre.state()
    }

    /// Return the needle used by this searcher.
    #[inline(always)]
    pub(crate) fn needle(&self) -> &[u8] {
        self.0.needle.as_slice()
    }

    /// Convert this searcher into a borrowed version, where the needle is
    /// borrowed.
    #[inline(always)]
    pub(crate) fn as_ref(&self) -> Forward<'_> {
        Forward(self.0.as_ref())
    }

    /// Convert this searcher into an owned version, where the needle is
    /// copied if it isn't already owned.
    #[cfg(feature = "std")]
    #[inline(always)]
    pub(crate) fn into_owned(self) -> Forward<'static> {
        Forward(self.0.into_owned())
    }

    /// Find the position of the first occurrence of this searcher's needle in
    /// the given haystack. If one does not exist, then return None.
    ///
    /// This will automatically initialize prefilter state. This should only
    /// be used for one-off searches.
    pub(crate) fn find(&self, haystack: &[u8]) -> Option<usize> {
        self.find_with(&mut self.prefilter_state(), haystack)
    }

    /// Find the position of the first occurrence of this searcher's needle in
    /// the given haystack. If one does not exist, then return None.
    ///
    /// This accepts prefilter state that is useful when using the same
    /// searcher multiple times, such as in an iterator.
    #[inline(always)]
    pub(crate) fn find_with(
        &self,
        prestate: &mut PrefilterState,
        haystack: &[u8],
    ) -> Option<usize> {
        let needle = self.0.needle.as_slice();
        if needle.len() <= 1 {
            if needle.is_empty() {
                // The empty needle matches at all positions.
                Some(0)
            } else {
                // We special case a single-byte needle to make things a bit
                // faster. Our prefilter infrastructure could be made to
                // handle it, but doing it this way has less overhead. This is
                // particularly impactful for common bytes.
                crate::memchr(needle[0], haystack)
            }
        } else if haystack.len() < needle.len() {
            None
        } else if haystack.len() < min_prefilter_len(haystack, needle) {
            rabinkarp::find_with(&self.0.pre.ninfo.nhash, haystack, needle)
        } else {
            self.find_with_imp(prestate, haystack, needle)
        }
    }

    #[inline(never)]
    fn find_with_imp(
        &self,
        prestate: &mut PrefilterState,
        haystack: &[u8],
        needle: &[u8],
    ) -> Option<usize> {
        match self.0.shift {
            Shift::Small { period } => {
                if should_prefilter(prestate, haystack, needle) {
                    self.find_small_imp(
                        prestate, true, haystack, needle, period,
                    )
                } else {
                    self.find_small_imp(
                        prestate, false, haystack, needle, period,
                    )
                }
            }
            Shift::Large { shift } => {
                if should_prefilter(prestate, haystack, needle) {
                    self.find_large_imp(
                        prestate, true, haystack, needle, shift,
                    )
                } else {
                    self.find_large_imp(
                        prestate, false, haystack, needle, shift,
                    )
                }
            }
        }
    }

    // Each of the two search implementations below can be accelerated by a
    // prefilter, but it is not always enabled. To avoid its overhead when
    // its disabled, we explicitly inline each search implementation based on
    // whether a prefilter will be used or not.

    #[inline(always)]
    fn find_small_imp(
        &self,
        prestate: &mut PrefilterState,
        prefilter: bool,
        haystack: &[u8],
        needle: &[u8],
        period: usize,
    ) -> Option<usize> {
        let last_byte = needle.len() - 1;
        let mut pos = 0;
        let mut shift = 0;
        while pos + needle.len() <= haystack.len() {
            let mut i = cmp::max(self.0.critical_pos, shift);
            if prefilter && prestate.is_effective() {
                pos += self.0.pre.find(prestate, &haystack[pos..], needle)?;
                shift = 0;
                i = self.0.critical_pos;
                if pos + needle.len() > haystack.len() {
                    return None;
                }
            }
            if !self.0.byteset.contains(haystack[pos + last_byte]) {
                pos += needle.len();
                shift = 0;
                continue;
            }
            while i < needle.len() && needle[i] == haystack[pos + i] {
                i += 1;
            }
            if i < needle.len() {
                pos += i - self.0.critical_pos + 1;
                shift = 0;
            } else {
                let mut j = self.0.critical_pos;
                while j > shift && needle[j] == haystack[pos + j] {
                    j -= 1;
                }
                if j <= shift && needle[shift] == haystack[pos + shift] {
                    return Some(pos);
                }
                pos += period;
                shift = needle.len() - period;
            }
        }
        None
    }

    #[inline(always)]
    fn find_large_imp(
        &self,
        prestate: &mut PrefilterState,
        prefilter: bool,
        haystack: &[u8],
        needle: &[u8],
        shift: usize,
    ) -> Option<usize> {
        let last_byte = needle.len() - 1;
        let mut pos = 0;
        'outer: while pos + needle.len() <= haystack.len() {
            if prefilter && prestate.is_effective() {
                pos += self.0.pre.find(prestate, &haystack[pos..], needle)?;
                if pos + needle.len() > haystack.len() {
                    return None;
                }
            }

            if !self.0.byteset.contains(haystack[pos + last_byte]) {
                pos += needle.len();
                continue;
            }
            let mut i = self.0.critical_pos;
            while i < needle.len() && needle[i] == haystack[pos + i] {
                i += 1;
            }
            if i < needle.len() {
                pos += i - self.0.critical_pos + 1;
            } else {
                for j in (0..self.0.critical_pos).rev() {
                    if needle[j] != haystack[pos + j] {
                        pos += shift;
                        continue 'outer;
                    }
                }
                return Some(pos);
            }
        }
        None
    }
}

impl<'n> Reverse<'n> {
    /// Create a searcher that uses the Two-Way algorithm by searching in
    /// reverse through any haystack.
    pub(crate) fn new(needle: &'n [u8]) -> Reverse<'n> {
        if needle.is_empty() {
            return Reverse(TwoWay::empty());
        }

        let pre = Freqy::inert();
        let byteset = ApproximateByteSet::new(needle);
        let min_suffix = Suffix::reverse(needle, SuffixKind::Minimal);
        let max_suffix = Suffix::reverse(needle, SuffixKind::Maximal);
        let (period_lower_bound, critical_pos) =
            if min_suffix.pos < max_suffix.pos {
                (min_suffix.period, min_suffix.pos)
            } else {
                (max_suffix.period, max_suffix.pos)
            };
        let shift = Shift::reverse(needle, period_lower_bound, critical_pos);
        let needle = CowBytes::new(needle);
        Reverse(TwoWay { needle, pre, byteset, critical_pos, shift })
    }

    /// Return the needle used by this searcher.
    #[inline(always)]
    pub(crate) fn needle(&self) -> &[u8] {
        self.0.needle.as_slice()
    }

    /// Convert this searcher into a borrow version, where the needle is
    /// borrowed.
    #[inline(always)]
    pub(crate) fn as_ref(&self) -> Reverse<'_> {
        Reverse(self.0.as_ref())
    }

    /// Convert this searcher into an owned version, where the needle is
    /// copied if it isn't already owned.
    #[cfg(feature = "std")]
    #[inline(always)]
    pub(crate) fn into_owned(self) -> Reverse<'static> {
        Reverse(self.0.into_owned())
    }

    /// Find the position of the last occurrence of this searcher's needle
    /// in the given haystack. If one does not exist, then return None.
    ///
    /// This will automatically initialize prefilter state. This should only
    /// be used for one-off searches.
    pub(crate) fn rfind(&self, haystack: &[u8]) -> Option<usize> {
        // For the reverse case, we don't use a prefilter. It's plausible that
        // perhaps we should, but it's a lot of additional code to do it, and
        // it's not clear that it's actually worth it. If you have a really
        // compelling use case for this, please file an issue.

        let needle = self.0.needle.as_slice();
        if needle.is_empty() {
            return Some(haystack.len());
        } else if haystack.len() < needle.len() {
            return None;
        } else if needle.len() == 1 {
            // See the forward case above for why we special single-byte
            // needles instead of making our prefilter infrastructure handle
            // it.
            return crate::memrchr(needle[0], haystack);
        }
        match self.0.shift {
            Shift::Small { period } => {
                self.rfind_small_imp(haystack, needle, period)
            }
            Shift::Large { shift } => {
                self.rfind_large_imp(haystack, needle, shift)
            }
        }
    }

    #[inline(always)]
    fn rfind_small_imp(
        &self,
        haystack: &[u8],
        needle: &[u8],
        period: usize,
    ) -> Option<usize> {
        let nlen = needle.len();
        let mut pos = haystack.len();
        let mut shift = nlen;
        while pos >= nlen {
            if !self.0.byteset.contains(haystack[pos - nlen]) {
                pos -= nlen;
                shift = nlen;
                continue;
            }
            let mut i = cmp::min(self.0.critical_pos, shift);
            while i > 0 && needle[i - 1] == haystack[pos - nlen + i - 1] {
                i -= 1;
            }
            if i > 0 || needle[0] != haystack[pos - nlen] {
                pos -= self.0.critical_pos - i + 1;
                shift = nlen;
            } else {
                let mut j = self.0.critical_pos;
                while j < shift && needle[j] == haystack[pos - nlen + j] {
                    j += 1;
                }
                if j == shift {
                    return Some(pos - nlen);
                }
                pos -= period;
                shift = period;
            }
        }
        None
    }

    #[inline(always)]
    fn rfind_large_imp(
        &self,
        haystack: &[u8],
        needle: &[u8],
        shift: usize,
    ) -> Option<usize> {
        let nlen = needle.len();
        let mut pos = haystack.len();
        while pos >= nlen {
            if !self.0.byteset.contains(haystack[pos - nlen]) {
                pos -= nlen;
                continue;
            }
            let mut i = self.0.critical_pos;
            while i > 0 && needle[i - 1] == haystack[pos - nlen + i - 1] {
                i -= 1;
            }
            if i > 0 || needle[0] != haystack[pos - nlen] {
                pos -= self.0.critical_pos - i + 1;
            } else {
                let mut j = self.0.critical_pos;
                while j < nlen && needle[j] == haystack[pos - nlen + j] {
                    j += 1;
                }
                if j == nlen {
                    return Some(pos - nlen);
                }
                pos -= shift;
            }
        }
        None
    }
}

impl<'n> TwoWay<'n> {
    #[inline(always)]
    fn empty() -> TwoWay<'static> {
        TwoWay {
            needle: CowBytes::new(b""),
            pre: Freqy::inert(),
            byteset: ApproximateByteSet::new(b""),
            critical_pos: 0,
            shift: Shift::Large { shift: 0 },
        }
    }

    #[inline(always)]
    fn as_ref(&self) -> TwoWay<'_> {
        TwoWay {
            needle: CowBytes::new(self.needle.as_slice()),
            pre: self.pre.clone(),
            byteset: self.byteset,
            critical_pos: self.critical_pos,
            shift: self.shift.clone(),
        }
    }

    #[cfg(feature = "std")]
    #[inline(always)]
    fn into_owned(self) -> TwoWay<'static> {
        TwoWay {
            needle: self.needle.into_owned(),
            pre: self.pre.clone(),
            byteset: self.byteset,
            critical_pos: self.critical_pos,
            shift: self.shift,
        }
    }
}

/// A representation of the amount we're allowed to shift by during Two-Way
/// search.
///
/// When computing a critical factorization of the needle, we find the position
/// of the critical factorization by finding the needle's maximal (or minimal)
/// suffix, along with the period of that suffix. It turns out that the period
/// of that suffix is a lower bound on the period of the needle itself.
///
/// This lower bound is equivalent to the actual period of the needle in
/// some cases. To describe that case, we denote the needle as `x` where
/// `x = uv` and `v` is the lexicographic maximal suffix of `v`. The lower
/// bound given here is always the period of `v`, which is `<= period(x)`. The
/// case where `period(v) == period(x)` occurs when `len(u) < (len(x) / 2)` and
/// where `u` is a suffix of `v[0..period(v)]`.
///
/// This case is important because the search algorithm for when the
/// periods are equivalent is slightly different than the search algorithm
/// for when the periods are not equivalent. In particular, when they aren't
/// equivalent, we know that the period of the needle is no less than half its
/// length. In this case, we shift by an amount less than or equal to the
/// period of the needle (determined by the maximum length of the components
/// of the critical factorization of `x`, i.e., `max(len(u), len(v))`)..
///
/// The above two cases are represented by the variants below. Each entails
/// a different instantiation of the Two-Way search algorithm.
///
/// N.B. If we could find a way to compute the exact period in all cases,
/// then we could collapse this case analysis and simplify the algorithm. The
/// Two-Way paper suggests this is possible, but more reading is required to
/// grok why the authors didn't pursue that path.
#[derive(Clone, Debug)]
enum Shift {
    Small { period: usize },
    Large { shift: usize },
}

impl Shift {
    /// Compute the shift for a given needle in the forward direction.
    ///
    /// This requires a lower bound on the period and a critical position.
    /// These can be computed by extracting both the minimal and maximal
    /// lexicographic suffixes, and choosing the right-most starting position.
    /// The lower bound on the period is then the period of the chosen suffix.
    fn forward(
        needle: &[u8],
        period_lower_bound: usize,
        critical_pos: usize,
    ) -> Shift {
        let large = cmp::max(critical_pos, needle.len() - critical_pos);
        if critical_pos * 2 >= needle.len() {
            return Shift::Large { shift: large };
        }

        let (u, v) = needle.split_at(critical_pos);
        if !util::is_suffix(&v[..period_lower_bound], u) {
            return Shift::Large { shift: large };
        }
        Shift::Small { period: period_lower_bound }
    }

    /// Compute the shift for a given needle in the reverse direction.
    ///
    /// This requires a lower bound on the period and a critical position.
    /// These can be computed by extracting both the minimal and maximal
    /// lexicographic suffixes, and choosing the left-most starting position.
    /// The lower bound on the period is then the period of the chosen suffix.
    fn reverse(
        needle: &[u8],
        period_lower_bound: usize,
        critical_pos: usize,
    ) -> Shift {
        let large = cmp::max(critical_pos, needle.len() - critical_pos);
        if (needle.len() - critical_pos) * 2 >= needle.len() {
            return Shift::Large { shift: large };
        }

        let (v, u) = needle.split_at(critical_pos);
        if !util::is_prefix(&v[v.len() - period_lower_bound..], u) {
            return Shift::Large { shift: large };
        }
        Shift::Small { period: period_lower_bound }
    }
}

/// A suffix extracted from a needle along with its period.
#[derive(Debug)]
struct Suffix {
    /// The starting position of this suffix.
    ///
    /// If this is a forward suffix, then `&bytes[pos..]` can be used. If this
    /// is a reverse suffix, then `&bytes[..pos]` can be used. That is, for
    /// forward suffixes, this is an inclusive starting position, where as for
    /// reverse suffixes, this is an exclusive ending position.
    pos: usize,
    /// The period of this suffix.
    ///
    /// Note that this is NOT necessarily the period of the string from which
    /// this suffix comes from. (It is always less than or equal to the period
    /// of the original string.)
    period: usize,
}

impl Suffix {
    fn forward(needle: &[u8], kind: SuffixKind) -> Suffix {
        debug_assert!(!needle.is_empty());

        // suffix represents our maximal (or minimal) suffix, along with
        // its period.
        let mut suffix = Suffix { pos: 0, period: 1 };
        // The start of a suffix in `needle` that we are considering as a
        // more maximal (or minimal) suffix than what's in `suffix`.
        let mut candidate_start = 1;
        // The current offset of our suffixes that we're comparing.
        //
        // When the characters at this offset are the same, then we mush on
        // to the next position since no decision is possible. When the
        // candidate's character is greater (or lesser) than the corresponding
        // character than our current maximal (or minimal) suffix, then the
        // current suffix is changed over to the candidate and we restart our
        // search. Otherwise, the candidate suffix is no good and we restart
        // our search on the next candidate.
        //
        // The three cases above correspond to the three cases in the loop
        // below.
        let mut offset = 0;

        while candidate_start + offset < needle.len() {
            let current = needle[suffix.pos + offset];
            let candidate = needle[candidate_start + offset];
            match kind.cmp(current, candidate) {
                SuffixOrdering::Accept => {
                    suffix = Suffix { pos: candidate_start, period: 1 };
                    candidate_start += 1;
                    offset = 0;
                }
                SuffixOrdering::Skip => {
                    candidate_start += offset + 1;
                    offset = 0;
                    suffix.period = candidate_start - suffix.pos;
                }
                SuffixOrdering::Push => {
                    if offset + 1 == suffix.period {
                        candidate_start += suffix.period;
                        offset = 0;
                    } else {
                        offset += 1;
                    }
                }
            }
        }
        suffix
    }

    fn reverse(needle: &[u8], kind: SuffixKind) -> Suffix {
        debug_assert!(!needle.is_empty());

        // See the comments in `forward` for how this works.
        let mut suffix = Suffix { pos: needle.len(), period: 1 };
        if needle.len() == 1 {
            return suffix;
        }
        let mut candidate_start = needle.len() - 1;
        let mut offset = 0;

        while offset < candidate_start {
            let current = needle[suffix.pos - offset - 1];
            let candidate = needle[candidate_start - offset - 1];
            match kind.cmp(current, candidate) {
                SuffixOrdering::Accept => {
                    suffix = Suffix { pos: candidate_start, period: 1 };
                    candidate_start -= 1;
                    offset = 0;
                }
                SuffixOrdering::Skip => {
                    candidate_start -= offset + 1;
                    offset = 0;
                    suffix.period = suffix.pos - candidate_start;
                }
                SuffixOrdering::Push => {
                    if offset + 1 == suffix.period {
                        candidate_start -= suffix.period;
                        offset = 0;
                    } else {
                        offset += 1;
                    }
                }
            }
        }
        suffix
    }
}

/// The kind of suffix to extract.
#[derive(Clone, Copy, Debug)]
enum SuffixKind {
    /// Extract the smallest lexicographic suffix from a string.
    ///
    /// Technically, this doesn't actually pick the smallest lexicographic
    /// suffix. e.g., Given the choice between `a` and `aa`, this will choose
    /// the latter over the former, even though `a < aa`. The reasoning for
    /// this isn't clear from the paper, but it still smells like a minimal
    /// suffix.
    Minimal,
    /// Extract the largest lexicographic suffix from a string.
    ///
    /// Unlike `Minimal`, this really does pick the maximum suffix. e.g., Given
    /// the choice between `z` and `zz`, this will choose the latter over the
    /// former.
    Maximal,
}

/// The result of comparing corresponding bytes between two suffixes.
#[derive(Clone, Copy, Debug)]
enum SuffixOrdering {
    /// This occurs when the given candidate byte indicates that the candidate
    /// suffix is better than the current maximal (or minimal) suffix. That is,
    /// the current candidate suffix should supplant the current maximal (or
    /// minimal) suffix.
    Accept,
    /// This occurs when the given candidate byte excludes the candidate suffix
    /// from being better than the current maximal (or minimal) suffix. That
    /// is, the current candidate suffix should be dropped and the next one
    /// should be considered.
    Skip,
    /// This occurs when no decision to accept or skip the candidate suffix
    /// can be made, e.g., when corresponding bytes are equivalent. In this
    /// case, the next corresponding bytes should be compared.
    Push,
}

impl SuffixKind {
    /// Returns true if and only if the given candidate byte indicates that
    /// it should replace the current suffix as the maximal (or minimal)
    /// suffix.
    fn cmp(self, current: u8, candidate: u8) -> SuffixOrdering {
        use self::SuffixOrdering::*;

        match self {
            SuffixKind::Minimal if candidate < current => Accept,
            SuffixKind::Minimal if candidate > current => Skip,
            SuffixKind::Minimal => Push,
            SuffixKind::Maximal if candidate > current => Accept,
            SuffixKind::Maximal if candidate < current => Skip,
            SuffixKind::Maximal => Push,
        }
    }
}

/// A bitset used to track whether a particular byte exists in a needle or not.
///
/// Namely, bit 'i' is set if and only if byte%64==i for any byte in the
/// needle. If a particular byte in the haystack is NOT in this set, then one
/// can conclude that it is also not in the needle, and thus, one can advance
/// in the haystack by needle.len() bytes.
#[derive(Clone, Copy, Debug)]
struct ApproximateByteSet(u64);

impl ApproximateByteSet {
    /// Create a new set from the given needle.
    fn new(needle: &[u8]) -> ApproximateByteSet {
        let mut bits = 0;
        for &b in needle {
            bits |= 1 << (b % 64);
        }
        ApproximateByteSet(bits)
    }

    /// Return true if and only if the given byte might be in this set. This
    /// may return a false positive, but will never return a false negative.
    fn contains(&self, byte: u8) -> bool {
        self.0 & (1 << (byte % 64)) != 0
    }
}

/// Return true if and only if a prefilter should be used given the current
/// prefilter state, haystack and needle.
fn should_prefilter(
    prestate: &mut PrefilterState,
    haystack: &[u8],
    needle: &[u8],
) -> bool {
    if !prestate.is_effective() {
        return false;
    }
    haystack.len() > min_prefilter_len(haystack, needle)
}

/// Return the minimum length of the haystack in which a prefilter should be
/// used. If the haystack is below this length, then it's probably not worth
/// the overhead of running the prefilter.
fn min_prefilter_len(_haystack: &[u8], needle: &[u8]) -> usize {
    /// If the haystack length isn't greater than needle.len() * FACTOR, then
    /// no prefilter will be used. The presumption here is that since there
    /// are so few bytes to check, it's not worth running the prefilter since
    /// there will need to be a validation step anyway. Thus, the prefilter is
    /// largely redundant work.
    ///
    /// Increase the factor noticeably hurts the
    /// memmem/krate/prebuilt/teeny-*/never-john-watson benchmarks.
    const PREFILTER_LENGTH_FACTOR: usize = 2;
    const VECTOR_MIN_LENGTH: usize = 16;
    core::cmp::max(VECTOR_MIN_LENGTH, PREFILTER_LENGTH_FACTOR * needle.len())
}

#[cfg(all(test, feature = "std", not(miri)))]
mod tests {
    use quickcheck::quickcheck;

    use super::*;

    mod props_nopre {
        define_memmem_quickcheck_tests!(
            super::simpletests::twoway_find_nopre,
            super::simpletests::twoway_rfind
        );
    }
    // Just run the reverse tests again, since reverse doesn't do prefilters.
    mod props_pre {
        define_memmem_quickcheck_tests!(
            super::simpletests::twoway_find_pre,
            super::simpletests::twoway_rfind
        );
    }

    /// Convenience wrapper for computing the suffix as a byte string.
    fn get_suffix_forward(needle: &[u8], kind: SuffixKind) -> (&[u8], usize) {
        let s = Suffix::forward(needle, kind);
        (&needle[s.pos..], s.period)
    }

    /// Convenience wrapper for computing the reverse suffix as a byte string.
    fn get_suffix_reverse(needle: &[u8], kind: SuffixKind) -> (&[u8], usize) {
        let s = Suffix::reverse(needle, kind);
        (&needle[..s.pos], s.period)
    }

    /// Return all of the non-empty suffixes in the given byte string.
    fn suffixes(bytes: &[u8]) -> Vec<&[u8]> {
        (0..bytes.len()).map(|i| &bytes[i..]).collect()
    }

    /// Return the lexicographically maximal suffix of the given byte string.
    fn naive_maximal_suffix_forward(needle: &[u8]) -> &[u8] {
        let mut sufs = suffixes(needle);
        sufs.sort();
        sufs.pop().unwrap()
    }

    /// Return the lexicographically maximal suffix of the reverse of the given
    /// byte string.
    fn naive_maximal_suffix_reverse(needle: &[u8]) -> Vec<u8> {
        let mut reversed = needle.to_vec();
        reversed.reverse();
        let mut got = naive_maximal_suffix_forward(&reversed).to_vec();
        got.reverse();
        got
    }

    #[test]
    fn suffix_forward() {
        macro_rules! assert_suffix_min {
            ($given:expr, $expected:expr, $period:expr) => {
                let (got_suffix, got_period) =
                    get_suffix_forward($given.as_bytes(), SuffixKind::Minimal);
                let got_suffix = std::str::from_utf8(got_suffix).unwrap();
                assert_eq!(($expected, $period), (got_suffix, got_period));
            };
        }

        macro_rules! assert_suffix_max {
            ($given:expr, $expected:expr, $period:expr) => {
                let (got_suffix, got_period) =
                    get_suffix_forward($given.as_bytes(), SuffixKind::Maximal);
                let got_suffix = std::str::from_utf8(got_suffix).unwrap();
                assert_eq!(($expected, $period), (got_suffix, got_period));
            };
        }

        assert_suffix_min!("a", "a", 1);
        assert_suffix_max!("a", "a", 1);

        assert_suffix_min!("ab", "ab", 2);
        assert_suffix_max!("ab", "b", 1);

        assert_suffix_min!("ba", "a", 1);
        assert_suffix_max!("ba", "ba", 2);

        assert_suffix_min!("abc", "abc", 3);
        assert_suffix_max!("abc", "c", 1);

        assert_suffix_min!("acb", "acb", 3);
        assert_suffix_max!("acb", "cb", 2);

        assert_suffix_min!("cba", "a", 1);
        assert_suffix_max!("cba", "cba", 3);

        assert_suffix_min!("abcabc", "abcabc", 3);
        assert_suffix_max!("abcabc", "cabc", 3);

        assert_suffix_min!("abcabcabc", "abcabcabc", 3);
        assert_suffix_max!("abcabcabc", "cabcabc", 3);

        assert_suffix_min!("abczz", "abczz", 5);
        assert_suffix_max!("abczz", "zz", 1);

        assert_suffix_min!("zzabc", "abc", 3);
        assert_suffix_max!("zzabc", "zzabc", 5);

        assert_suffix_min!("aaa", "aaa", 1);
        assert_suffix_max!("aaa", "aaa", 1);

        assert_suffix_min!("foobar", "ar", 2);
        assert_suffix_max!("foobar", "r", 1);
    }

    #[test]
    fn suffix_reverse() {
        macro_rules! assert_suffix_min {
            ($given:expr, $expected:expr, $period:expr) => {
                let (got_suffix, got_period) =
                    get_suffix_reverse($given.as_bytes(), SuffixKind::Minimal);
                let got_suffix = std::str::from_utf8(got_suffix).unwrap();
                assert_eq!(($expected, $period), (got_suffix, got_period));
            };
        }

        macro_rules! assert_suffix_max {
            ($given:expr, $expected:expr, $period:expr) => {
                let (got_suffix, got_period) =
                    get_suffix_reverse($given.as_bytes(), SuffixKind::Maximal);
                let got_suffix = std::str::from_utf8(got_suffix).unwrap();
                assert_eq!(($expected, $period), (got_suffix, got_period));
            };
        }

        assert_suffix_min!("a", "a", 1);
        assert_suffix_max!("a", "a", 1);

        assert_suffix_min!("ab", "a", 1);
        assert_suffix_max!("ab", "ab", 2);

        assert_suffix_min!("ba", "ba", 2);
        assert_suffix_max!("ba", "b", 1);

        assert_suffix_min!("abc", "a", 1);
        assert_suffix_max!("abc", "abc", 3);

        assert_suffix_min!("acb", "a", 1);
        assert_suffix_max!("acb", "ac", 2);

        assert_suffix_min!("cba", "cba", 3);
        assert_suffix_max!("cba", "c", 1);

        assert_suffix_min!("abcabc", "abca", 3);
        assert_suffix_max!("abcabc", "abcabc", 3);

        assert_suffix_min!("abcabcabc", "abcabca", 3);
        assert_suffix_max!("abcabcabc", "abcabcabc", 3);

        assert_suffix_min!("abczz", "a", 1);
        assert_suffix_max!("abczz", "abczz", 5);

        assert_suffix_min!("zzabc", "zza", 3);
        assert_suffix_max!("zzabc", "zz", 1);

        assert_suffix_min!("aaa", "aaa", 1);
        assert_suffix_max!("aaa", "aaa", 1);
    }

    quickcheck! {
        fn qc_suffix_forward_maximal(bytes: Vec<u8>) -> bool {
            if bytes.is_empty() {
                return true;
            }

            let (got, _) = get_suffix_forward(&bytes, SuffixKind::Maximal);
            let expected = naive_maximal_suffix_forward(&bytes);
            got == expected
        }

        fn qc_suffix_reverse_maximal(bytes: Vec<u8>) -> bool {
            if bytes.is_empty() {
                return true;
            }

            let (got, _) = get_suffix_reverse(&bytes, SuffixKind::Maximal);
            let expected = naive_maximal_suffix_reverse(&bytes);
            expected == got
        }
    }
}

#[cfg(test)]
mod simpletests {
    use super::*;

    pub(crate) fn twoway_find_nopre(
        haystack: &[u8],
        needle: &[u8],
    ) -> Option<usize> {
        let config = Config { prefilter: Prefilter::None };
        Forward::new(config, needle).find(haystack)
    }

    pub(crate) fn twoway_find_pre(
        haystack: &[u8],
        needle: &[u8],
    ) -> Option<usize> {
        let config = Config { prefilter: Prefilter::Auto };
        Forward::new(config, needle).find(haystack)
    }

    pub(crate) fn twoway_rfind(
        haystack: &[u8],
        needle: &[u8],
    ) -> Option<usize> {
        Reverse::new(needle).rfind(haystack)
    }

    mod simple_nopre {
        define_memmem_simple_tests!(
            super::twoway_find_nopre,
            super::twoway_rfind
        );
    }
    // Just run the reverse tests again, since reverse doesn't do prefilters.
    mod simple_pre {
        define_memmem_simple_tests!(
            super::twoway_find_pre,
            super::twoway_rfind
        );
    }
}
