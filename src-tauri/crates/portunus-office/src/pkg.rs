//! Zip container access for office packages (OOXML and ODF are both zips):
//! opening, part listing, size-capped part reads, and the per-document
//! inflated-bytes budget.

use std::io::Read;
use zip::ZipArchive;

// Guards against zip bombs: reject inflated entries above these limits.
pub const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Marker error for a budget stop. Callers that prefer to degrade rather than
/// fail (the pptx slide loop) compare against this to tell a budget stop from a
/// real read error.
pub const BUDGET_EXCEEDED: &str = "office: package inflated-size budget exceeded";

/// Cumulative inflated-bytes budget for one document. MAX_ENTRY_BYTES caps a
/// single part; this caps the whole package, so a zip bomb cannot be spread
/// across many individually-legal entries.
pub struct Budget {
    spent: u64,
    cap: u64,
}

impl Budget {
    pub fn new() -> Self {
        Budget {
            spent: 0,
            cap: MAX_TOTAL_BYTES,
        }
    }

    pub fn take(&mut self, n: u64) -> Result<(), String> {
        let next = self.spent.saturating_add(n);
        if next > self.cap {
            return Err(BUDGET_EXCEEDED.to_string());
        }
        self.spent = next;
        Ok(())
    }

    #[allow(dead_code)] // Used by the later-stage renderers; covered by tests.
    pub fn spent(&self) -> u64 {
        self.spent
    }
}

pub type Zip = ZipArchive<std::io::BufReader<std::fs::File>>;

pub fn open_zip(path: &str) -> Result<Zip, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| e.to_string())
}

pub fn read_entry(zip: &mut Zip, name: &str, budget: &mut Budget) -> Result<Option<String>, String> {
    let mut entry = match zip.by_name(name) {
        Ok(e) => e,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if entry.size() > MAX_ENTRY_BYTES {
        return Err(format!("entry {} too large: {} bytes", name, entry.size()));
    }
    // Charge the declared inflated size before inflating anything.
    budget.take(entry.size())?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    Ok(Some(buf))
}

/// Byte-oriented sibling of `read_entry`, for binary parts (embedded media).
#[allow(dead_code)] // Used by the later-stage renderers.
pub fn read_entry_bytes(
    zip: &mut Zip,
    name: &str,
    budget: &mut Budget,
) -> Result<Option<Vec<u8>>, String> {
    let mut entry = match zip.by_name(name) {
        Ok(e) => e,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if entry.size() > MAX_ENTRY_BYTES {
        return Err(format!("entry {} too large: {} bytes", name, entry.size()));
    }
    budget.take(entry.size())?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(Some(buf))
}

/// Names of the parts matching `pred`, in archive order. Collected up front so
/// the archive isn't borrowed while the parts are being read.
pub fn list_parts(zip: &mut Zip, pred: impl Fn(&str) -> bool) -> Vec<String> {
    let len = zip.len();
    (0..len)
        .filter_map(|i| {
            let f = zip.by_index(i).ok()?;
            let name = f.name();
            if pred(name) {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// A package fixture on disk, deleted when it drops.
///
/// Test-only, and shared by every renderer's tests: a [`Zip`] is
/// `ZipArchive<BufReader<File>>`, so no fixture package can live in memory, and
/// a `Ctx` that carries the media path needs a real archive behind it even when
/// the fixture references no media at all.
#[cfg(test)]
pub struct TestPkg(std::path::PathBuf);

#[cfg(test)]
impl TestPkg {
    /// A repeated entry name means "replace": `zip` itself rejects a duplicate,
    /// so the last write of a name wins and document order is preserved.
    pub fn new(tag: &str, entries: &[(&str, Vec<u8>)]) -> TestPkg {
        use std::io::Write;
        let mut seen = std::collections::HashSet::new();
        let mut parts: Vec<&(&str, Vec<u8>)> = entries
            .iter()
            .rev()
            .filter(|(name, _)| seen.insert(*name))
            .collect();
        parts.reverse();

        // Tests run in parallel in one process, so the pid alone is not unique.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "portunus-office-{tag}-{}-{n}.zip",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in parts {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
        TestPkg(path)
    }

    pub fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }

    pub fn open(&self) -> Zip {
        open_zip(self.path()).expect("fixture package opens")
    }
}

#[cfg(test)]
impl Drop for TestPkg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_allows_up_to_the_cap_then_refuses() {
        let mut b = Budget::new();
        assert!(b.take(MAX_TOTAL_BYTES - 10).is_ok());
        assert_eq!(b.spent(), MAX_TOTAL_BYTES - 10);
        assert!(b.take(10).is_ok());
        assert_eq!(b.spent(), MAX_TOTAL_BYTES);
        let err = b.take(1).expect_err("past the cap must fail");
        assert_eq!(err, BUDGET_EXCEEDED);
        // A refused take does not consume budget.
        assert_eq!(b.spent(), MAX_TOTAL_BYTES);
    }

    #[test]
    fn budget_saturates_instead_of_overflowing() {
        let mut b = Budget::new();
        assert!(b.take(u64::MAX).is_err());
        assert_eq!(b.spent(), 0);
    }
}
