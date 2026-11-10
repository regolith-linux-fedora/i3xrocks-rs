//! Insertion-ordered associative array of byte strings.
//!
//! Keys keep the position of their first insertion; setting an existing
//! key replaces its value in place. Keys and values behave like C strings:
//! anything after an embedded NUL byte is dropped.

#[derive(Clone, Debug, Default)]
pub struct Map {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// The C-string view of a byte slice (up to the first NUL byte).
pub fn cstr(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(pos) => &bytes[..pos],
        None => bytes,
    }
}

impl Map {
    pub fn new() -> Map {
        Map::default()
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let key = cstr(key);
        self.pairs
            .iter()
            .find(|(k, _)| k.as_slice() == key)
            .map(|(_, v)| v.as_slice())
    }

    pub fn set(&mut self, key: &[u8], value: &[u8]) {
        let key = cstr(key);
        let value = cstr(value).to_vec();
        match self.pairs.iter_mut().find(|(k, _)| k.as_slice() == key) {
            Some(pair) => pair.1 = value,
            None => self.pairs.push((key.to_vec(), value)),
        }
    }

    pub fn clear(&mut self) {
        self.pairs.clear();
    }

    /// Set every pair of `base` into this map, in order.
    pub fn copy_from(&mut self, base: &Map) {
        for (key, value) in &base.pairs {
            self.set(key, value);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.pairs.iter().map(|(k, v)| (k.as_slice(), v.as_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order_and_replaces_in_place() {
        let mut map = Map::new();
        map.set(b"a", b"1");
        map.set(b"b", b"2");
        map.set(b"a", b"3");
        let pairs: Vec<_> = map.iter().collect();
        assert_eq!(pairs, vec![(&b"a"[..], &b"3"[..]), (&b"b"[..], &b"2"[..])]);
    }

    #[test]
    fn truncates_at_nul() {
        let mut map = Map::new();
        map.set(b"k\0x", b"v\0w");
        assert_eq!(map.get(b"k"), Some(&b"v"[..]));
    }
}
