// Content hashing using SHA-256 for symbol dedup across workspaces.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Compute SHA-256 hash of a byte slice, returning lowercase hex string.
pub fn hash_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// Compute SHA-256 hash of a file's contents.
pub fn hash_file(path: &Path) -> Result<String> {
    let data = std::fs::read(path)?;
    Ok(hash_bytes(&data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_hash_bytes_returns_hex_string() {
        let hash = hash_bytes(b"hello world");
        // SHA-256 of "hello world" is a well-known value
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_hash_bytes_empty_input() {
        let hash = hash_bytes(b"");
        // SHA-256 of empty string
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_hash_bytes_different_inputs_produce_different_hashes() {
        let hash1 = hash_bytes(b"file content A");
        let hash2 = hash_bytes(b"file content B");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_bytes_same_input_produces_same_hash() {
        let hash1 = hash_bytes(b"identical content");
        let hash2 = hash_bytes(b"identical content");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_file_returns_hash_of_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"fn main() {}").unwrap();

        let hash = hash_file(&path).unwrap();
        let expected = hash_bytes(b"fn main() {}");
        assert_eq!(hash, expected);
    }

    #[test]
    fn test_hash_file_nonexistent_returns_error() {
        let result = hash_file(std::path::Path::new("/nonexistent/file.rs"));
        assert!(result.is_err());
    }

    #[test]
    fn test_hash_is_64_hex_chars() {
        let hash = hash_bytes(b"test");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
