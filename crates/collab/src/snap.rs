//! The shared form of a sheet's parts, and the two small algorithms the shim needs: aligning the rows (or columns) a person
//! changed with the ones the room holds, and hashing a line so an unchanged one is recognised after it moved.

use std::collections::BTreeMap;

use gridcraft_model::{Cell, LineInfo};
use serde_json::{Value as Json, json};

/// One cell as the room holds it: the constant value (`v`, a `gridcraft_core::Value` as JSON), the formula text (`f`, without `=`)
/// and the style (`s`, the style's content key). A formula cell holds no value: every window recalculates.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CellRec {
    pub v: Option<Json>,
    pub f: Option<String>,
    pub s: Option<String>,
}

impl CellRec {
    /// The fields that differ from `old`: `(key, value)` to set, `(key, None)` to remove.
    #[must_use]
    pub fn changes_from(&self, old: Option<&CellRec>) -> Vec<(&'static str, Option<Json>)> {
        let empty = CellRec::default();
        let old = old.unwrap_or(&empty);
        let mut out = Vec::new();
        if self.v != old.v {
            out.push(("v", self.v.clone()));
        }
        if self.f != old.f {
            out.push(("f", self.f.clone().map(Json::from)));
        }
        if self.s != old.s {
            out.push(("s", self.s.clone().map(Json::from)));
        }
        out
    }

    /// The cell as a JSON object (for the digest and the tests).
    #[must_use]
    pub fn json(&self) -> Json {
        json!({"v": self.v, "f": self.f, "s": self.s})
    }
}

/// A row's or column's own properties the room shares: its size, whether it is hidden, and whether the person set the size.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Prop {
    pub size: Option<f32>,
    pub hidden: bool,
    pub custom: bool,
}

impl Prop {
    /// The shared properties of a line, `None` when it states none.
    #[must_use]
    pub fn of(l: &LineInfo) -> Option<Prop> {
        let p = Prop { size: l.size, hidden: l.hidden, custom: l.custom };
        (p != Prop::default()).then_some(p)
    }

    #[must_use]
    pub fn json(&self) -> Json {
        let mut o = serde_json::Map::new();
        if let Some(s) = self.size {
            o.insert("h".into(), json!(f64::from(s)));
        }
        if self.hidden {
            o.insert("x".into(), json!(true));
        }
        if self.custom {
            o.insert("c".into(), json!(true));
        }
        Json::Object(o)
    }

    #[must_use]
    pub fn from_json(v: &Json) -> Option<Prop> {
        let o = v.as_object()?;
        let p = Prop {
            size: o.get("h").and_then(Json::as_f64).map(|f| f as f32),
            hidden: o.get("x").and_then(Json::as_bool) == Some(true),
            custom: o.get("c").and_then(Json::as_bool) == Some(true),
        };
        (p != Prop::default()).then_some(p)
    }
}

/// FNV-1a over a stream of bytes, for line signatures.
#[derive(Clone, Copy)]
pub struct Hasher(u64);

impl Default for Hasher {
    fn default() -> Self {
        Hasher(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher {
    pub fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.0 ^= u64::from(*x);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    pub fn num(&mut self, n: u64) {
        self.bytes(&n.to_le_bytes());
    }
    pub fn finish(self) -> u64 {
        self.0
    }
}

/// What a cell contributes to the signature of its line: its place along the other axis, its shared fields and its style.
pub fn hash_cell(h: &mut Hasher, along: u64, cell: &Cell) {
    h.num(along);
    match &cell.formula {
        Some(f) => {
            h.bytes(b"f");
            h.bytes(f.text.as_bytes());
        }
        None => {
            h.bytes(b"v");
            h.bytes(serde_json::to_string(&cell.value).unwrap_or_default().as_bytes());
        }
    }
    h.num(u64::from(cell.style.0));
}

/// A string's hash, for the places that hold ids.
#[must_use]
pub fn hash_str(s: &str) -> u64 {
    let mut h = Hasher::default();
    h.bytes(s.as_bytes());
    h.finish()
}

/// How the lines (rows or columns) of a sheet now relate to the ids the room holds for them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// For each new line, the old line it is (`Some`: it keeps the old line's id; an edit in place is still the same line).
    pub from_old: Vec<Option<usize>>,
    /// The old lines that are gone, ascending.
    pub deleted: Vec<usize>,
}

impl Plan {
    /// Nothing moved: every line is the line at its own place.
    #[must_use]
    pub fn identity(n: usize) -> Plan {
        Plan { from_old: (0..n).map(Some).collect(), deleted: Vec::new() }
    }

    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.deleted.is_empty() && self.from_old.iter().enumerate().all(|(i, o)| *o == Some(i))
    }

    /// A new line sits among old ones (not only after the last).
    #[must_use]
    pub fn inserted_inside(&self) -> bool {
        let last_old = self.from_old.iter().rposition(Option::is_some);
        self.from_old.iter().enumerate().any(|(i, o)| o.is_none() && last_old.is_some_and(|l| i < l))
    }

    /// No old line moved or went: the new lines are the old ones in place, with new ones only after them (an append).
    #[must_use]
    pub fn is_stable(&self, old_len: usize) -> bool {
        self.deleted.is_empty() && self.from_old.iter().enumerate().all(|(i, o)| match o {
            Some(j) => *j == i,
            None => i >= old_len,
        })
    }
}

/// The largest edit distance the aligner searches; a bigger change is paired by position (every line below is an edit in place).
const MAX_EDIT: usize = 1_500;

/// Which of `old` and `new` are the same line: lines equal in hash at both ends are matched, the middle by a shortest edit
/// script, and what is left between two matched lines is paired by position (an edited line keeps its id) with the surplus
/// inserted or deleted. Ids of an edited line therefore survive typing, and the ids of the lines around an insert survive too.
#[must_use]
pub fn align(old: &[u64], new: &[u64]) -> Plan {
    let mut pre = 0;
    while pre < old.len() && pre < new.len() && old[pre] == new[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < old.len() - pre && suf < new.len() - pre && old[old.len() - 1 - suf] == new[new.len() - 1 - suf] {
        suf += 1;
    }
    let (om, nm) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
    let mut from_old: Vec<Option<usize>> = vec![None; new.len()];
    let mut deleted = Vec::new();
    for (i, slot) in from_old.iter_mut().enumerate().take(pre) {
        *slot = Some(i);
    }
    for k in 0..suf {
        from_old[new.len() - 1 - k] = Some(old.len() - 1 - k);
    }
    // Anchors in the middle: (old index, new index) pairs of equal lines, increasing in both.
    let anchors = lcs(om, nm).unwrap_or_default();
    let mut o = 0;
    let mut n = 0;
    let pair_gap = |o0: usize, o1: usize, n0: usize, n1: usize, from_old: &mut Vec<Option<usize>>, deleted: &mut Vec<usize>| {
        let (ol, nl) = (o1 - o0, n1 - n0);
        for k in 0..ol.min(nl) {
            from_old[pre + n0 + k] = Some(pre + o0 + k);
        }
        for k in ol.min(nl)..ol {
            deleted.push(pre + o0 + k);
        }
    };
    for (ao, an) in anchors {
        pair_gap(o, ao, n, an, &mut from_old, &mut deleted);
        from_old[pre + an] = Some(pre + ao);
        o = ao + 1;
        n = an + 1;
    }
    pair_gap(o, om.len(), n, nm.len(), &mut from_old, &mut deleted);
    deleted.sort_unstable();
    Plan { from_old, deleted }
}

/// A longest common subsequence of equal lines, by Myers' shortest edit script; `None` when the edit is bigger than [`MAX_EDIT`].
fn lcs(a: &[u64], b: &[u64]) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (a.len() as i32, b.len() as i32);
    let max = ((n + m) as usize).min(MAX_EDIT) as i32;
    let offset = max + 1;
    let mut v = vec![0i32; (2 * offset + 2) as usize];
    // trace[d] holds v[-d..=d] as it stood before round d.
    let mut trace: Vec<Vec<i32>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max {
        trace.push(v[(offset - d) as usize..=(offset + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let d_end = found?;
    let mut out = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..=d_end).rev() {
        let before = &trace[d as usize];
        let at = |k: i32| before[(k + d) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = if d == 0 { 0 } else { at(prev_k) };
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            out.push((x as usize, y as usize));
        }
        if d > 0 {
            x = prev_x;
            y = prev_y;
        }
    }
    out.reverse();
    Some(out)
}

/// A sorted map's entries as JSON (for the digest).
pub fn map_json<K: std::fmt::Display, V>(m: &BTreeMap<K, V>, f: impl Fn(&V) -> Json) -> Json {
    Json::Object(m.iter().map(|(k, v)| (k.to_string(), f(v))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(old: &[u64], new: &[u64]) -> Plan {
        align(old, new)
    }

    #[test]
    fn an_edit_in_place_keeps_every_line() {
        let p = plan(&[1, 2, 3, 4], &[1, 9, 3, 4]);
        assert_eq!(p.from_old, vec![Some(0), Some(1), Some(2), Some(3)]);
        assert!(p.deleted.is_empty());
        assert!(p.is_identity());
    }

    #[test]
    fn an_insert_keeps_the_lines_around_it() {
        let p = plan(&[1, 2, 3], &[1, 7, 2, 3]);
        assert_eq!(p.from_old, vec![Some(0), None, Some(1), Some(2)]);
        assert!(p.deleted.is_empty());
    }

    #[test]
    fn a_delete_names_the_line_that_went() {
        let p = plan(&[1, 2, 3, 4], &[1, 3, 4]);
        assert_eq!(p.from_old, vec![Some(0), Some(1 + 1), Some(3)]);
        assert_eq!(p.deleted, vec![1]);
    }

    #[test]
    fn an_insert_and_an_edit_at_once() {
        // line 2 edited (2 -> 9) and a new line after line 3.
        let p = plan(&[1, 2, 3, 4], &[1, 9, 3, 8, 4]);
        assert_eq!(p.from_old, vec![Some(0), Some(1), Some(2), None, Some(3)]);
        assert!(p.deleted.is_empty());
    }

    #[test]
    fn a_delete_at_the_top_with_a_blank_added_at_the_end() {
        let p = plan(&[10, 20, 30], &[20, 30, 0]);
        assert_eq!(p.deleted, vec![0]);
        assert_eq!(p.from_old, vec![Some(1), Some(2), None]);
    }

    #[test]
    fn lcs_finds_the_common_lines() {
        let m = lcs(&[1, 2, 3, 4, 5], &[2, 4, 6]).unwrap();
        assert_eq!(m, vec![(1, 0), (3, 1)]);
    }
}
