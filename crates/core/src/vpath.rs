//! Virtual paths inside an archive: `/Projects/2024/raw_reads/a.fastq`.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

/// Directory at the archive root reserved for pack metadata (manifests, solid blocks).
pub const RESERVED_ROOT_NAME: &str = ".archive";

/// A normalized path within an archive. The root has no components.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct VPath(Vec<String>);

impl VPath {
    pub fn root() -> Self {
        VPath(Vec::new())
    }

    /// Parse `/a/b`, `a/b`, or `a/b/`. Rejects `.`, `..`, and NUL.
    pub fn parse(s: &str) -> Result<Self> {
        let mut parts = Vec::new();
        for c in s.split('/') {
            if c.is_empty() {
                continue;
            }
            validate_name(c)?;
            parts.push(c.to_string());
        }
        let p = VPath(parts);
        p.check_reserved()?;
        Ok(p)
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn components(&self) -> &[String] {
        &self.0
    }

    pub fn name(&self) -> Option<&str> {
        self.0.last().map(|s| s.as_str())
    }

    pub fn parent(&self) -> Option<VPath> {
        if self.0.is_empty() { None } else { Some(VPath(self.0[..self.0.len() - 1].to_vec())) }
    }

    pub fn join(&self, name: &str) -> Result<VPath> {
        validate_name(name)?;
        let mut v = self.0.clone();
        v.push(name.to_string());
        let p = VPath(v);
        p.check_reserved()?;
        Ok(p)
    }

    /// Join a relative path such as `sub/dir/file`.
    pub fn join_rel(&self, rel: &str) -> Result<VPath> {
        let mut p = self.clone();
        for c in rel.split('/').filter(|c| !c.is_empty()) {
            p = p.join(c)?;
        }
        Ok(p)
    }

    pub fn starts_with(&self, other: &VPath) -> bool {
        self.0.len() >= other.0.len() && self.0[..other.0.len()] == other.0[..]
    }

    /// Path relative to the root without a leading slash, as stored in packs.
    pub fn to_rel_string(&self) -> String {
        self.0.join("/")
    }

    fn check_reserved(&self) -> Result<()> {
        if self.0.first().map(|s| s.as_str()) == Some(RESERVED_ROOT_NAME) {
            return Err(Error::InvalidPath(format!("\"{RESERVED_ROOT_NAME}\" is reserved at the top of the archive")));
        }
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(Error::InvalidPath(format!("invalid name {name:?}")));
    }
    if name.len() > 255 {
        return Err(Error::InvalidPath(format!("name longer than 255 bytes: {name:?}")));
    }
    Ok(())
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.0.join("/"))
    }
}

impl fmt::Debug for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VPath({self})")
    }
}

impl Serialize for VPath {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for VPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        VPath::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display() {
        let p = VPath::parse("/a//b/c/").unwrap();
        assert_eq!(p.to_string(), "/a/b/c");
        assert_eq!(p.to_rel_string(), "a/b/c");
        assert_eq!(p.parent().unwrap().to_string(), "/a/b");
        assert_eq!(VPath::parse("/").unwrap(), VPath::root());
        assert!(VPath::parse("a/../b").is_err());
        assert!(VPath::parse("/.archive/x").is_err());
        assert!(VPath::parse("/x/.archive").is_ok());
        assert!(VPath::parse("a/b").unwrap().starts_with(&VPath::parse("a").unwrap()));
    }
}
